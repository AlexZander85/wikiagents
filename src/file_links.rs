//! Where a file mentioned in a transcript actually is.
//!
//! The UI used to build a GitHub URL for every path it saw, so a file that only
//! ever lived on this machine got a link that could only 404. This answers from
//! the facts instead: the tree of `origin` as of the last fetch, and the disk.
//!
//! Every path here comes from a transcript, which is untrusted text. A network
//! path is therefore never touched: just checking that `\\host\share\x` exists
//! makes Windows connect to that host and offer it the user's credentials.

use crate::web::{detect_git_remote_and_branch, find_git_root, git_cmd};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Longer than any real path; a longer string is not worth a filesystem call.
const MAX_PATH_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// In `origin`'s tree: a link to the host will work.
    Remote,
    /// On this machine only.
    Local,
    /// The project has a remote but no tracking refs to check against, and the
    /// file is not on disk either, so a link may or may not work.
    Unverified,
    Missing,
}

#[derive(Debug, Serialize)]
pub struct FileLink {
    pub path: String,
    pub state: LinkState,
    /// Path inside the repository, for building the host's URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    /// The branch `repo_path` was found on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Where the file is on this machine, if it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
}

struct RemoteTree {
    branch: String,
    files: HashSet<String>,
}

/// One session's view of the disk and its repository, loaded once per request.
pub struct FileLocator {
    project: Option<PathBuf>,
    root: Option<PathBuf>,
    has_remote: bool,
    head_branch: Option<String>,
    trees: Vec<RemoteTree>,
}

impl FileLocator {
    pub fn for_project(project: &str) -> Self {
        let project = Path::new(project.trim().trim_matches(['"', '\'']).trim());
        // `web` and other labels are not directories: joining onto one would
        // resolve against wherever the server happens to be running.
        let project = (project.is_absolute() && !is_network_path(&project.to_string_lossy()))
            .then(|| project.to_path_buf());
        let root = project.as_deref().and_then(find_git_root);
        let (remote, head_branch) = project
            .as_deref()
            .map_or((None, None), detect_git_remote_and_branch);
        let has_remote = remote.is_some();
        let trees = match &root {
            Some(root) if has_remote => load_remote_trees(root, head_branch.as_deref()),
            _ => Vec::new(),
        };
        Self {
            project,
            root,
            has_remote,
            head_branch,
            trees,
        }
    }

    /// The places a mentioned path can mean. A relative path is relative to the
    /// session's folder, or to the repository root when the session started in a
    /// subfolder but the assistant wrote the path from the top.
    fn candidates(&self, raw: &str) -> Vec<PathBuf> {
        let raw = raw.trim();
        if raw.is_empty()
            || raw.chars().count() > MAX_PATH_CHARS
            || raw.contains(['\0', '\n', '\r'])
            || is_network_path(raw)
        {
            return Vec::new();
        }
        if let Some(path) = absolute_path(raw) {
            return vec![path];
        }
        let as_path = Path::new(raw);
        if as_path.has_root()
            || as_path
                .components()
                .any(|c| matches!(c, Component::Prefix(_)))
        {
            return Vec::new();
        }
        let mut found: Vec<PathBuf> = Vec::new();
        for base in [self.project.as_deref(), self.root.as_deref()]
            .into_iter()
            .flatten()
        {
            let joined = base.join(as_path);
            if !found.contains(&joined) {
                found.push(joined);
            }
        }
        found
    }

    pub fn locate(&self, raw: &str) -> FileLink {
        let candidates = self.candidates(raw);
        let local_path = candidates.iter().find_map(|c| existing_local_path(c));
        let local_text = local_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned());
        let in_repo: Vec<String> = match &self.root {
            Some(root) => candidates
                .iter()
                .filter_map(|c| relative_to_root(root, c))
                .collect(),
            None => Vec::new(),
        };

