//! The listing endpoints must answer for the sessions the feed is showing, not
//! for the whole index: a sidebar counter that says 3 210 while the list under
//! it holds 1 169 is the bug these tests pin down.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// `SESSIONWIKI_DATA` is process-global, so the tests in this binary take turns.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct WebChild(Child);

impl Drop for WebChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const API_HEADER: &str = "X-WikiAgents: 1\r\n";

fn spawn_web(data_dir: &Path) -> (WebChild, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let mut child = WebChild(
        Command::new(env!("CARGO_BIN_EXE_sessionwiki"))
            .env("SESSIONWIKI_DATA", data_dir)
            .args(["web", "--no-open", "--port", &address.port().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "web server exited unexpectedly"
        );
        if TcpStream::connect(address).is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "web server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, address)
}

fn get(address: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {address}\r\n{API_HEADER}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn json_of(response: &str) -> serde_json::Value {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP headers separator");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("bad JSON ({e}) in: {response}"))
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Dev")
        .env("GIT_AUTHOR_EMAIL", "dev@example.com")
        .env("GIT_COMMITTER_NAME", "Dev")
        .env("GIT_COMMITTER_EMAIL", "dev@example.com")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "gc.auto=0",
        ])
        .args(args)
        .spawn()
        .expect("git is installed")
        .wait()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn seed(data_dir: &Path, f: &dyn Fn(&mut rusqlite::Connection)) {
    std::env::set_var("SESSIONWIKI_DATA", data_dir);
    let mut conn = sessionwiki::index::open().unwrap();
    f(&mut conn);
    drop(conn);
}

/// The feed filters short and web sessions out; every number beside it has to
/// move the same way, and the hidden count has to come from the index rather
/// than from a second copy of the feed.
#[test]
fn listings_counts_and_stats_share_the_visible_scope() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    seed(&data_dir, &|conn| {
        conn.execute_batch(
            "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
             VALUES ('/a.jsonl', 100, 1000, 'long',         'claude-code', 'D:/proj', 't', '2026-06-01T10:00:00Z', '2026-06-01T11:00:00Z', 5, 'main'),
                    ('/b.jsonl', 100, 1000, 'short',        'claude-code', 'D:/proj', 't', '2026-06-01T10:00:00Z', '2026-06-01T10:05:00Z', 1, 'main'),
                    ('/c.jsonl', 100, 4000, 'webchat',      'qoder-web',   'D:/proj', 't', '2026-06-02T10:00:00Z', '2026-06-02T10:30:00Z', 4, 'main'),
                    ('/d.jsonl', 100,  250, 'short_edited', 'claude-code', 'D:/proj', 't', '2026-06-03T10:00:00Z', '2026-06-03T10:05:00Z', 1, 'main');
             INSERT INTO touched(session_id, path) VALUES ('long', 'src/lib.rs'), ('webchat', 'src/lib.rs'), ('short_edited', 'src/web.rs');",
        )
        .unwrap();
    });

    let (_child, address) = spawn_web(&data_dir);

    let unfiltered = json_of(&get(address, "/api/projects"));
    assert_eq!(unfiltered[0]["sessions"], 4, "whole project: {unfiltered}");

    let filtered = json_of(&get(address, "/api/projects?no_short=1&no_web=1"));
    // `short_edited` stays although it holds one message: it edited a file, so a
    // trace can point at it and it must not vanish from the list beside it.
    assert_eq!(
        filtered[0]["sessions"], 2,
        "the long session and the one that edited a file: {filtered}"
    );

    let hidden = json_of(&get(address, "/api/short_hidden?no_web=1"));
    assert_eq!(
        hidden["hidden"], 1,
        "only the short session without edits is hidden"
    );

    let stats = json_of(&get(address, "/api/stats?no_short=1&no_web=1"));
    assert_eq!(stats["visible"]["sessions"], 2);
    assert_eq!(stats["visible"]["bytes"], 1250);
    assert_eq!(stats["visible"]["tools"], 1);
    // The dropdown still lists every tool ever indexed - it is a picker, not a count.
    assert_eq!(stats["tools"].as_array().unwrap().len(), 2);

    let trace_all = json_of(&get(address, "/api/trace?path=src/lib.rs"));
    assert_eq!(trace_all.as_array().unwrap().len(), 2);
    let trace_no_web = json_of(&get(address, "/api/trace?path=src/lib.rs&no_web=1"));
    let ids: Vec<&str> = trace_no_web
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["long"], "the web chat leaves the trace too: {ids:?}");

    let edited = json_of(&get(address, "/api/trace?path=src/web.rs&no_short=1"));
    let edited_ids: Vec<&str> = edited
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        edited_ids, ["short_edited"],
        "a session that edited the file survives the short filter: {edited_ids:?}"
    );
}

/// A commit hash is matched by prefix and, when the repository is on disk, read
/// with git. A value that is not a hash must be refused outright: as a `LIKE`
/// template `%` would return some other commit's message.
#[test]
fn git_commit_endpoint_validates_the_sha_and_reads_the_real_repo() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "add the first file"]);
    let sha = {
        let out = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };

    let repo_for_sql = repo.display().to_string().replace('\\', "/");
    let insert = format!(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
         VALUES ('/a.jsonl', 100, 10, 'sess-1', 'claude-code', '{repo_for_sql}', 't', '2026-06-01T10:00:00Z', '2026-06-01T11:00:00Z', 5, 'main');
         INSERT INTO session_commits(session_id, commit_sha, message)
         VALUES ('sess-1', '{sha}', 'add the first file'),
                ('other', 'deadbeefdeadbeef', 'a message from another session');"
    );
    seed(&data_dir, &|conn| {
        conn.execute_batch(&insert).unwrap();
    });

    let (_child, address) = spawn_web(&data_dir);

    for bad in ["sha=", "sha=%25", "sha=abc", "sha=deadbeefxyz", "sha=%25deadbeef"] {
        let response = get(address, &format!("/api/git/commit?{bad}"));
        assert!(
            response.starts_with("HTTP/1.1 400 "),
            "expected 400 for {bad:?}, got: {response}"
        );
    }

    let short = &sha[..8];
    let info = json_of(&get(
        address,
        &format!("/api/git/commit?sha={short}&session_id=sess-1"),
    ));
    assert_eq!(info["found_in_git"], true, "repo on disk answers: {info}");
    assert_eq!(info["sha"], sha);
    assert_eq!(info["subject"], "add the first file");

    // A prefix that is not this commit's never borrows another session's message.
    let foreign = json_of(&get(
        address,
        "/api/git/commit?sha=deadbeef&session_id=sess-1",
    ));
    assert_eq!(foreign["found_in_git"], false);
    assert!(
        foreign["message"].is_null(),
        "another session's commit must not be shown: {foreign}"
    );
}
