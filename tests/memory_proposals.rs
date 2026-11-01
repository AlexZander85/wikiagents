//! An agent proposes, a person decides.
//!
//! `remember` writes straight into the knowledge base, so a wrong conclusion is
//! recalled as fact by every later session. These tests hold the queue to its
//! contract: nothing an agent offered reaches the knowledge base - or the web
//! API, or a backup - before a human approves it, and what does reach it is
//! redacted.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The header the Web UI puts on every `/api/` call; nothing else can add it.
const API_HEADER: &str = "X-WikiAgents: 1\r\n";

struct WebChild(Child);

impl Drop for WebChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

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
        assert!(
            Instant::now() < deadline,
            "web server did not start in time"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, address)
}

fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{request}").unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn get(address: SocketAddr, path: &str) -> String {
    exchange(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n{API_HEADER}Connection: close\r\n\r\n"),
    )
}

fn post(address: SocketAddr, path: &str, body: &str, with_header: bool) -> String {
    let header = if with_header { API_HEADER } else { "" };
    exchange(
        address,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: {address}\r\n{header}Content-Type: application/json\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn body_of(response: &str) -> &str {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP headers separator");
    body
}

/// A fresh index in `dir`, with the queue empty.
fn seed(dir: &Path) -> rusqlite::Connection {
    std::env::set_var("SESSIONWIKI_DATA", dir);
    let conn = sessionwiki::index::open().unwrap();
    conn.execute_batch("DELETE FROM memory_proposals; DELETE FROM snippets;")
        .unwrap();
    conn
}

fn memories(conn: &rusqlite::Connection) -> Vec<sessionwiki::index::Snippet> {
    sessionwiki::index::list_snippets(conn, None, None, None, None, Some("assistant_memory"), 50)
        .unwrap()
}

#[test]
fn a_proposal_becomes_a_snippet_only_when_a_person_approves_it() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-approve");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);

    let id = sessionwiki::index::propose_memory(
        &conn,
        "JWT refresh policy",
        "Rotate the refresh token every 15 minutes.",
        Some("auth, security"),
        Some("s1"),
        Some("claude-code"),
    )
    .unwrap();

    let pending = sessionwiki::index::list_memory_proposals(&conn, Some("pending")).unwrap();
    assert_eq!(pending.len(), 1, "one queued proposal");
    assert_eq!(pending[0].id, id);
    assert_eq!(pending[0].status, "pending");
    assert_eq!(pending[0].tags, "auth, security");
    assert_eq!(pending[0].session_id.as_deref(), Some("s1"));
    assert_eq!(pending[0].tool.as_deref(), Some("claude-code"));
    assert_eq!(pending[0].snippet_id, None);
    // The whole point: an agent's offer is not knowledge yet.
    assert!(memories(&conn).is_empty(), "queue writes nothing");

    let snippet_id = sessionwiki::index::approve_memory_proposal(&conn, &id).unwrap();
    // Approving twice must not save the note twice.
    assert_eq!(
        sessionwiki::index::approve_memory_proposal(&conn, &id).unwrap(),
        snippet_id,
        "approval is idempotent"
    );

    assert!(
        sessionwiki::index::list_memory_proposals(&conn, Some("pending"))
            .unwrap()
            .is_empty(),
        "nothing left to decide"
    );
    let approved = sessionwiki::index::list_memory_proposals(&conn, Some("approved")).unwrap();
    assert_eq!(approved.len(), 1);
    assert_eq!(approved[0].snippet_id, Some(snippet_id));
    assert!(approved[0].decided_at.is_some());

    let saved = memories(&conn);
    assert_eq!(saved.len(), 1, "one snippet from two approvals");
    assert_eq!(saved[0].title, "JWT refresh policy");
    assert_eq!(saved[0].lang.as_deref(), Some("markdown"));
    assert_eq!(saved[0].session_id, "s1");
    assert_eq!(
        saved[0].content,
        "Rotate the refresh token every 15 minutes."
    );

    // `memory list` with no status shows the pending queue, not the decided ones.
    let as_json = sessionwiki::index::list_memory_proposals(&conn, None).unwrap();
    assert_eq!(as_json.len(), 1, "no filter means every state");
    assert_eq!(as_json[0].status, "approved");
    let all = sessionwiki::index::list_memory_proposals(&conn, Some("all")).unwrap();
    assert_eq!(all.len(), 1, "`all` is the same as no filter");
}

#[test]
fn a_rejected_proposal_stays_as_a_record_and_never_saves() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-reject");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);

    let id = sessionwiki::index::propose_memory(
        &conn,
        "Guess about the cache",
        "Maybe the WAL lock needs a bigger timeout.",
        None,
        None,
        None,
    )
    .unwrap();
    assert!(
        sessionwiki::index::reject_memory_proposal(&conn, &id).unwrap(),
        "the proposal exists"
    );

    assert_eq!(
        sessionwiki::index::list_memory_proposals(&conn, Some("rejected"))
            .unwrap()
            .len(),
        1,
        "the decision is kept"
    );
    assert!(
        memories(&conn).is_empty(),
        "a rejected note never reaches the knowledge base"
    );
    assert!(
        !sessionwiki::index::reject_memory_proposal(&conn, "deadbeefcafe").unwrap(),
        "an unknown id is not silently accepted"
    );
    assert!(sessionwiki::index::approve_memory_proposal(&conn, "deadbeefcafe").is_err());
}

