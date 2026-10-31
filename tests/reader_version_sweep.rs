//! A parser fix for one tool must reach the sessions that tool already has on
//! disk, and it must not re-read the other tools. 1.3 stamps every `files` row
//! with the `reader_version` of its tool and, on the first open after a bump,
//! marks the live rows below that version for a re-parse - once, because the
//! versions are recorded in `meta`.

use rusqlite::{params, Connection};
use sessionwiki::index;

fn mtime(conn: &Connection, path: &str) -> i64 {
    conn.query_row("SELECT mtime FROM files WHERE path = ?1", [path], |r| {
        r.get(0)
    })
    .unwrap()
}

fn insert(conn: &Connection, path: &str, tool: &str, reader: i64, archived: bool) {
    conn.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended,
                           msg_count, kind, archived_at, reader)
         VALUES (?1, 5, 5, ?1, ?2, '/p', 't', '2026-10-01T00:00:00+00:00', NULL, 1, 'main', ?3, ?4)",
        params![
            path,
            tool,
            archived.then_some("2026-10-02T00:00:00+00:00"),
            reader
        ],
    )
    .unwrap();
}

#[test]
fn every_tool_is_reparsed_once_after_a_version_bump() {
    let dir = std::env::temp_dir().join("sessionwiki-test-reader-version-sweep");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SESSIONWIKI_DATA", &dir);

    let conn = index::open().unwrap();
    // One row per case: a codex session a version behind, a row that predates
    // the reader column, a row this version wrote, and an archived row.
    insert(&conn, "/c/stale.jsonl", "codex", index::READER - 1, false);
    insert(&conn, "/k/legacy.jsonl", "claude-code", 0, false);
    insert(
        &conn,
        "/k/current.jsonl",
        "claude-code",
        index::READER,
        false,
    );
    insert(&conn, "/c/gone.jsonl", "codex", index::READER - 1, true);
    // An index written by an older build carries no sweep mark; the first open
    // of this one finds the rows above stale and re-reads them.
    conn.execute(
        "DELETE FROM meta WHERE key = 'reader_versions_applied'",
        params![],
    )
    .unwrap();
    drop(conn);

    let conn = index::open().unwrap();
    assert_eq!(
        mtime(&conn, "/c/stale.jsonl"),
        -1,
        "a row below its tool's version is re-read"
    );
    assert_eq!(
        mtime(&conn, "/k/legacy.jsonl"),
        -1,
        "a row without a reader stamp is re-read too"
    );
    assert_eq!(
        mtime(&conn, "/k/current.jsonl"),
        5,
        "a row this version parsed stays"
    );
    assert_eq!(mtime(&conn, "/c/gone.jsonl"), 5, "archived rows stay");

    let swept: String = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'reader_versions_applied'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let versions: std::collections::BTreeMap<String, i64> = serde_json::from_str(&swept).unwrap();
    assert_eq!(
        versions.get("codex").copied(),
        Some(index::READER),
        "the sweep records the version it applied"
    );

    // A later open does not sweep again: the rows an old binary rewrites with
    // its stale reader wait for the next bump, they are not re-marked here.
    conn.execute(
        "UPDATE files SET mtime = 7 WHERE path = '/c/stale.jsonl'",
        params![],
    )
    .unwrap();
    drop(conn);
    let conn = index::open().unwrap();
    assert_eq!(mtime(&conn, "/c/stale.jsonl"), 7, "the sweep runs once");
}

#[test]
fn a_tool_without_an_override_sits_at_the_current_version() {
    for tool in sessionwiki::adapters::tool_names() {
        let got = sessionwiki::adapters::reader_version(tool);
        assert!(
            got >= index::READER,
            "{tool} at {got}, below this build's reader {}",
            index::READER
        );
        let override_set = sessionwiki::adapters::reader_overrides();
        if !override_set.iter().any(|(slug, _)| *slug == tool) {
            assert_eq!(got, index::READER, "{tool} has no override in this build");
        }
    }
}
