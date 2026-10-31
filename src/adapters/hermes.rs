use crate::adapters::{clean_path, dedup_paths, title_from_messages, Adapter, Store};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Hermes;

const KEY_SEP: char = '\u{1f}';

/// The session header row: title, cwd, start and end stamps, parent id.
type SessionMeta = (
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .context("open hermes sqlite db")
}

fn make_key(db: &Path, id: &str) -> String {
    format!("{}{KEY_SEP}{id}", db.display())
}

fn display_path(key: &str) -> String {
    key.replace(KEY_SEP, "#")
}

impl Adapter for Hermes {
    fn name(&self) -> &'static str {
        "hermes"
    }

    fn root(&self) -> Option<PathBuf> {
        if let Some(h) = std::env::var_os("HERMES_HOME") {
            let p = PathBuf::from(h);
            if p.is_absolute() {
                return Some(p);
            }
        }
        #[cfg(windows)]
        {
            if let Some(appdata) = std::env::var_os("LOCALAPPDATA") {
                return Some(PathBuf::from(appdata).join("hermes"));
            }
        }
        crate::util::home_dir().map(|h| h.join(".hermes"))
    }

    fn discover(&self) -> crate::adapters::Discovered {
        Vec::new().into()
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("hermes sessions are indexed from sqlite store, not direct files")
    }

    fn store(&self) -> Option<Store> {
        let root = self.root()?;
        if !root.is_dir() {
            return None;
        }

        let mut dbs = Vec::new();
        let default_db = root.join("state.db");
        if default_db.is_file() {
            dbs.push(default_db);
        }

        let profiles_dir = root.join("profiles");
        if profiles_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&profiles_dir) {
                for entry in entries.flatten() {
                    let pdb = entry.path().join("state.db");
                    if pdb.is_file() {
                        dbs.push(pdb);
                    }
                }
            }
        }

        if dbs.is_empty() {
            return None;
        }

        let mut keys = Vec::new();
        let mut had_error = false;

        for db in &dbs {
            let conn = match open_ro(db) {
                Ok(c) => c,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            let mut stmt = match conn.prepare("SELECT id, ended_at, started_at FROM sessions") {
                Ok(s) => s,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            let rows = match stmt.query_map([], |r| {
                let id: String = r.get(0)?;
                let ended: Option<i64> = r.get(1)?;
                let started: Option<i64> = r.get(2)?;
                let token = ended.or(started).unwrap_or(0);
                Ok((id, token))
            }) {
                Ok(rows) => rows,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };

            for (id, token) in rows.flatten() {
                keys.push((make_key(db, &id), token));
            }
        }

        Some(Store {
            keys,
            files: dbs,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (db, sid) = key.split_once(KEY_SEP).context("malformed hermes key")?;
        let conn = open_ro(Path::new(db))?;

        let (title, cwd, started_ts, ended_ts, parent_id): SessionMeta = conn.query_row(
            "SELECT title, cwd, started_at, ended_at, parent_session_id FROM sessions WHERE id = ?1",
            params![sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;

        let started = started_ts.and_then(ms_or_s_to_datetime);
        let ended = ended_ts.and_then(ms_or_s_to_datetime).or(started);
        let subagent = parent_id.as_ref().is_some_and(|p| !p.trim().is_empty());

        let has_active_col = conn.prepare("SELECT active FROM messages LIMIT 0").is_ok();
        let query = if has_active_col {
            "SELECT role, content, tool_calls, timestamp, active FROM messages WHERE session_id = ?1 ORDER BY rowid ASC"
        } else {
            "SELECT role, content, tool_calls, timestamp, 1 FROM messages WHERE session_id = ?1 ORDER BY rowid ASC"
        };
        let mut mstmt = conn.prepare(query)?;

        let mut messages = Vec::new();
        let mut touched = Vec::new();
        let mut edits = Vec::new();

        let mrows = mstmt.query_map(params![sid], |r| {
            let role: String = r.get(0)?;
            let content: Option<String> = r.get(1)?;
            let tool_calls: Option<String> = r.get(2)?;
            let ts: Option<i64> = r.get(3)?;
            let active: Option<i64> = r.get(4).ok();
            Ok((role, content, tool_calls, ts, active))
        })?;

        for (role_str, content, tool_calls_raw, ts_val, active) in mrows.flatten() {
            let is_rewound = active.is_some_and(|a| a == 0);
            let time = ts_val.and_then(ms_or_s_to_datetime);
            let role = match role_str.to_lowercase().as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::Tool,
            };

            let text = content.unwrap_or_default();
            if !text.is_empty() {
                messages.push(Message {
                    role,
                    text: text.clone(),
                    ts: time,
                    tool: None,
                    is_rewound,
                });
            }

            if let Some(raw_calls) = tool_calls_raw {
                if let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(&raw_calls) {
                    for call in calls {
                        let fn_obj = if call.get("function").is_some() {
                            call.get("function").unwrap()
                        } else {
                            &call
                        };
                        let name = fn_obj.get("name").and_then(Value::as_str).unwrap_or("tool");
                        let call_id = call
                            .get("id")
                            .or_else(|| call.get("call_id"))
                            .and_then(Value::as_str)
                            .map(String::from);
                        let args = fn_obj.get("arguments").cloned().unwrap_or(Value::Null);

                        extract_edits_from_tool(name, &args, time, &mut touched, &mut edits);

                        let mut msg = Message::tool_call(
                            time,
                            ToolCall {
                                id: call_id,
                                name: name.to_string(),
                                args,
                            },
                        );
                        msg.is_rewound = is_rewound;
                        messages.push(msg);
                    }
                }
            }
        }

        let computed_title = title_from_messages(&messages);
        let final_title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(computed_title);

        Ok(Session {
            id: short_id(key),
            tool: self.name(),
            path: PathBuf::from(display_path(key)),
            project: cwd.unwrap_or_default(),
            started,
            ended,
            title: final_title,
            subagent,
            parent_id: parent_id.filter(|p| !p.trim().is_empty()),
            messages,
            touched: dedup_paths(touched),
            edits,
        })
    }
}

fn ms_or_s_to_datetime(ts: i64) -> Option<DateTime<Utc>> {
    if ts > 10_000_000_000 {
        DateTime::from_timestamp_millis(ts)
    } else {
        DateTime::from_timestamp(ts, 0)
    }
}

fn extract_edits_from_tool(
    name: &str,
    args: &Value,
    time: Option<DateTime<Utc>>,
    touched: &mut Vec<String>,
    edits: &mut Vec<EditEvent>,
) {
    let path_val = args
        .get("file_path")
        .or_else(|| args.get("path"))
        .or_else(|| args.get("target_file"))
        .and_then(Value::as_str);

    let Some(raw_path) = path_val else { return };
    let Some(cleaned) = clean_path(raw_path) else {
        return;
    };
    touched.push(cleaned.clone());

    let (kind, snippet) = match name {
        "edit_file" | "Edit" => {
            let snip = args
                .get("new_str")
                .or_else(|| args.get("replacement"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (EditKind::Edit, snip.to_string())
        }
        "write_file" | "Write" => {
            let snip = args.get("content").and_then(Value::as_str).unwrap_or("");
            (EditKind::Write, snip.to_string())
        }
        _ => (EditKind::Write, String::new()),
    };

    edits.push(EditEvent {
        path: cleaned,
        kind,
        snippet,
        ts: time,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hermes_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("state.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                title TEXT,
                cwd TEXT,
                started_at INTEGER,
                ended_at INTEGER,
                parent_session_id TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                session_id TEXT,
                role TEXT,
                content TEXT,
                tool_calls TEXT,
                timestamp INTEGER
            );
            INSERT INTO sessions VALUES ('hermes-s1', 'Build hermes adapter', '/work/hermes', 1700000000, 1700000010, NULL);
            INSERT INTO messages VALUES (1, 'hermes-s1', 'user', 'Analyze system architecture', NULL, 1700000001);
            INSERT INTO messages VALUES (2, 'hermes-s1', 'assistant', 'Starting architecture analysis', NULL, 1700000002);
            ",
        ).unwrap();

        let key = make_key(&db_path, "hermes-s1");
        let session = Hermes.parse_key(&key).unwrap();
        assert_eq!(session.tool, "hermes");
        assert_eq!(session.project, "/work/hermes");
        assert_eq!(session.title, "Build hermes adapter");
        assert_eq!(session.messages.len(), 2);
    }
}
