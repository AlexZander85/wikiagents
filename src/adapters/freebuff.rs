use super::{
    clean_path, dedup_paths, redacted_truncate, title_from_messages, Adapter, Discovered, Store,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};

const KEY_SEP: char = '\u{1f}';
const SNIPPET_CAP: usize = 200;

/// Freebuff is an AI coding agent with a desktop app and CLI.
/// Freebuff Desktop stores each project's threads and messages in a SQLite
/// database at `~/.config/freebuff-desktop/projects/<project-slug>/desktop-v2.db`.
pub struct Freebuff;

fn open_ro(db: &Path) -> Result<Connection> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .or_else(|_| {
            let uri = format!("file:{}?immutable=1", db.display());
            Connection::open_with_flags(
                &uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )
        })
        .with_context(|| format!("open sqlite db {}", db.display()))
}

fn make_key(db: &Path, id: &str) -> String {
    format!("{}{KEY_SEP}{id}", db.display())
}

fn display_path(key: &str) -> String {
    key.replace(KEY_SEP, "#")
}

fn ms_to_datetime(ms: i64) -> Option<DateTime<Utc>> {
    if ms > 0 {
        DateTime::from_timestamp(ms / 1000, ((ms % 1000) * 1_000_000) as u32)
    } else {
        None
    }
}

fn base_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) =
        std::env::var_os("FREEBUFF_CONFIG_DIR").or_else(|| std::env::var_os("FREEBUFF_HOME"))
    {
        dirs.push(PathBuf::from(dir));
    }
    if let Some(home) = crate::util::home_dir() {
        dirs.push(home.join(".config").join("freebuff-desktop"));
    }
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("freebuff-desktop"));
        }
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        dirs.push(PathBuf::from(xdg).join("freebuff-desktop"));
    }
    dirs
}

fn find_all_dbs() -> Vec<PathBuf> {
    let mut dbs = Vec::new();
    for base in base_dirs() {
        if !base.is_dir() {
            continue;
        }
        let projects_dir = base.join("projects");
        if projects_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&projects_dir) {
                for entry in entries.flatten() {
                    let db_path = entry.path().join("desktop-v2.db");
                    if db_path.is_file() {
                        dbs.push(db_path);
                    }
                }
            }
        }
        let root_db = base.join("desktop-v2.db");
        if root_db.is_file() && !dbs.contains(&root_db) {
            dbs.push(root_db);
        }
    }
    dbs
}

fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = crate::redact::redact(text).into_owned(),
        Value::Array(values) => values.iter_mut().for_each(redact_json_strings),
        Value::Object(values) => values.values_mut().for_each(redact_json_strings),
        _ => {}
    }
}

