//! The `AI-Session:` trailer: what `wikiagents hook install-git` installs, and
//! what `wikiagents hook prepare-commit-msg` does with a commit message. This
//! is the exact half of AI Git Blame - a commit tagged by the hook needs no
//! timestamp guesswork, and a session that only quoted a SHA is not credited.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_sessionwiki");
const SESSION: &str = "019a-tags-a-commit";

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

/// A throwaway directory the hook can be pointed at.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn hook(cwd: &Path, msg: &Path, source: Option<&str>, session: Option<&str>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.current_dir(cwd)
        .arg("hook")
        .arg("prepare-commit-msg")
        .arg(msg);
    if let Some(s) = source {
        cmd.arg(s);
    }
    match session {
        Some(id) => cmd.env("WIKIAGENTS_SESSION_ID", id),
        None => cmd.env_remove("WIKIAGENTS_SESSION_ID"),
    };
    cmd.output().unwrap()
}

/// A fresh repository with one commit, configured so a global user config
/// (hooks path, signing, autocrlf) cannot decide the outcome.
fn init_repo(dir: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["-c", "core.hooksPath=.git/hooks"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=Tester",
        "-c",
        "user.email=tester@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "chore: empty base commit",
    ]);
}

#[test]
fn a_session_id_becomes_a_trailer_and_is_never_doubled() {
    let dir = scratch("sessionwiki-test-trailer-env");
    let msg = dir.join("COMMIT_EDITMSG");
    std::fs::write(&msg, "fix: refresh the expired token\n").unwrap();

    let out = hook(&dir, &msg, None, Some(SESSION));
    assert!(out.status.success(), "the hook must exit 0");
    let tagged = std::fs::read_to_string(&msg).unwrap();
    assert!(
        tagged.contains("AI-Session: 019a-tags-a-commit"),
        "trailer appended: {tagged}"
    );
    assert!(
        tagged.starts_with("fix: refresh the expired token"),
        "the subject stays first: {tagged}"
    );

    // An amend runs the hook over the message that already carries the trailer.
    hook(&dir, &msg, None, Some("another-session-99"));
    let once = std::fs::read_to_string(&msg).unwrap();
    assert_eq!(
        once.matches("AI-Session:").count(),
        1,
        "one trailer, and the first id wins: {once}"
    );
    assert!(!once.contains("another-session-99"));
}

#[test]
fn nothing_is_added_without_an_id_or_for_a_squash_and_merge() {
    let dir = scratch("sessionwiki-test-trailer-silent");
    for (source, name) in [
        (None, "message"),
        (Some("squash"), "squash"),
        (Some("merge"), "merge"),
    ] {
        let msg = dir.join(name);
        std::fs::write(&msg, "rebase: keep the history\n").unwrap();
        let out = hook(&dir, &msg, source, None);
        assert!(out.status.success(), "still exits 0 with no id");
        assert_eq!(
            std::fs::read_to_string(&msg).unwrap(),
            "rebase: keep the history\n",
            "message from `{name}` untouched"
        );
    }
    // A path that does not exist is not our problem to fix.
    let out = hook(&dir, &dir.join("absent-msg"), None, Some(SESSION));
    assert!(out.status.success(), "never blocks a commit");

    // git also calls this hook with a third argument (the sha of a reword or
    // amend), and a hand-written `"$@"` must not be rejected over it.
    let msg = dir.join("three_args");
    std::fs::write(&msg, "docs: mention the third argument\n").unwrap();
    let out = Command::new(BIN)
        .current_dir(&dir)
        .env("WIKIAGENTS_SESSION_ID", SESSION)
        .args([
            "hook",
            "prepare-commit-msg",
            msg.to_str().unwrap(),
            "message",
            "abc1234",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(std::fs::read_to_string(&msg)
        .unwrap()
        .contains("AI-Session: 019a-tags-a-commit"));
}

#[test]
fn install_git_tags_real_commits_and_leaves_foreign_hooks_alone() {
    if !git_available() {
        eprintln!("skipping: git is not available");
        return;
    }
    let dir = scratch("sessionwiki-test-install-git-hook");
    init_repo(&dir);

    let install = || {
        let out = Command::new(BIN)
            .current_dir(&dir)
            .args(["hook", "install-git"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "install-git: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    install();
    let path = dir.join(".git").join("hooks").join("prepare-commit-msg");
    assert!(path.is_file(), "the hook was written");
    install();
    let script = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        script.matches("hook prepare-commit-msg").count(),
        1,
        "our own hook is not installed twice: {script}"
    );

    // A commit made by a session records that session, and git itself reads it
    // back - which is all `wikiagents commit <sha>` needs to be exact. The id
    // travels through the environment, which is what an agent-side integration
    // (or a wrapper script) sets; a plain terminal commit has none.
    std::fs::write(dir.join("auth.rs"), "fn refresh() {}\n").unwrap();
    let git_commit = |args: &[&str], session: Option<&str>| {
        let mut cmd = Command::new("git");
        cmd.current_dir(&dir)
            .args(["-c", "core.hooksPath=.git/hooks"])
            .args([
                "-c",
                "user.name=Tester",
                "-c",
                "user.email=tester@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args);
        match session {
            Some(id) => cmd.env("WIKIAGENTS_SESSION_ID", id),
            None => cmd.env_remove("WIKIAGENTS_SESSION_ID"),
        };
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    Command::new("git")
        .current_dir(&dir)
        .arg("add")
        .arg("auth.rs")
        .output()
        .unwrap();
    git_commit(
        &["commit", "-q", "-m", "fix: refresh the expired token"],
        Some(SESSION),
    );
    let sha = Command::new("git")
        .current_dir(&dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let sha = String::from_utf8_lossy(&sha.stdout).trim().to_string();
    assert_eq!(
        sessionwiki::blame::commit_session_trailer(&dir, &sha).as_deref(),
        Some(SESSION),
        "the hook's trailer is readable from the commit"
    );

    // A commit made without any session id stays untagged.
    std::fs::write(dir.join("plain.rs"), "fn noop() {}\n").unwrap();
    Command::new("git")
        .current_dir(&dir)
        .arg("add")
        .arg("plain.rs")
        .output()
        .unwrap();
    git_commit(&["commit", "-q", "-m", "chore: no session here"], None);
    let head = Command::new("git")
        .current_dir(&dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let untagged = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(
        sessionwiki::blame::commit_session_trailer(&dir, &untagged),
        None
    );
    // Not a sha at all: no crash, no git flag injection.
    assert_eq!(
        sessionwiki::blame::commit_session_trailer(&dir, "--help"),
        None
    );
}

#[test]
fn a_foreign_hook_is_reported_not_overwritten() {
    if !git_available() {
        eprintln!("skipping: git is not available");
        return;
    }
    let dir = scratch("sessionwiki-test-install-git-foreign");
    init_repo(&dir);
    let path = dir.join(".git").join("hooks").join("prepare-commit-msg");
    std::fs::write(&path, "#!/bin/sh\necho somebody elses hook\n").unwrap();

    let out = Command::new(BIN)
        .current_dir(&dir)
        .args(["hook", "install-git"])
        .output()
        .unwrap();
    assert!(out.status.success(), "it is a note, not a failure");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "#!/bin/sh\necho somebody elses hook\n",
        "the other tool's hook survives"
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.contains("wikiagents hook prepare-commit-msg"),
        "and the user is told what to add: {printed}"
    );
}