        for repo_path in &in_repo {
            if let Some(tree) = self.trees.iter().find(|t| t.files.contains(repo_path)) {
                return FileLink {
                    path: raw.to_string(),
                    state: LinkState::Remote,
                    repo_path: Some(repo_path.clone()),
                    branch: Some(tree.branch.clone()),
                    local_path: local_text,
                };
            }
        }
        if local_text.is_some() {
            return FileLink {
                path: raw.to_string(),
                state: LinkState::Local,
                repo_path: None,
                branch: None,
                local_path: local_text,
            };
        }
        if self.has_remote && self.trees.is_empty() {
            if let Some(repo_path) = in_repo.into_iter().next() {
                return FileLink {
                    path: raw.to_string(),
                    state: LinkState::Unverified,
                    repo_path: Some(repo_path),
                    branch: self.head_branch.clone(),
                    local_path: None,
                };
            }
        }
        FileLink {
            path: raw.to_string(),
            state: LinkState::Missing,
            repo_path: None,
            branch: None,
            local_path: None,
        }
    }

    /// The file this path means on this machine, if it is still there.
    pub fn existing(&self, raw: &str) -> Option<PathBuf> {
        self.candidates(raw)
            .iter()
            .find_map(|c| existing_local_path(c))
    }
}

/// The checked-out branch first, then the default one: a file merged long ago is
/// on GitHub even while a feature branch that was never pushed is checked out.
fn load_remote_trees(root: &Path, head_branch: Option<&str>) -> Vec<RemoteTree> {
    let default_branch = git_cmd(root)
        .args(["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .and_then(|name| name.strip_prefix("origin/").map(str::to_string));

    let mut names: Vec<String> = head_branch.map(str::to_string).into_iter().collect();
    if let Some(default_branch) = default_branch {
        if !names.contains(&default_branch) {
            names.push(default_branch);
        }
    }
    let trees: Vec<RemoteTree> = names
        .into_iter()
        .filter_map(|branch| load_remote_tree(root, branch))
        .collect();
    if !trees.is_empty() {
        return trees;
    }
    // Every git process adds to the time before links appear, so the usual names
    // are only guessed when nothing better led to a tree: an unpushed branch in
    // a repository whose `origin/HEAD` was never set.
    ["main", "master"]
        .into_iter()
        .filter_map(|branch| load_remote_tree(root, branch.to_string()))
        .collect()
}

fn load_remote_tree(root: &Path, branch: String) -> Option<RemoteTree> {
    let out = git_cmd(root)
        .args(["ls-tree", "-r", "-z", "--name-only"])
        .arg(format!("refs/remotes/origin/{branch}"))
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let files = out
        .stdout
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    Some(RemoteTree { branch, files })
}

fn is_network_path(raw: &str) -> bool {
    raw.starts_with("\\\\")
        || raw.starts_with("//")
        || raw.starts_with("\\/")
        || raw.starts_with("/\\")
}

/// `D:\x`, `~/x`, and on Windows Git Bash's `/d/x`. A bare `/x` on Windows means
/// nothing here (`/api/session` is far likelier a URL than a file on the current
/// drive), so it stays unresolved.
fn absolute_path(raw: &str) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        return crate::util::home_dir().map(|home| home.join(rest));
    }
    if cfg!(windows) {
        if let Some((drive, rest)) = raw
            .strip_prefix('/')
            .and_then(|tail| tail.split_once('/'))
            .filter(|(drive, _)| drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic())
        {
            return Some(PathBuf::from(format!(
                "{}:\\{}",
                drive.to_ascii_uppercase(),
                rest.replace('/', "\\")
            )));
        }
    }
    let path = Path::new(raw);
    path.is_absolute().then(|| path.to_path_buf())
}

/// The real path of an existing local file, without Windows' `\\?\` prefix. A
/// mapped drive canonicalizes to its network path and counts as not local.
fn existing_local_path(path: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    let text = real.to_string_lossy();
    let plain = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if plain.starts_with("UNC\\") || is_network_path(plain) {
        return None;
    }
    Some(PathBuf::from(plain))
}

