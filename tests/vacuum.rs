use sessionwiki::{commands, index};
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn fresh_index() -> (tempfile::TempDir, rusqlite::Connection) {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("SESSIONWIKI_DATA", dir.path());
    let conn = index::open().unwrap();
    (dir, conn)
}

#[test]
fn vacuum_preserves_data_and_search_results() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, conn) = fresh_index();

    // Insert sample metadata, files, messages, and FTS entries
    conn.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, msg_count, kind)
         VALUES ('/path/to/test.json', 1000, 500, 'session-v1', 'claude-code', 'proj', 'Refactor auth', '2026-10-24T12:00:00Z', 2, 'main')",
        [],
    ).unwrap();

    conn.execute(
        "INSERT INTO messages(session_id, role, text)
         VALUES ('session-v1', 'user', 'Please implement OAuth2 token verification')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO messages(session_id, role, text)
         VALUES ('session-v1', 'assistant', 'Implemented OAuth2 verification in auth.rs')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO msgs(rowid, text) VALUES (1, 'Please implement OAuth2 token verification')",
        [],
    )
    .unwrap();

    conn.execute(
        "INSERT INTO msgs(rowid, text) VALUES (2, 'Implemented OAuth2 verification in auth.rs')",
        [],
    )
    .unwrap();

    drop(conn);

    // Run vacuum command
    commands::vacuum().expect("vacuum should succeed");

    // Reopen and verify data and FTS search results are intact
    let conn = index::open().unwrap();

    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM files WHERE session_id = 'session-v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);

    let msg_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM messages WHERE session_id = 'session-v1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(msg_count, 2);

    // Verify FTS search works on preserved tables
    let search_hits: i64 = conn
        .query_row(
            "SELECT count(*) FROM msgs WHERE msgs MATCH 'OAuth2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(search_hits, 2);
}

#[test]
fn vacuum_insufficient_space_preflights() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, conn) = fresh_index();

    conn.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, msg_count, kind)
         VALUES ('/path/to/test.json', 1000, 500, 'session-space', 'claude-code', 'proj', 'Space test', '2026-10-24T12:00:00Z', 1, 'main')",
        [],
    ).unwrap();
    drop(conn);

    // Simulate insufficient free space (1 byte)
    std::env::set_var("SESSIONWIKI_SIMULATE_FREE_SPACE", "1");
    let err = commands::vacuum().expect_err("vacuum must fail when disk space is insufficient");
    std::env::remove_var("SESSIONWIKI_SIMULATE_FREE_SPACE");

    let msg = err.to_string();
    assert!(
        msg.contains("insufficient disk space"),
        "error message should indicate insufficient disk space, got: {msg}"
    );
}

#[test]
fn vacuum_lock_contention_reports_busy() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, conn1) = fresh_index();

    // Insert data on connection 1
    conn1.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, msg_count, kind)
         VALUES ('/path/to/test.json', 1000, 500, 'session-lock', 'claude-code', 'proj', 'Lock test', '2026-10-24T12:00:00Z', 1, 'main')",
        [],
    ).unwrap();

    // Start a read transaction on connection 1 (holding a read lock on WAL)
    let tx1 = conn1.unchecked_transaction().unwrap();
    let _val: i64 = tx1
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap();

    // Connection 2 attempts WAL checkpoint with truncate while connection 1 is actively reading
    let conn2 = index::open().unwrap();

    // We also make a write to WAL so there is an active WAL page to checkpoint
    conn2.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, msg_count, kind)
         VALUES ('/path/to/test2.json', 1000, 500, 'session-lock2', 'claude-code', 'proj', 'Lock test 2', '2026-10-24T12:00:00Z', 1, 'main')",
        [],
    ).unwrap();

    let (busy, _log, _checkpointed): (i32, i32, i32) = conn2
        .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();

    // Connection 1 held an open transaction reading the database, so the checkpoint cannot truncate
    assert_eq!(
        busy, 1,
        "WAL checkpoint should report busy == 1 under reader lock contention"
    );

    drop(tx1);
    drop(conn1);
    drop(conn2);
}

#[test]
fn new_index_and_vacuumed_index_use_incremental_auto_vacuum() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, conn) = fresh_index();
    let mode: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2, "a fresh index starts in incremental mode");

    // An index made before the setting: switch it off, then let vacuum convert it.
    conn.execute_batch("PRAGMA auto_vacuum = NONE; VACUUM;")
        .unwrap();
    drop(conn);
    commands::vacuum().unwrap();
    let conn = index::open().unwrap();
    let mode: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2, "vacuum converts an existing index");
}

#[test]
fn automatic_compaction_triggers_only_when_worth_it() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (_dir, conn) = fresh_index();
    let path = index::db_path().unwrap();

    // A small index is never worth interrupting anyone for.
    assert_eq!(index::compact_due(&conn, &path).unwrap(), None);

    // A large one still in auto_vacuum = NONE gets converted once.
    conn.execute_batch(
        "PRAGMA auto_vacuum = NONE; VACUUM;
         CREATE TABLE pad(b BLOB);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 80)
         INSERT INTO pad SELECT randomblob(256 * 1024) FROM n;
         PRAGMA wal_checkpoint(TRUNCATE);",
    )
    .unwrap();
    assert_eq!(
        index::compact_due(&conn, &path).unwrap(),
        Some("switch to incremental auto-vacuum")
    );
    index::compact(&conn, &path).unwrap();
    let mode: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, 2);
    assert_eq!(index::compact_due(&conn, &path).unwrap(), None);

    // A month later it runs again regardless.
    let month_ago = chrono::Utc::now().timestamp() - 31 * 86_400;
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'compact_at'",
        [month_ago.to_string()],
    )
    .unwrap();
    assert_eq!(
        index::compact_due(&conn, &path).unwrap(),
        Some("monthly compaction")
    );
    index::compact(&conn, &path).unwrap();

    // A big delete leaves free pages: over a tenth of the file triggers it.
    conn.execute_batch("DELETE FROM pad WHERE rowid % 2 = 0;")
        .unwrap();
    assert_eq!(
        index::compact_due(&conn, &path).unwrap(),
        Some("over 10% of the file is free pages")
    );
}