impl Adapter for Freebuff {
    fn name(&self) -> &'static str {
        "freebuff"
    }

    fn root(&self) -> Option<PathBuf> {
        for base in base_dirs() {
            if base.is_dir() {
                let projects = base.join("projects");
                if projects.is_dir() {
                    return Some(projects);
                }
                return Some(base);
            }
        }
        crate::util::home_dir().map(|h| h.join(".config").join("freebuff-desktop").join("projects"))
    }

    fn reconcile_scope(&self) -> Option<String> {
        super::root_scope(self.root().as_deref())
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("freebuff sessions are indexed from sqlite store, not direct files")
    }

    fn store(&self) -> Option<Store> {
        let dbs = find_all_dbs();
        if dbs.is_empty() {
            return None;
        }

        let mut keys: Vec<(String, i64)> = Vec::new();
        let mut had_error = false;

        for db in &dbs {
            let conn = match open_ro(db) {
                Ok(c) => c,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            let mut stmt = match conn.prepare(
                "SELECT t.id, t.updated_at, t.created_at
                 FROM threads t
                 WHERE EXISTS (SELECT 1 FROM messages m WHERE m.thread_id = t.id)",
            ) {
                Ok(s) => s,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            let rows = stmt.query_map([], |r| {
                let id: String = r.get(0)?;
                let updated: Option<i64> = r.get(1)?;
                let created: Option<i64> = r.get(2)?;
                let token = updated.or(created).unwrap_or(0);
                Ok((id, token))
            });

            match rows {
                Ok(rows) => {
                    for (id, token) in rows.flatten() {
                        keys.push((make_key(db, &id), token));
                    }
                }
                Err(_) => had_error = true,
            }
        }

        Some(Store {
            keys,
            files: dbs,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (db_path_str, thread_id) = key
            .split_once(KEY_SEP)
            .context("malformed freebuff session key")?;
        let db_path = Path::new(db_path_str);
        let conn = open_ro(db_path)?;

        let (thread_project, raw_title, created_at, updated_at): (
            Option<String>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        ) = conn.query_row(
            "SELECT project_path, title, created_at, updated_at FROM threads WHERE id = ?1",
            params![thread_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;

        let project = thread_project
            .filter(|p| !p.trim().is_empty())
            .or_else(|| read_project_json(db_path))
            .unwrap_or_default();

        let started = created_at.and_then(ms_to_datetime);
        let ended = updated_at.and_then(ms_to_datetime).or(started);

        let mut mstmt = conn.prepare(
            "SELECT seq, role, parts_json, ts FROM messages WHERE thread_id = ?1 ORDER BY seq ASC",
        )?;

        let mut messages: Vec<Message> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        let mut edits: Vec<EditEvent> = Vec::new();

        let mrows = mstmt.query_map(params![thread_id], |r| {
            let seq: i64 = r.get(0)?;
            let role: String = r.get(1)?;
            let parts_json: String = r.get(2)?;
            let ts: Option<i64> = r.get(3)?;
            Ok((seq, role, parts_json, ts))
        })?;

        for (_, role_str, parts_json, ts_val) in mrows.flatten() {
            let ts = ts_val.and_then(ms_to_datetime);
            let Ok(parts) = serde_json::from_str::<Value>(&parts_json) else {
                continue;
            };

            let Some(parts_array) = parts.as_array() else {
                continue;
            };

            if role_str == "user" {
                let mut text_acc = Vec::new();
                for part in parts_array {
                    if part.get("kind").and_then(Value::as_str) == Some("text") {
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                text_acc.push(trimmed);
                            }
                        }
                    }
                }
                if !text_acc.is_empty() {
                    messages.push(Message {
                        role: Role::User,
                        text: text_acc.join("\n"),
                        ts,
                        tool: None,
                        is_rewound: false,
                    });
                }
            } else if role_str == "assistant" {
                for part in parts_array {
                    let kind = part.get("kind").and_then(Value::as_str).unwrap_or("");
                    match kind {
                        "text" => {
                            if let Some(t) = part.get("text").and_then(Value::as_str) {
                                let trimmed = t.trim();
                                if !trimmed.is_empty() {
                                    messages.push(Message {
                                        role: Role::Assistant,
                                        text: trimmed.to_string(),
                                        ts,
                                        tool: None,
                                        is_rewound: false,
                                    });
                                }
                            }
                        }
                        "tool" => {
                            let tool_name = part
                                .get("toolName")
                                .and_then(Value::as_str)
                                .unwrap_or("tool");
                            let call_id = part
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty())
                                .map(str::to_owned);
                            let input = part.get("input");

                            extract_tool_edits(tool_name, input, ts, &mut touched, &mut edits);

                            let mut args = input.cloned().unwrap_or_else(|| serde_json::json!({}));
                            redact_json_strings(&mut args);
                            messages.push(Message::tool_call(
                                ts,
                                ToolCall {
                                    id: call_id,
                                    name: tool_name.to_owned(),
                                    args,
                                },
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }

        let title = raw_title
            .as_deref()
            .filter(|t| !t.trim().is_empty() && *t != "New thread")
            .map(|t| redacted_truncate(t, 80))
            .unwrap_or_else(|| title_from_messages(&messages));

        Ok(Session {
            id: short_id(&display_path(key)),
            tool: self.name(),
            path: PathBuf::from(display_path(key)),
            project,
            started,
            ended,
            title,
            subagent: false,
            parent_id: None,
            messages,
            touched: dedup_paths(touched),
            edits,
        })
    }
}

fn read_project_json(db_path: &Path) -> Option<String> {
    let parent = db_path.parent()?;
    let pjson = parent.join("project.json");
    if !pjson.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&pjson).ok()?;
    let val: Value = serde_json::from_str(&content).ok()?;
    val.get("projectPath")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn extract_tool_edits(
    tool_name: &str,
    input: Option<&Value>,
    ts: Option<DateTime<Utc>>,
    touched: &mut Vec<String>,
    edits: &mut Vec<EditEvent>,
) {
    let Some(input) = input else {
        return;
    };

    match tool_name {
        "write_file" => {
            if let Some(p) = input.get("path").and_then(Value::as_str) {
                if let Some(cp) = clean_path(p) {
                    touched.push(cp.clone());
                    let content = input.get("content").and_then(Value::as_str).unwrap_or("");
                    let snippet = redacted_truncate(content.trim(), SNIPPET_CAP);
                    edits.push(EditEvent {
                        path: cp,
                        kind: EditKind::Write,
                        snippet,
                        ts,
                    });
                }
            }
        }
        "str_replace" => {
            if let Some(p) = input.get("path").and_then(Value::as_str) {
                if let Some(cp) = clean_path(p) {
                    touched.push(cp.clone());
                    let snippet = input
                        .get("replacements")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first())
                        .and_then(|r| r.get("newString"))
                        .and_then(Value::as_str)
                        .map(|s| redacted_truncate(s.trim(), SNIPPET_CAP))
                        .unwrap_or_default();
                    edits.push(EditEvent {
                        path: cp,
                        kind: EditKind::Edit,
                        snippet,
                        ts,
                    });
                }
            }
        }
        name if name.contains("write") || name.contains("edit") || name.contains("patch") => {
            for key in ["path", "file_path", "filePath"] {
                if let Some(p) = input.get(key).and_then(Value::as_str) {
                    if let Some(cp) = clean_path(p) {
                        touched.push(cp);
                        break;
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_freebuff_db_fixture() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("desktop-v2.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                project_path TEXT NOT NULL,
                title TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE messages (
                seq INTEGER PRIMARY KEY,
                thread_id TEXT NOT NULL,
                role TEXT NOT NULL,
                parts_json TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            INSERT INTO threads (id, project_path, title, created_at, updated_at)
            VALUES ('t1', '/my/project', 'Refactor auth', 1700000000000, 1700000010000);
            INSERT INTO messages (seq, thread_id, role, parts_json, ts)
            VALUES
            (1, 't1', 'user', '[{\"kind\":\"text\",\"text\":\"Please fix bug in login\"}]', 1700000001000),
            (2, 't1', 'assistant', '[{\"kind\":\"tool\",\"id\":\"call_1\",\"toolName\":\"write_file\",\"input\":{\"path\":\"src/login.rs\",\"content\":\"pub fn login() {}\"}}]', 1700000002000),
            (3, 't1', 'assistant', '[{\"kind\":\"tool\",\"id\":\"call_2\",\"toolName\":\"str_replace\",\"input\":{\"path\":\"src/login.rs\",\"replacements\":[{\"oldString\":\"pub fn login() {}\",\"newString\":\"pub fn login() -> bool { true }\"}]}}]', 1700000004000),
            (4, 't1', 'assistant', '[{\"kind\":\"text\",\"text\":\"I have fixed the login function.\"}]', 1700000005000);",
        )
        .unwrap();

        let key = make_key(&db_path, "t1");
        let adapter = Freebuff;
        let session = adapter.parse_key(&key).unwrap();

        assert_eq!(session.tool, "freebuff");
        assert_eq!(session.project, "/my/project");
        assert_eq!(session.title, "Refactor auth");
        assert_eq!(session.touched, vec!["src/login.rs"]);
        assert_eq!(session.edits.len(), 2);
        assert_eq!(session.edits[0].kind, EditKind::Write);
        assert_eq!(session.edits[0].path, "src/login.rs");
        assert_eq!(session.edits[0].snippet, "pub fn login() {}");
        assert_eq!(session.edits[1].kind, EditKind::Edit);
        assert_eq!(session.edits[1].path, "src/login.rs");
        assert_eq!(session.edits[1].snippet, "pub fn login() -> bool { true }");

        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "Please fix bug in login");
        assert_eq!(session.messages[1].role, Role::Tool);
        assert!(session.messages[1].tool.is_some());
        assert_eq!(session.messages[2].role, Role::Tool);
        assert!(session.messages[2].tool.is_some());
        assert_eq!(session.messages[3].role, Role::Assistant);
        assert_eq!(session.messages[3].text, "I have fixed the login function.");
    }

    #[test]
    fn discovers_real_freebuff_databases_if_present() {
        let adapter = Freebuff;
        if let Some(store) = adapter.store() {
            assert!(!store.had_error);
            if !store.keys.is_empty() {
                let session = adapter.parse_key(&store.keys[0].0).unwrap();
                assert_eq!(session.tool, "freebuff");
                assert!(!session.title.is_empty());
            }
        }
    }
}
