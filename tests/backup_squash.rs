//! Folding the backup repository's history, against a bare repository on disk.
//!
//! The squash is the one operation in this crate that rewrites a remote branch, so
//! the interesting parts are not the local commit but the rules around it: it
//! refuses while the two sides differ, it keeps an archive branch when asked, and a
//! second machine must then be able to back up again instead of erroring out over
//! histories that no longer share an ancestor. Real git, real pushes - into a
//! tempdir, so nothing here can reach GitHub or any repository of the user's.
//!
//! `git` is required; without it the tests say so and pass, like the other
//! git-dependent suites.

use sessionwiki::{backup, index};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Everything a run reads is process-global - `SESSIONWIKI_DATA`,
/// `WIKIAGENTS_BACKUP_DIR`, the current directory and HOME - so one machine at a
/// time.
static LOCK: Mutex<()> = Mutex::new(());

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("git {:?} at {}: {e}", args, dir.display()));
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A `file:///` URL for a path, so a Windows `D:\tmp\…` never reaches git as a
/// string full of separators it would have to guess about.
fn url_of(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// One "machine": a temp home, a temp index, a staging directory of its own and a
/// bare repository standing in for GitHub.
struct Machine {
    root: tempfile::TempDir,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
    saved_cwd: PathBuf,
    stage: PathBuf,
    remote: PathBuf,
}

impl Machine {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("tempdir");
        let stage = root.path().join("stage");
        std::fs::create_dir_all(&stage).unwrap();
        let remote = root.path().join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git(root.path(), &["init", "--bare", &remote.to_string_lossy()]);

        let saved_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let keys = [
            "SESSIONWIKI_DATA",
            "WIKIAGENTS_BACKUP_DIR",
            "HOME",
            "USERPROFILE",
            "WIKIAGENTS_WEB_SESSION_DIR",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
        ];
        let saved: Vec<(String, Option<std::ffi::OsString>)> = keys
            .iter()
            .map(|k| ((*k).to_string(), std::env::var_os(k)))
            .collect();

        // A home with nothing in it: the export step would otherwise pick up this
        // machine's real web-chat exports through the current directory.
        std::env::set_current_dir(root.path()).unwrap();
        std::env::set_var("SESSIONWIKI_DATA", root.path().join("db"));
        std::env::set_var("WIKIAGENTS_BACKUP_DIR", &stage);
        std::env::set_var("HOME", root.path());
        std::env::set_var("USERPROFILE", root.path());
        std::env::set_var("WIKIAGENTS_WEB_SESSION_DIR", root.path().join("no_chats"));
        std::env::set_var("GIT_AUTHOR_NAME", "WikiAgents Test");
        std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.invalid");
        std::env::set_var("GIT_COMMITTER_NAME", "WikiAgents Test");
        std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.invalid");

        Machine {
            root,
            saved,
            saved_cwd,
            stage,
            remote,
        }
    }

    fn stage(&self) -> &Path {
        &self.stage
    }

    fn remote_url(&self) -> String {
        url_of(&self.remote)
    }

    fn use_stage(&self, dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::env::set_var("WIKIAGENTS_BACKUP_DIR", dir);
    }

    /// An index with one session and one note. The note is what changes between
    /// backups: identical content would leave the worktree clean and skip the
    /// commit, and the test needs commits.
    fn seed(&self) -> rusqlite::Connection {
        let conn = index::open().unwrap();
        conn.execute_batch(
            "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
             VALUES ('/s1.jsonl', 100, 10, 's1', 'claude-code', 'D:/proj', 'first', '2026-10-01T10:00:00Z', '2026-10-01T10:30:00Z', 1, 'main');
             INSERT INTO messages(session_id, role, text, full_text, is_rewound) VALUES ('s1', 'user', 'fold the history please', NULL, 0);
             INSERT INTO notes(session_id, note, updated) VALUES ('s1', 'revision 0', '2026-10-05T00:00:00+00:00');",
        )
        .unwrap();
        conn
    }

    fn set_note(&self, conn: &rusqlite::Connection, text: &str) {
        conn.execute(
            "UPDATE notes SET note = ?1, updated = '2026-10-06T00:00:00+00:00' WHERE session_id = 's1'",
            rusqlite::params![text],
        )
        .unwrap();
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
        let _ = std::env::set_current_dir(&self.saved_cwd);
    }
}

fn tracked_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = git(dir, &["ls-tree", "-r", "--name-only", "HEAD"])
        .lines()
        .map(|l| l.to_string())
        .collect();
    names.sort();
    names
}

