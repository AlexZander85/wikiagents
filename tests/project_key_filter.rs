//! One project, however a tool spelled it. `files.project_key` is the form the
//! filters match on, so `--project D:\projectx` also finds the sessions an
//! export wrote as `D:\\projectx` and the ones started in a subfolder of it.
//! The column is filled on the first open after it ships, then at index time.

use rusqlite::{params, Connection};
use sessionwiki::index;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

/// The same folder as four tools recorded it, plus a folder inside it.
fn spellings() -> Vec<String> {
    if cfg!(windows) {
        vec![
            r"D:\\projectx".to_string(),
            r#""D:/projectx/""#.to_string(),
            r"d:\projectx".to_string(),
            r"d:\projectx\src".to_string(),
        ]
    } else {
        vec![
            r#""/home/u/projectx""#.to_string(),
            "/home/u/projectx//".to_string(),
            "/home/u/projectx".to_string(),
            "/home/u/projectx/src".to_string(),
        ]
    }
}

fn insert(conn: &Connection, path: &str, project: &str) {
    conn.execute(
        "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started,
                           msg_count, kind, reader)
         VALUES (?1, 5, 5, ?1, 'claude-code', ?2, 't', '2026-10-01T00:00:00+00:00', 1, 'main', ?3)",
        params![path, project, index::READER],
    )
    .unwrap();
}

fn key_of(conn: &Connection, path: &str) -> String {
    conn.query_row(
        "SELECT project_key FROM files WHERE path = ?1",
        [path],
        |r| r.get(0),
    )
    .unwrap()
}

fn listed(conn: &Connection, project: &str) -> Vec<String> {
    let rows = index::recent_range(
        conn,
        50,
        &index::ListQuery {
            project: Some(project),
            ..Default::default()
        },
        index::SessionSort::Created,
    )
    .unwrap();
    let mut ids: Vec<String> = rows.into_iter().map(|r| r.project).collect();
    ids.sort();
    ids
}

#[test]
fn every_spelling_of_a_project_gets_one_key_and_one_filter() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-project-key");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SESSIONWIKI_DATA", &dir);

    // Rows written before the column existed carry an empty key.
    let conn = index::open().unwrap();
    let spellings = spellings();
    for (i, project) in spellings.iter().enumerate() {
        insert(&conn, &format!("/s/{i}.jsonl"), project);
    }
    drop(conn);

    // The next open backfills, once.
    let conn = index::open().unwrap();
    let root = if cfg!(windows) {
        "d:/projectx".to_string()
    } else {
        "/home/u/projectx".to_string()
    };
    assert_eq!(key_of(&conn, "/s/0.jsonl"), root);
    assert_eq!(key_of(&conn, "/s/1.jsonl"), root);
    assert_eq!(key_of(&conn, "/s/2.jsonl"), root);
    assert_eq!(key_of(&conn, "/s/3.jsonl"), format!("{root}/src"));

    // One filter, typed the way a human types it, reaches all four.
    let query = if cfg!(windows) {
        r"D:\projectx"
    } else {
        "/home/u/projectx"
    };
    assert_eq!(
        listed(&conn, query).len(),
        4,
        "the folder, its escaped and quoted forms, and its subfolder"
    );
    // A session started in a subfolder belongs to the repository around it, so
    // filtering by either half of that path finds the whole project.
    let sub = if cfg!(windows) {
        r"D:\projectx\src"
    } else {
        "/home/u/projectx/src"
    };
    assert_eq!(
        listed(&conn, sub).len(),
        4,
        "a subfolder and its repository are one project"
    );

    // A fragment that is not a path keeps its substring match.
    assert_eq!(
        listed(&conn, "projectx").len(),
        4,
        "a bare name still matches by substring"
    );
    // And a sibling is not swept in by the prefix rules.
    assert_eq!(
        listed(&conn, &format!("{query}-other")).len(),
        0,
        "a path that merely shares a prefix is another project"
    );
}

/// A repository and its subfolders are one project to `project`, so the row limit
/// of a listing is spent across all of them. A view of one folder asks for that
/// folder and gets it, in whichever spelling it was stored.
#[test]
fn a_folder_listing_spends_its_limit_on_that_folder() {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join("sessionwiki-test-project-folder");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SESSIONWIKI_DATA", &dir);

    let conn = index::open().unwrap();
    for i in 0..5 {
        insert(&conn, &format!("/s/root{i}.jsonl"), "/repo/x");
    }
    insert(&conn, "/s/sub.jsonl", "/repo/x/sub");
    insert(&conn, "/s/deep.jsonl", "/repo/x/sub/deep/");
    insert(&conn, "/s/sibling.jsonl", "/repo/x/sub-other");

    let folders = index::project_folders(&conn, "/repo/x/sub/").unwrap();
    assert_eq!(folders.len(), 2, "{folders:?}");
    let rows = index::recent_range(
        &conn,
        2,
        &index::ListQuery {
            folders: Some(&folders),
            ..Default::default()
        },
        index::SessionSort::Created,
    )
    .unwrap();
    assert_eq!(rows.len(), 2, "the limit is not used up by the repository");
    assert!(rows.iter().all(
        |r| r.project.starts_with("/repo/x/sub") && !r.project.starts_with("/repo/x/sub-other")
    ));

    let none = index::recent_range(
        &conn,
        10,
        &index::ListQuery {
            folders: Some(&[]),
            ..Default::default()
        },
        index::SessionSort::Created,
    )
    .unwrap();
    assert!(none.is_empty(), "a folder nothing lives in lists nothing");
}

#[test]
fn a_folder_contains_itself_and_what_is_below_it_and_nothing_else() {
    let key = sessionwiki::util::project_key("/repo/x");
    assert!(index::project_in_folder("/repo/x/", &key));
    assert!(index::project_in_folder("/repo/x/sub", &key));
    assert!(!index::project_in_folder("/repo/x-other", &key));
    assert!(!index::project_in_folder("/repo", &key));
    // The "no project" view is not a root of every absolute path.
    assert!(index::project_in_folder("", ""));
    assert!(!index::project_in_folder("/repo/x", ""));
}
