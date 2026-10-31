use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct WebChild(Child);

impl Drop for WebChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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

    let exe = env!("CARGO_BIN_EXE_sessionwiki");

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);

    let mut child = WebChild(
        Command::new(exe)
            .env("SESSIONWIKI_DATA", &data_dir)
            .args(["web", "--no-open", "--port", &address.port().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "web server exited unexpectedly"
        );
        if let Ok(stream) = TcpStream::connect(address) {
            break stream;
        }
        assert!(
            Instant::now() < deadline,
            "web server did not start in time"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let get = |stream: &mut TcpStream, path: &str| -> serde_json::Value {
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 200 "),
            "expected 200 OK, got: {response}"
        );
        let (_, body) = response
            .split_once("\r\n\r\n")
            .expect("HTTP headers separator");
        serde_json::from_str(body).expect("valid JSON response")
    };

    let json = get(&mut stream, "/api/analytics");
    assert_eq!(json["tools"], serde_json::json!(["claude-code"]));
    assert_eq!(json["projects"], serde_json::json!(["D:/proj"]));
    // [start, end, tool, project, messages, edits] in Unix seconds; no subagent.
    assert_eq!(
        json["sessions"],
        serde_json::json!([[1792881000i64, 1792883400i64, 0, 0, 7, 1]])
    );
    assert_eq!(json["totals"]["touched_files"], 1);

    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let files = get(
        &mut stream,
        "/api/analytics/files?since=2026-10-24T00:00:00Z&until=2026-10-25T23:59:59Z",
    );
    assert_eq!(
        files,
        serde_json::json!([{"path": "src/lib.rs", "sessions": 1, "edits": 1}])
    );
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let none = get(
        &mut stream,
        "/api/analytics/files?since=2026-10-26T00:00:00Z",
    );
    assert_eq!(none, serde_json::json!([]));
}