fn remote_heads(url: &str) -> String {
    let out = std::process::Command::new("git")
        .args(["ls-remote", "--heads", url])
        .output()
        .expect("git ls-remote");
    assert!(out.status.success(), "ls-remote failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn three_backups_fold_into_one_commit_that_keeps_every_file_and_an_archive_branch() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    let url = machine.remote_url();

    for i in 1..=3 {
        machine.set_note(&conn, &format!("revision {i}"));
        backup::run_backup(&conn, &url).expect("a backup to a local bare repo");
    }
    let stage = machine.stage();
    assert_eq!(
        git(stage, &["rev-list", "--count", "HEAD"]),
        "3",
        "three backups are three commits"
    );
    let before = tracked_files(stage);
    assert!(
        before.iter().any(|f| f.ends_with("s1.json")),
        "the session files are in the tree: {before:?}"
    );

    let size = backup::repo_size(stage).unwrap();
    assert_eq!(size.commits, 3, "the monitor counts what git counts");
    assert!(size.working_bytes > 0, "the working files are measured");
    assert_eq!(
        backup::size_level(size.pack_bytes),
        backup::SizeLevel::Ok,
        "a test repository is nowhere near 800 MB"
    );

    let report = backup::squash_history(&conn, &url, true).expect("the fold succeeds");
    assert_eq!(report.before.commits, 3);
    assert_eq!(report.after.commits, 1, "one commit holds the tree now");
    assert_eq!(git(stage, &["rev-list", "--count", "HEAD"]), "1");
    assert_eq!(
        tracked_files(stage),
        before,
        "folding history must not change a single file"
    );
    assert!(
        git(stage, &["log", "--format=%s", "-1"]).contains("history squashed"),
        "the commit says what it did, which is how the next machine recognises it"
    );
    assert!(
        backup::get_backup_config(&conn)
            .unwrap()
            .last_squash_at
            .is_some(),
        "the fold is recorded for the UI to stop recommending it"
    );

    let heads = remote_heads(&url);
    let archive = report
        .archive_branch
        .expect("--keep-archive names a branch");
    assert!(
        heads.contains(&format!("refs/heads/{archive}")),
        "the old history stays reachable on the server: {heads}"
    );
    assert_eq!(
        git(
            &machine.remote,
            &["rev-list", "--count", &format!("refs/heads/{archive}")]
        ),
        "3",
        "the archive branch has the three commits that were folded"
    );
    assert_eq!(
        git(&machine.remote, &["rev-list", "--count", "refs/heads/main"]),
        "1",
        "main is the single commit"
    );
}

#[test]
fn the_fold_refuses_when_the_remote_and_this_machine_disagree() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    let url = machine.remote_url();
    backup::run_backup(&conn, &url).unwrap();

    // Another clone moves the remote ahead of this staging directory. The bare
    // repo's HEAD still points at the branch `git init` invented, so the clone
    // has to be told which branch it is working on.
    let other = machine.root.path().join("other");
    git(
        machine.root.path(),
        &["clone", "--branch", "main", &url, &other.to_string_lossy()],
    );
    git(&other, &["checkout", "-B", "main", "origin/main"]);
    std::fs::write(other.join("README.md"), "someone else pushed this\n").unwrap();
    git(other.as_path(), &["add", "-A"]);
    git(
        other.as_path(),
        &["commit", "-m", "a commit this machine has not seen"],
    );
    git(other.as_path(), &["push", "origin", "main"]);

    let err = backup::squash_history(&conn, &url, true)
        .expect_err("a repository that is behind must not be folded");
    let msg = err.to_string();
    assert!(msg.contains("differ"), "says why: {msg}");
    assert!(msg.contains("--restore"), "and what to do first: {msg}");

    let stage = machine.stage();
    assert_eq!(
        git(stage, &["rev-list", "--count", "HEAD"]),
        "1",
        "the refusal changed nothing here"
    );
    assert!(
        !remote_heads(&url).contains("refs/heads/archive/"),
        "a refused fold pushes no archive branch either"
    );
}

#[test]
fn after_a_fold_the_other_machine_backs_up_again_without_losing_files() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    let url = machine.remote_url();

    for i in 1..=2 {
        machine.set_note(&conn, &format!("revision {i}"));
        backup::run_backup(&conn, &url).unwrap();
    }

    // The second machine exists before the fold, so it carries the old history.
    let second = machine.root.path().join("second-machine");
    git(
        machine.root.path(),
        &["clone", "--branch", "main", &url, &second.to_string_lossy()],
    );
    // The remote advertises HEAD as the branch `git init --bare` invented, so a
    // clone lands on an unborn local branch. Whatever its name, this machine
    // works on main.
    git(&second, &["checkout", "-B", "main", "origin/main"]);
    assert_eq!(git(&second, &["rev-list", "--count", "HEAD"]), "2");

    backup::squash_history(&conn, &url, false).expect("fold without an archive branch");
    assert_eq!(
        git(&machine.remote, &["rev-list", "--count", "refs/heads/main"]),
        "1",
        "the remote is one commit now"
    );

    // Its next backup has nothing in common with the remote. Replaying its two old
    // commits on top would undo the fold, so it adopts the squashed history and
    // puts its own files above it.
    machine.set_note(&conn, "written on the second machine");
    machine.use_stage(&second);
    let report = backup::run_backup(&conn, &url).expect("the second machine backs up");
    assert!(report.success, "{}", report.message);

    assert_eq!(
        git(&second, &["rev-list", "--count", "HEAD"]),
        "2",
        "the squashed commit plus this backup - the old history is not back"
    );
    let files = tracked_files(&second);
    assert!(
        files.iter().any(|f| f.ends_with("s1.json")) && files.contains(&"README.md".to_string()),
        "no file was dropped by the adoption: {files:?}"
    );
    assert_eq!(
        git(&machine.remote, &["rev-list", "--count", "refs/heads/main"]),
        "2",
        "and the remote took it as a normal fast-forward"
    );
}

