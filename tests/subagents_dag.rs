use rusqlite::params;
use sessionwiki::index;
use std::path::PathBuf;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn fresh_index() -> (rusqlite::Connection, PathBuf) {
    let dir = std::env::temp_dir().join(format!("sessionwiki-test-dag-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SESSIONWIKI_DATA", &dir);
    let conn = index::open().unwrap();
    (conn, dir)
}

fn seed_session(conn: &rusqlite::Connection, id: &str, parent_id: Option<&str>, is_sub: bool) {
    conn.execute(
        "INSERT OR REPLACE INTO files
         (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id)
         VALUES (?1, 0, 0, ?2, 'test-tool', '/repo', ?2, '2026-06-10T10:00:00+00:00', '2026-06-10T10:00:00+00:00', 1, ?3, ?4)",
        params![
            format!("/tmp/sessions/{id}.jsonl"),
            id,
            if is_sub { "sub" } else { "main" },
            parent_id,
        ],
    ).unwrap();
}

#[test]
fn test_subagent_dag_linking_and_queries() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (conn, dir) = fresh_index();

    seed_session(&conn, "parent-session-1", None, false);
    seed_session(&conn, "child-sub-1", Some("parent-session-1"), true);
    seed_session(&conn, "child-sub-2", Some("parent-session-1"), true);
    seed_session(&conn, "other-main-2", None, false);

    // 1. Default recent sessions (include_subagents = false) returns only main sessions
    let mains = index::recent(&conn, 50, None, None, None, false).unwrap();
    assert_eq!(mains.len(), 2);
    let main_ids: Vec<&str> = mains.iter().map(|s| s.session_id.as_str()).collect();
    assert!(main_ids.contains(&"parent-session-1"));
    assert!(main_ids.contains(&"other-main-2"));
    assert!(!main_ids.contains(&"child-sub-1"));

    // 2. All recent sessions (include_subagents = true) returns all 4
    let all = index::recent(&conn, 50, None, None, None, true).unwrap();
    assert_eq!(all.len(), 4);

    // 3. Query subagents for parent
    let children = index::subagents_for_parent(&conn, "parent-session-1").unwrap();
    assert_eq!(children.len(), 2);
    let child_ids: Vec<&str> = children.iter().map(|s| s.session_id.as_str()).collect();
    assert!(child_ids.contains(&"child-sub-1"));
    assert!(child_ids.contains(&"child-sub-2"));
    assert_eq!(children[0].parent_id.as_deref(), Some("parent-session-1"));
    assert_eq!(children[1].parent_id.as_deref(), Some("parent-session-1"));

    // 4. Resolve child returns parent_id
    let resolved_child = index::resolve(&conn, "child-sub-1").unwrap();
    assert_eq!(resolved_child.len(), 1);
    assert_eq!(
        resolved_child[0].parent_id.as_deref(),
        Some("parent-session-1")
    );

    // 5. Unrelated session has no subagents
    let no_children = index::subagents_for_parent(&conn, "other-main-2").unwrap();
    assert!(no_children.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}
