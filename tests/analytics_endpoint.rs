use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct WebChild(Child);

impl Drop for WebChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The header the Web UI puts on every `/api/` call. Nothing else can: a
/// cross-site page has no way to add a custom header to an `<img>` or a form.
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
        assert!(
            Instant::now() < deadline,
            "web server did not start in time"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, address)
}

/// Send one `Connection: close` request and return the whole response, status
/// line included.
///
/// A body bigger than the server's write buffer arrives `Transfer-Encoding:
/// chunked`, and a raw socket sees the framing a browser hides - including a
/// chunk boundary that cuts a multibyte character in half. So the bytes are
/// read as bytes and the framing is undone before the response is turned into
/// text; the page itself is the only answer big enough for that path today.
fn exchange(address: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(stream, "{request}").unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let Some(split) = raw
        .windows(4)
        .position(|w| w == &b"\r\n\r\n"[..])
        .map(|i| i + 4)
    else {
        return String::from_utf8_lossy(&raw).to_string();
    };
    let (head, body) = raw.split_at(split);
    let head = String::from_utf8_lossy(head).to_string();
    let chunked = head.lines().skip(1).any(|line| {
        let (name, value) = line.split_once(':').unwrap_or(("", ""));
        name.trim_matches(' ')
            .eq_ignore_ascii_case("transfer-encoding")
            && value.contains("chunked")
    });
    let body = if chunked {
        decode_chunked(body)
    } else {
        body.to_vec()
    };
    format!("{head}\r\n\r\n{}", String::from_utf8_lossy(&body))
}

/// Concatenate the payloads of a `Transfer-Encoding: chunked` body.
fn decode_chunked(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut rest = body;
    while let Some(line_end) = rest
        .windows(2)
        .position(|w| w == &b"\r\n"[..])
        .map(|i| i + 2)
    {
        let line = std::str::from_utf8(&rest[..line_end - 2])
            .unwrap_or("")
            .trim();
        let Ok(size) = usize::from_str_radix(line, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let Some(end) = line_end.checked_add(size) else {
            break;
        };
        if end > rest.len() {
            break;
        }
        out.extend_from_slice(&rest[line_end..end]);
        rest = &rest[end..];
        if rest.starts_with(b"\r\n") {
            rest = &rest[2..];
        }
    }
    out
}

fn get(address: SocketAddr, path: &str) -> String {
    exchange(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n{API_HEADER}Connection: close\r\n\r\n"),
    )
}

fn get_without_header(address: SocketAddr, path: &str) -> String {
    exchange(
        address,
        &format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"),
    )
}

fn body_of(response: &str) -> &str {
    let (_, body) = response
        .split_once("\r\n\r\n")
        .expect("HTTP headers separator");
    body
}

#[test]
fn analytics_endpoint_returns_main_sessions_and_period_files() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // One main session with a subagent and an edit: the Activity tab gets the
    // main one only, as a compact row, plus totals.
    std::env::set_var("SESSIONWIKI_DATA", &data_dir);
    let conn = sessionwiki::index::open().unwrap();
    conn.execute_batch(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
         VALUES ('/a.jsonl', 0, 1, 'main-1', 'claude-code', 'D:/proj', 't', '2026-10-24T22:30:00.000+00:00', '2026-10-24T23:10:00Z', 7, 'main'),
                ('/b.jsonl', 0, 1, 'sub-1', 'claude-code', 'D:/proj', 't', '2026-10-24T22:40:00Z', '2026-10-24T22:50:00Z', 3, 'sub');
         INSERT INTO touched(session_id, path) VALUES ('main-1', 'src/lib.rs');
         INSERT INTO edits(session_id, path, kind, ts, snippet) VALUES ('main-1', 'src/lib.rs', 'edit', NULL, 'x');",
    )
    .unwrap();
    drop(conn);

    let (_child, address) = spawn_web(&data_dir);

    let response = get(address, "/api/analytics");
    assert!(
        response.starts_with("HTTP/1.1 200 "),
        "expected 200 OK, got: {response}"
    );
    let json: serde_json::Value =
        serde_json::from_str(body_of(&response)).expect("valid JSON response");
    assert_eq!(json["tools"], serde_json::json!(["claude-code"]));
    assert_eq!(json["projects"], serde_json::json!(["D:/proj"]));
    // [start, end, tool, project, messages, edits] in Unix seconds; no subagent.
    assert_eq!(
        json["sessions"],
        serde_json::json!([[1792881000i64, 1792883400i64, 0, 0, 7, 1]])
    );
    assert_eq!(json["totals"]["touched_files"], 1);

    let files = get(
        address,
        "/api/analytics/files?since=2026-10-24T00:00:00Z&until=2026-10-25T23:59:59Z",
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body_of(&files)).unwrap(),
        serde_json::json!([{"path": "src/lib.rs", "sessions": 1, "edits": 1}])
    );
    let none = get(address, "/api/analytics/files?since=2026-10-26T00:00:00Z");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body_of(&none)).unwrap(),
        serde_json::json!([])
    );
}

#[test]
fn api_endpoints_require_the_ui_header_and_post() {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    // Nothing to seed: the server creates its own index in this directory, and
    // opening one here would race the other test in this binary over
    // SESSIONWIKI_DATA.
    let (_child, address) = spawn_web(&data_dir);

    // The page itself is a navigation, so it stays reachable without the header.
    assert!(
        get_without_header(address, "/").starts_with("HTTP/1.1 200 "),
        "the HTML page must not need the header"
    );
    // Without it, no data leaves: this is what an <img> or a forged form gets.
    let refused = get_without_header(address, "/api/stats");
    assert!(
        refused.starts_with("HTTP/1.1 403 "),
        "headerless /api/stats: {refused}"
    );
    assert!(
        get(address, "/api/stats").starts_with("HTTP/1.1 200 "),
        "our own call must still work"
    );
    // State-changing endpoints take POST only, even carrying the header.
    for path in ["/api/backup/run", "/api/forget", "/api/snippet/delete"] {
        let wrong_method = get(address, path);
        assert!(
            wrong_method.starts_with("HTTP/1.1 405 "),
            "GET {path}: {wrong_method}"
        );
    }
}