#[test]
fn repo_size_reads_gits_own_numbers_and_ignores_the_git_directory() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    backup::run_backup(&conn, &machine.remote_url()).unwrap();

    let stage = machine.stage();
    let size = backup::repo_size(stage).unwrap();
    assert_eq!(size.commits, 1);

    // The working total is the tree, not the object store: everything inside
    // `.git` - including this repository's own objects - must be skipped, or a
    // backup would report itself as growing on every run.
    let mut expected = 0u64;
    for entry in walkdir::WalkDir::new(stage)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .flatten()
    {
        if entry.file_type().is_file() {
            expected += entry.metadata().map(|m| m.len()).unwrap_or(0);
        }
    }
    assert_eq!(
        size.working_bytes, expected,
        "working bytes is exactly the tree"
    );
    assert!(
        size.pack_bytes + size.loose_bytes > 0,
        "objects are counted"
    );
}

#[test]
fn the_size_thresholds_are_the_github_limit() {
    use backup::SizeLevel;
    let mib = 1024 * 1024;
    assert_eq!(backup::size_level(799 * mib), SizeLevel::Ok);
    assert_eq!(backup::size_level(backup::PACK_WARN_BYTES), SizeLevel::Warn);
    assert_eq!(backup::size_level(900 * mib), SizeLevel::Warn);
    assert_eq!(
        backup::size_level(backup::PACK_LIMIT_BYTES),
        SizeLevel::Critical
    );
    assert_eq!(
        backup::PACK_LIMIT_BYTES,
        1024 * mib,
        "GitHub's recommended 1 GB"
    );
}

#[test]
fn a_backup_with_nothing_new_adds_no_commit_but_still_pushes() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    let url = machine.remote_url();

    backup::run_backup(&conn, &url).unwrap();
    backup::run_backup(&conn, &url).unwrap();
    let stage = machine.stage();
    assert_eq!(git(stage, &["rev-list", "--count", "HEAD"]), "1");

    // A commit a failed push left behind goes out with the next run, even one
    // that has nothing of its own to add.
    machine.set_note(&conn, "revision 1");
    let gone = machine.remote.with_extension("gone");
    std::fs::rename(&machine.remote, &gone).unwrap();
    assert!(backup::run_backup(&conn, &url).is_err());
    std::fs::rename(&gone, &machine.remote).unwrap();
    backup::run_backup(&conn, &url).unwrap();
    assert_eq!(
        git(stage, &["rev-parse", "HEAD"]),
        git(&machine.remote, &["rev-parse", "main"]),
        "the remote caught up"
    );
}

#[test]
fn a_failed_backup_is_what_the_status_says() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let conn = machine.seed();
    let url = machine.remote_url();
    backup::run_backup(&conn, &url).unwrap();
    let status = || backup::get_backup_config(&conn).unwrap().last_status;
    assert_eq!(status().as_deref(), Some("ok"));

    machine.set_note(&conn, "revision 1");
    let gone = machine.remote.with_extension("gone");
    std::fs::rename(&machine.remote, &gone).unwrap();
    assert!(backup::run_backup(&conn, &url).is_err());
    std::fs::rename(&gone, &machine.remote).unwrap();
    let after = status().unwrap_or_default();
    assert!(after.starts_with("error:"), "{after}");
}

#[test]
fn nested_chat_exports_are_copied_and_the_rest_of_their_folder_is_not() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !git_available() {
        eprintln!("skipping: git is not available on this machine");
        return;
    }
    let machine = Machine::new();
    let chats = machine.root.path().join("chats");
    std::fs::create_dir_all(chats.join("gpt")).unwrap();
    std::fs::write(chats.join("gpt/conversations-000.json"), "[]").unwrap();
    std::fs::write(
        chats.join("gpt/user.json"),
        r#"{"email":"me@example.invalid"}"#,
    )
    .unwrap();
    std::fs::write(chats.join("gemini_all.json"), "[]").unwrap();
    std::env::set_var("WIKIAGENTS_WEB_SESSION_DIR", &chats);

    let conn = machine.seed();
    backup::run_backup(&conn, &machine.remote_url()).unwrap();
    let files = tracked_files(machine.stage());
    assert!(files
        .iter()
        .any(|f| f == "web_session/gpt/conversations-000.json"));
    assert!(files.iter().any(|f| f == "web_session/gemini_all.json"));
    assert!(!files.iter().any(|f| f.ends_with("user.json")), "{files:?}");
}