#[test]
fn a_proposal_is_redacted_and_capped_before_it_is_queued() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-redact");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);

    // A secret must not sit in the queue: the queue is read by the web UI and
    // travels in backups long before anyone approves it.
    let body = format!("deploy with key {}", concat!("AKIA", "IOSFODNN7EXAMPLE"));
    let long_title = "x".repeat(sessionwiki::index::PROPOSAL_TITLE_MAX + 120);
    let id =
        sessionwiki::index::propose_memory(&conn, &long_title, &body, None, None, None).unwrap();
    let queued = sessionwiki::index::list_memory_proposals(&conn, Some("pending")).unwrap();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].id, id);
    assert!(
        !queued[0].content.contains("AKIAIOSFODNN7EXAMPLE"),
        "token redacted: {}",
        queued[0].content
    );
    assert!(
        queued[0].content.contains("[redacted:"),
        "redaction marked: {}",
        queued[0].content
    );
    assert_eq!(
        queued[0].title.chars().count(),
        sessionwiki::index::PROPOSAL_TITLE_MAX,
        "title capped"
    );

    let huge = "line\n".repeat(6000);
    let id =
        sessionwiki::index::propose_memory(&conn, "Long note", &huge, None, None, None).unwrap();
    let queued = sessionwiki::index::list_memory_proposals(&conn, Some("pending")).unwrap();
    let second = queued.iter().find(|p| p.id == id).expect("the long note");
    assert!(
        second.content.chars().count() <= sessionwiki::index::PROPOSAL_CONTENT_MAX + 20,
        "body capped, was {}",
        second.content.chars().count()
    );
    assert!(second.content.contains("truncated"));
    assert!(
        second.content.contains('\n'),
        "capping keeps the markdown shape"
    );

    // An empty note is not a memory.
    assert!(sessionwiki::index::propose_memory(&conn, "  ", "content", None, None, None).is_err());
    assert!(sessionwiki::index::propose_memory(&conn, "title", "   ", None, None, None).is_err());
}