/// `path` relative to `root` with `/` separators, by text alone: a file deleted
/// locally is still in the remote tree, so it cannot be required to exist. The
/// remainder keeps the spelling the transcript used, because git's is exact.
fn relative_to_root(root: &Path, path: &Path) -> Option<String> {
    let root = normalized(root)?;
    let path = normalized(path)?;
    if path.len() <= root.len() {
        return None;
    }
    let same_root = root.iter().zip(&path).all(|(a, b)| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    });
    same_root.then(|| path[root.len()..].join("/"))
}

/// The path's parts with `.` and `..` resolved. `None` if it climbs above its
/// own start, which no real path in a repository does.
fn normalized(path: &Path) -> Option<Vec<String>> {
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    Some(parts)
}

/// Select the file in the system's file manager. This shows it and never opens
/// it: a transcript can name any executable, and a click must not run it.
pub fn reveal(path: &Path) -> std::io::Result<()> {
    spawn_file_manager(path)
}

#[cfg(windows)]
fn spawn_file_manager(path: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    if path.to_string_lossy().contains('"') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a quote in a path",
        ));
    }
    // Written by hand: the standard quoting wraps the whole `/select,…` when the
    // path has a space, and Explorer then opens Documents instead.
    let mut argument = std::ffi::OsString::from("/select,\"");
    argument.push(path);
    argument.push("\"");
    std::process::Command::new("explorer.exe")
        .raw_arg(argument)
        .spawn()
        .map(drop)
}

