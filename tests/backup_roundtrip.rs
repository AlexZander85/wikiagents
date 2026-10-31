//! Backup export -> fresh index -> restore, without touching git.
//!
//! The restore used to be quietly broken: it wrote `INSERT INTO msgs
//! (session_id, text)` into an external-content FTS5 table whose only column
//! is `text`, swallowed the error with `.ok()`, and re-inserted every message
//! on each run. So this test pins the three things that were wrong:
//! a restored session is searchable, a second restore adds nothing, and a
//! curated note comes back.

use sessionwiki::backup;
use sessionwiki::index;
use std::sync::Mutex;

// SESSIONWIKI_DATA is process-global, so the two databases are built one after
// the other; a poisoned lock still lets the next test run.
static LOCK: Mutex<()> = Mutex::new(());

fn seed_source(dir: &std::path::Path) -> rusqlite::Connection {
    std::env::set_var("SESSIONWIKI_DATA", dir);
    let conn = index::open().unwrap();
    conn.execute_batch(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
         VALUES ('/s1.jsonl', 100, 10, 's1', 'claude-code', 'D:/proj', 'first', '2026-10-01T10:00:00Z', '2026-10-01T10:30:00Z', 2, 'main'),
                ('/s2.jsonl', 200, 10, 's2', 'codex', 'D:/proj', 'second', '2026-10-02T10:00:00Z', '2026-10-02T10:30:00Z', 1, 'main');
         INSERT INTO touched(session_id, path) VALUES ('s1', 'src/paint.rs');
         INSERT INTO session_commits(session_id, commit_sha) VALUES ('s1', 'a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2');",
    )
    .unwrap();
    for (sid, role, text) in [
        ("s1", "user", "настрой gitlab-ci для deploy"),
        (
            "s1",
            "assistant",
            "used the zylophone helper to wire the pipeline",
        ),
        ("s2", "user", "another session about nginx"),
    ] {
        conn.execute(
            "INSERT INTO messages(session_id, role, text, full_text, is_rewound) VALUES (?1,?2,?3,NULL,0)",
            rusqlite::params![sid, role, text],
        )
        .unwrap();
        // The real indexer's pattern: the FTS rowid IS messages.id.
        conn.execute(
            "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
            rusqlite::params![conn.last_insert_rowid(), text],
        )
        .unwrap();
    }
    conn.execute_batch(
        "INSERT INTO tags(session_id, tag) VALUES ('s1', 'ci');
         INSERT INTO notes(session_id, note, updated) VALUES ('s1', 'keep this recipe', '2026-10-05T00:00:00+00:00');
         INSERT INTO snippets(session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts)
         VALUES ('s1', 1, 'deploy helper', 'rust', 'fn main() {}', '', '2026-10-01T10:20:00Z', 'claude-code', 'assistant', '2026-10-01T10:20:00Z');",
    )
    .unwrap();
    conn
}