#[test]
fn the_web_api_serves_the_queue_and_only_the_app_can_decide() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-web");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);
    let id = sessionwiki::index::propose_memory(
        &conn,
        "Sync runs on a worker",
        "The web server hands a sync to a worker thread.",
        Some("architecture"),
        Some("s1"),
        Some("codex"),
    )
    .unwrap();
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    drop(conn);

    let (_child, address) = spawn_web(&dir);

    let response = get(address, "/api/memory/proposals?status=pending");
    assert!(
        response.starts_with("HTTP/1.1 200 "),
        "expected 200 OK, got: {response}"
    );
    let json: serde_json::Value =
        serde_json::from_str(body_of(&response)).expect("valid JSON response");
    assert_eq!(json.as_array().map(Vec::len), Some(1), "one pending item");
    assert_eq!(json[0]["id"], id.as_str());
    assert_eq!(json[0]["status"], "pending");
    assert_eq!(json[0]["title"], "Sync runs on a worker");

    // Approving writes to the index, so it needs the app header like any other
    // state-changing call: without it, a cross-site page could empty the queue.
    let body = format!(r#"{{"id":"{id}"}}"#);
    let blocked = post(address, "/api/memory/approve", &body, false);
    assert!(
        blocked.starts_with("HTTP/1.1 403 "),
        "expected 403 without the header, got: {blocked}"
    );
    let blocked_reject = post(address, "/api/memory/reject", &body, false);
    assert!(
        blocked_reject.starts_with("HTTP/1.1 403 "),
        "expected 403 without the header, got: {blocked_reject}"
    );
    // A GET on the same path cannot decide either.
    let wrong_method = get(address, "/api/memory/approve");
    assert!(
        !wrong_method.starts_with("HTTP/1.1 200 "),
        "a GET must not approve: {wrong_method}"
    );

    let allowed = post(address, "/api/memory/approve", &body, true);
    assert!(
        allowed.starts_with("HTTP/1.1 200 "),
        "expected 200 OK, got: {allowed}"
    );
    let out: serde_json::Value =
        serde_json::from_str(body_of(&allowed)).expect("approval answers with JSON");
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["status"], "approved");

    let conn = sessionwiki::index::open_readonly().unwrap();
    assert!(
        sessionwiki::index::list_memory_proposals(&conn, Some("pending"))
            .unwrap()
            .is_empty(),
        "the queue drained through the API"
    );
    let saved = memories(&conn);
    assert_eq!(saved.len(), 1, "the approval wrote the snippet");
    assert_eq!(saved[0].title, "Sync runs on a worker");
    // The read-only handle above is the shape the MCP server and the CLI use:
    // it must answer with the queue, not fail on the table it cannot create.

    // One more note, queued while the server was up, for the rejection path.
    let writable = sessionwiki::index::open().unwrap();
    let second = sessionwiki::index::propose_memory(
        &writable,
        "Rejected over HTTP",
        "A second offer nobody wants.",
        None,
        None,
        None,
    )
    .unwrap();
    drop(writable);

    // Reading a decided id back is the rejected path's turn.
    let rejected_body = format!(r#"{{"id":"{second}"}}"#);
    let rejected = post(address, "/api/memory/reject", &rejected_body, true);
    assert!(
        rejected.starts_with("HTTP/1.1 200 "),
        "expected 200 OK, got: {rejected}"
    );
    let out: serde_json::Value =
        serde_json::from_str(body_of(&rejected)).expect("rejection answers with JSON");
    assert_eq!(out["ok"], serde_json::json!(true));
    assert_eq!(out["status"], "rejected");

    let conn = sessionwiki::index::open().unwrap();
    assert!(
        sessionwiki::index::list_memory_proposals(&conn, Some("pending"))
            .unwrap()
            .is_empty(),
        "both decisions cleared the queue"
    );
    assert_eq!(
        sessionwiki::index::list_memory_proposals(&conn, Some("rejected"))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(memories(&conn).len(), 1, "a rejection saves nothing");
}

#[test]
fn the_mcp_propose_memory_tool_queues_without_saving() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-mcp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);
    drop(conn);

    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"propose_memory","arguments":{"title":"WAL busy timeout","content":"Set busy_timeout to 5s in the reader pool.","tags":"sqlite","session_id":"s9","tool":"codex"}}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_snippets","arguments":{"role":"assistant_memory"}}}"#,
    ];

    let mut child = Command::new(env!("CARGO_BIN_EXE_sessionwiki"))
        .arg("mcp")
        .env("SESSIONWIKI_DATA", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for r in requests {
            writeln!(stdin, "{r}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "EOF must exit 0");
    let stdout = String::from_utf8(out.stdout).unwrap();

    let mut proposed_id = String::new();
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let id = value["id"].as_i64();
        if id == Some(2) {
            let text = value["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(parsed[0]["status"], "pending", "{parsed}");
            assert_eq!(parsed[0]["title"], "WAL busy timeout");
            proposed_id = parsed[0]["id"].as_str().unwrap_or_default().to_string();
            assert_eq!(proposed_id.len(), 12, "the id is a short hash");
        }
        if id == Some(3) {
            let text = value["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert_eq!(
                parsed.as_array().map(Vec::len),
                Some(0),
                "proposing saved nothing: {parsed}"
            );
        }
    }
    assert!(
        !proposed_id.is_empty(),
        "propose_memory answered with an id"
    );

    // The proposal survives the server process: the human decides it later.
    let conn = sessionwiki::index::open().unwrap();
    let pending = sessionwiki::index::list_memory_proposals(&conn, Some("pending")).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, proposed_id);
    assert_eq!(pending[0].session_id.as_deref(), Some("s9"));
    assert_eq!(pending[0].tool.as_deref(), Some("codex"));
}

#[test]
fn the_cli_lists_and_approves_a_proposal() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-memory-cli");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let conn = seed(&dir);
    let id = sessionwiki::index::propose_memory(
        &conn,
        "Sweep runs once per version",
        "The reader sweep stores its marker in meta.",
        Some("index"),
        Some("s3"),
        Some("qoder"),
    )
    .unwrap();
    drop(conn);

    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_sessionwiki"))
            .env("SESSIONWIKI_DATA", &dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    };

    let pending: serde_json::Value =
        serde_json::from_str(run(&["memory", "list", "--json"]).trim()).unwrap();
    assert_eq!(pending.as_array().map(Vec::len), Some(1));
    assert_eq!(pending[0]["id"], id.as_str());
    assert_eq!(pending[0]["status"], "pending");

    // The default view is the queue a person works through.
    let shown = run(&["memory", "list"]);
    assert!(shown.contains("Sweep runs once per version"), "{shown}");
    assert!(!shown.contains("approved"), "decided notes stay out");

    let answer: serde_json::Value =
        serde_json::from_str(run(&["memory", "approve", &id, "--json"]).trim()).unwrap();
    assert_eq!(answer["status"], "approved");
    assert!(answer["snippet_id"].as_i64().unwrap() > 0);

    let empty: serde_json::Value =
        serde_json::from_str(run(&["memory", "list", "--json"]).trim()).unwrap();
    assert_eq!(empty.as_array().map(Vec::len), Some(0), "queue drained");
    let approved: serde_json::Value =
        serde_json::from_str(run(&["memory", "list", "--status", "approved", "--json"]).trim())
            .unwrap();
    assert_eq!(approved.as_array().map(Vec::len), Some(1));

    let conn = sessionwiki::index::open_readonly().unwrap();
    assert_eq!(
        memories(&conn).len(),
        1,
        "the CLI approval wrote the snippet"
    );

    // An id that is not in the queue is an error, not a silent no-op.
    let bad = Command::new(env!("CARGO_BIN_EXE_sessionwiki"))
        .env("SESSIONWIKI_DATA", &dir)
        .args(["memory", "approve", "deadbeefcafe"])
        .output()
        .unwrap();
    assert!(
        !bad.status.success(),
        "approving an unknown id must fail: {}",
        String::from_utf8_lossy(&bad.stdout)
    );
    let stderr = String::from_utf8_lossy(&bad.stderr).to_lowercase();
    assert!(
        stderr.contains("deadbeefcafe"),
        "the error names the id: {stderr}"
    );
}