#[cfg(target_os = "macos")]
fn spawn_file_manager(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(drop)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn spawn_file_manager(path: &Path) -> std::io::Result<()> {
    // Handing xdg-open the file itself would launch it, so only its folder.
    let folder = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    std::process::Command::new("xdg-open")
        .arg(folder)
        .spawn()
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Dev")
            .env("GIT_AUTHOR_EMAIL", "dev@example.com")
            .env("GIT_COMMITTER_NAME", "Dev")
            .env("GIT_COMMITTER_EMAIL", "dev@example.com")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git is installed");
        assert!(status.success(), "git {args:?} failed");
    }

    fn write(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    /// `pushed.md` and `src/deleted.rs` are on origin; `src/deleted.rs` is gone
    /// locally; `only_here.md` was never committed. The URL is rewritten to look
    /// like GitHub after cloning, because that is what makes `git_remote` known.
    fn repo_with_origin() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        git(&work, &["init", "-q", "-b", "main"]);
        write(&work.join("pushed.md"));
        write(&work.join("src/deleted.rs"));
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "first"]);
        git(
            &work,
            &["remote", "add", "origin", &origin.to_string_lossy()],
        );
        git(&work, &["push", "-q", "origin", "main"]);
        git(
            &work,
            &["remote", "set-url", "origin", "https://github.com/a/b.git"],
        );
        std::fs::remove_file(work.join("src/deleted.rs")).unwrap();
        write(&work.join("only_here.md"));
        (tmp, work)
    }

    #[test]
    fn a_pushed_file_links_to_the_host_and_a_local_one_does_not() {
        let (_tmp, work) = repo_with_origin();
        let locator = FileLocator::for_project(&work.to_string_lossy());

        let pushed = locator.locate("pushed.md");
        assert_eq!(pushed.state, LinkState::Remote);
        assert_eq!(pushed.repo_path.as_deref(), Some("pushed.md"));
        assert_eq!(pushed.branch.as_deref(), Some("main"));
        assert!(pushed.local_path.is_some());

        let local = locator.locate("only_here.md");
        assert_eq!(local.state, LinkState::Local);
        assert!(local.repo_path.is_none());
        assert!(local.local_path.is_some());

        assert_eq!(locator.locate("never_existed.md").state, LinkState::Missing);
    }

    #[test]
    fn a_file_deleted_locally_is_still_on_the_host() {
        let (_tmp, work) = repo_with_origin();
        let locator = FileLocator::for_project(&work.to_string_lossy());
        let link = locator.locate("src/deleted.rs");
        assert_eq!(link.state, LinkState::Remote);
        assert!(link.local_path.is_none());
    }

    #[test]
    fn absolute_paths_are_located_inside_and_outside_the_repository() {
        let (tmp, work) = repo_with_origin();
        let locator = FileLocator::for_project(&work.to_string_lossy());

        let inside = work.join("pushed.md");
        let link = locator.locate(&inside.to_string_lossy());
        assert_eq!(link.state, LinkState::Remote);
        assert_eq!(link.repo_path.as_deref(), Some("pushed.md"));

        let outside = tmp.path().join("elsewhere.txt");
        write(&outside);
        assert_eq!(
            locator.locate(&outside.to_string_lossy()).state,
            LinkState::Local
        );
    }

    #[test]
    fn a_session_in_a_subfolder_finds_paths_written_from_either_level() {
        let (_tmp, work) = repo_with_origin();
        write(&work.join("sub/inner.md"));
        let locator = FileLocator::for_project(&work.join("sub").to_string_lossy());
        assert_eq!(locator.locate("pushed.md").state, LinkState::Remote);
        assert_eq!(locator.locate("inner.md").state, LinkState::Local);
    }

    #[test]
    fn a_remote_without_tracking_refs_cannot_be_called_missing() {
        let tmp = tempfile::tempdir().unwrap();
        git(tmp.path(), &["init", "-q", "-b", "main"]);
        git(
            tmp.path(),
            &["remote", "add", "origin", "https://github.com/a/b.git"],
        );
        let locator = FileLocator::for_project(&tmp.path().to_string_lossy());
        assert_eq!(locator.locate("src/x.rs").state, LinkState::Unverified);
    }

    #[test]
    fn a_folder_with_no_repository_only_knows_the_disk() {
        let tmp = tempfile::tempdir().unwrap();
        write(&tmp.path().join("notes.md"));
        let locator = FileLocator::for_project(&tmp.path().to_string_lossy());
        assert_eq!(locator.locate("notes.md").state, LinkState::Local);
        assert_eq!(locator.locate("other.md").state, LinkState::Missing);
    }

    #[test]
    fn network_paths_are_refused_before_anything_touches_them() {
        let tmp = tempfile::tempdir().unwrap();
        let locator = FileLocator::for_project(&tmp.path().to_string_lossy());
        for hostile in [
            r"\\evil.example\share\a.md",
            "//evil.example/share/a.md",
            r"\/evil.example\share\a.md",
            r"\\?\UNC\evil.example\share\a.md",
            r"\\.\pipe\x",
        ] {
            assert!(locator.candidates(hostile).is_empty(), "{hostile}");
            assert_eq!(locator.locate(hostile).state, LinkState::Missing);
            assert!(locator.existing(hostile).is_none());
        }
    }

    #[test]
    fn a_label_for_a_project_never_resolves_against_the_working_directory() {
        let locator = FileLocator::for_project("web");
        assert_eq!(locator.locate("Cargo.toml").state, LinkState::Missing);
    }

    #[test]
    fn junk_is_not_a_path() {
        let tmp = tempfile::tempdir().unwrap();
        let locator = FileLocator::for_project(&tmp.path().to_string_lossy());
        assert!(locator.candidates("").is_empty());
        assert!(locator.candidates("a\nb.md").is_empty());
        assert!(locator.candidates(&"x".repeat(5000)).is_empty());
    }

    #[test]
    fn the_repository_path_keeps_the_transcripts_spelling() {
        let root = Path::new("/repo");
        assert_eq!(
            relative_to_root(root, Path::new("/repo/./src/../src/A.rs")).as_deref(),
            Some("src/A.rs")
        );
        assert_eq!(relative_to_root(root, Path::new("/other/a.rs")), None);
        assert_eq!(relative_to_root(root, Path::new("/repo")), None);
    }
}