fn message_count(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT count(*) FROM messages", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn backup_export_and_restore_roundtrip_is_searchable_and_idempotent() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    let src_dir = root.path().join("src-db");
    let dst_dir = root.path().join("dst-db");
    let export_dir = root.path().join("export");
    std::fs::create_dir_all(&export_dir).unwrap();

    // 1. Export from a populated index into a plain directory (no git).
    let source = seed_source(&src_dir);
    let sessions_dir = export_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let curated_dir = export_dir.join("curated");
    std::fs::create_dir_all(&curated_dir).unwrap();
    backup::export_curated_tables(&source, &curated_dir).unwrap();
    let (total, written) = backup::export_granular_sessions(&source, &export_dir).unwrap();
    assert_eq!(total, 2, "both sessions are listed");
    assert_eq!(written, 2, "both sessions are fresh files");
    drop(source);

    // 2. Restore into an empty index.
    std::env::set_var("SESSIONWIKI_DATA", &dst_dir);
    let mut dest = index::open().unwrap();

    backup::restore_curated_tables(&dest, &curated_dir).unwrap();
    let report = backup::restore_granular_sessions(&mut dest, &sessions_dir).unwrap();
    assert_eq!(
        (report.restored, report.skipped, report.failed),
        (2, 0, 0),
        "a fresh index restores everything"
    );

    // (a) The restored text is findable - this is what the broken FTS insert
    // used to hide behind `.ok()`.
    let hits = index::search(&dest, "zylophone", 10, None, None).unwrap();
    let found: Vec<&str> = hits.iter().map(|h| h.row.session_id.as_str()).collect();
    assert!(found.contains(&"s1"), "restored message is not searchable");

    // (c) The curated note came back, and so did the tag and the snippet.
    let note: String = dest
        .query_row("SELECT note FROM notes WHERE session_id = 's1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(note, "keep this recipe");
    let counts: (i64, i64, i64) = dest
        .query_row(
            "SELECT (SELECT count(*) FROM tags), (SELECT count(*) FROM snippets), (SELECT count(*) FROM touched)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(counts, (1, 1, 1), "tags, snippets and provenance restored");

    // Restored rows are marked archived, or the next sync sweep would delete
    // every one of them as a session that vanished from disk.
    let archived: i64 = dest
        .query_row(
            "SELECT count(*) FROM files WHERE archived_at IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(archived, 2);

    // (b) A second restore is a no-op: everything is skipped, nothing doubles.
    let before = message_count(&dest);
    let again = backup::restore_granular_sessions(&mut dest, &sessions_dir).unwrap();
    assert_eq!(
        (again.restored, again.skipped, again.failed),
        (0, 2, 0),
        "already-restored sessions must be skipped"
    );
    assert_eq!(message_count(&dest), before, "messages were duplicated");

    // Curated merge is not destructive either way.
    backup::restore_curated_tables(&dest, &curated_dir).unwrap();
    let notes: i64 = dest
        .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
        .unwrap();
    assert_eq!(notes, 1);
}

#[test]
fn a_restore_keeps_the_newer_local_note_and_never_duplicates_a_tag() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = tempfile::tempdir().unwrap();
    std::env::set_var("SESSIONWIKI_DATA", root.path().join("db"));
    let conn = index::open().unwrap();
    conn.execute_batch(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind)
         VALUES ('/s1.jsonl', 0, 1, 's1', 'claude-code', 'D:/proj', 't', NULL, NULL, 0, 'main');
         INSERT INTO notes(session_id, note, updated) VALUES ('s1', 'written locally yesterday', '2026-10-20T00:00:00+00:00');
         INSERT INTO tags(session_id, tag) VALUES ('s1', 'local');",
    )
    .unwrap();

    let curated = root.path().join("curated");
    std::fs::create_dir_all(&curated).unwrap();
    // An older note from the remote, and a tag this machine already has.
    std::fs::write(
        curated.join("notes.json"),
        r#"[{"session_id":"s1","note":"older remote draft","updated":"2026-10-01T00:00:00+00:00"}]"#,
    )
    .unwrap();
    std::fs::write(
        curated.join("tags.json"),
        r#"[{"session_id":"s1","tag":"local"},{"session_id":"s1","tag":"from-remote"}]"#,
    )
    .unwrap();
    // A file we cannot read must fail loudly, not look like "nothing to restore".
    std::fs::write(curated.join("favorites.json"), "{ not json").unwrap();
    assert!(backup::restore_curated_tables(&conn, &curated).is_err());

    // With the unreadable file fixed, the merge keeps the newer local note.
    std::fs::write(curated.join("favorites.json"), "[]").unwrap();
    backup::restore_curated_tables(&conn, &curated).unwrap();
    let note: String = conn
        .query_row("SELECT note FROM notes WHERE session_id = 's1'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        note, "written locally yesterday",
        "an older remote note overwrote local work"
    );

    // The tag the remote also has is not inserted twice; its own tag is added.
    let tags: Vec<String> = conn
        .prepare("SELECT tag FROM tags WHERE session_id = 's1' ORDER BY tag")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    assert_eq!(tags, vec!["from-remote".to_string(), "local".to_string()]);
}
