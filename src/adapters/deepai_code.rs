use super::{
    bounded_redacted_output, clean_path, dedup_paths, parse_ts, redacted_truncate,
    title_from_messages, Adapter, Discovered, Store,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall, ToolResult};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub struct DeepAiCode;

const KEY_SEP: char = '\u{1f}';
const SNIPPET_CAP: usize = 200;

fn make_key(db: &Path, id: &str) -> String {
    format!("{}{KEY_SEP}{id}", db.display())
}

fn display_path(key: &str) -> String {
    key.replace(KEY_SEP, "#")
}

fn open_ro(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .or_else(|_| {
        let uri = format!("file:{}?immutable=1", path.display());
        Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
    })
    .with_context(|| format!("open sqlite db {}", path.display()))
}

fn db_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();

    if let Some(h) = std::env::var_os("DEEPAI_CODE_HOME")
        .or_else(|| std::env::var_os("DEEPAI_HOME"))
        .or_else(|| std::env::var_os("T3CODE_HOME"))
    {
        let p = PathBuf::from(h);
        for cand in [
            p.join("userdata").join("statev2.sqlite"),
            p.join("statev2.sqlite"),
        ] {
            if cand.is_file() && !out.contains(&cand) {
                out.push(cand);
            }
        }
    }

    if let Some(home) = crate::util::home_dir() {
        for cand in [
            home.join(".deepai-code")
                .join("userdata")
                .join("statev2.sqlite"),
            home.join(".deepai-code").join("statev2.sqlite"),
        ] {
            if cand.is_file() && !out.contains(&cand) {
                out.push(cand);
            }
        }
    }

    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let p = PathBuf::from(appdata);
        for cand in [
            p.join("deepai-code-v2")
                .join("userdata")
                .join("statev2.sqlite"),
            p.join("deepai-code-v2").join("statev2.sqlite"),
            p.join("DeepAI Code")
                .join("userdata")
                .join("statev2.sqlite"),
            p.join("DeepAI Code").join("statev2.sqlite"),
        ] {
            if cand.is_file() && !out.contains(&cand) {
                out.push(cand);
            }
        }
    }

    out
}

impl Adapter for DeepAiCode {
    fn name(&self) -> &'static str {
        "deepai-code"
    }

    fn root(&self) -> Option<PathBuf> {
        db_paths()
            .into_iter()
            .next()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    fn reconcile_scope(&self) -> Option<String> {
        super::root_scope(self.root().as_deref())
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, _path: &Path) -> Result<Session> {
        anyhow::bail!("deepai-code sessions are indexed from sqlite store, not direct files")
    }

    fn store(&self) -> Option<Store> {
        let dbs = db_paths();
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

            let mut found_v2 = false;
            if let Ok(mut stmt) = conn.prepare(
                "SELECT thread_id, updated_at, created_at FROM orchestration_v2_projection_threads WHERE deleted_at IS NULL",
            ) {
                let rows = stmt.query_map([], |r| {
                    let id: String = r.get(0)?;
                    let updated: Option<String> = r.get(1)?;
                    let created: Option<String> = r.get(2)?;
                    Ok((id, updated.or(created)))
                });
                if let Ok(rows) = rows {
                    for (id, ts_str) in rows.flatten() {
                        found_v2 = true;
                        let token = ts_str
                            .as_deref()
                            .and_then(parse_ts)
                            .map(|t| t.timestamp_millis())
                            .unwrap_or(0);
                        keys.push((make_key(db, &id), token));
                    }
                } else {
                    had_error = true;
                }
            }

            if !found_v2 {
                if let Ok(mut stmt) = conn.prepare(
                    "SELECT thread_id, updated_at, created_at FROM projection_threads WHERE deleted_at IS NULL",
                ) {
                    if let Ok(rows) = stmt.query_map([], |r| {
                        let id: String = r.get(0)?;
                        let updated: Option<String> = r.get(1)?;
                        let created: Option<String> = r.get(2)?;
                        Ok((id, updated.or(created)))
                    }) {
                        for (id, ts_str) in rows.flatten() {
                            let token = ts_str
                                .as_deref()
                                .and_then(parse_ts)
                                .map(|t| t.timestamp_millis())
                                .unwrap_or(0);
                            keys.push((make_key(db, &id), token));
                        }
                    }
                }
            }
        }

        Some(Store {
            keys,
            files: dbs,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (db, sid) = key.split_once(KEY_SEP).context("malformed deepai-code key")?;
        let conn = open_ro(Path::new(db)).with_context(|| format!("open {db}"))?;

        let mut projects: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
        if let Ok(mut pstmt) =
            conn.prepare("SELECT project_id, title, workspace_root FROM projection_projects")
        {
            if let Ok(prows) = pstmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ),
                ))
            }) {
                for (pid, info) in prows.flatten() {
                    projects.insert(pid, info);
                }
            }
        }

        let mut project_id = None;
        let mut title = None;
        let mut created_at = None;
        let mut updated_at = None;
        let mut worktree_path = None;
        let mut forked_from = None;

        if let Ok((pid, t, c, u, payload_str)) = conn.query_row(
            "SELECT project_id, title, created_at, updated_at, payload_json \
             FROM orchestration_v2_projection_threads WHERE thread_id = ?1",
            params![sid],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        ) {
            project_id = pid;
            title = t;
            created_at = c;
            updated_at = u;
            if let Some(pj_str) = payload_str {
                if let Ok(pj) = serde_json::from_str::<Value>(&pj_str) {
                    worktree_path = pj
                        .get("worktreePath")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    forked_from = pj
                        .get("forkedFrom")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
        } else if let Ok((pid, t, c, u, wt)) = conn.query_row(
            "SELECT project_id, title, created_at, updated_at, worktree_path \
             FROM projection_threads WHERE thread_id = ?1",
            params![sid],
            |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        ) {
            project_id = pid;
            title = t;
            created_at = c;
            updated_at = u;
            worktree_path = wt;
        }

        let project = worktree_path
            .filter(|p| !p.trim().is_empty())
            .or_else(|| {
                project_id.as_ref().and_then(|pid| {
                    projects.get(pid).and_then(|info| {
                        info.1
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .or_else(|| {
                                info.0
                                    .as_deref()
                                    .filter(|s| !s.trim().is_empty() && *s != "No project")
                            })
                            .map(str::to_owned)
                    })
                })
            })
            .unwrap_or_else(|| "deepai-code".to_string());

        let started = created_at.as_deref().and_then(parse_ts);
        let ended = updated_at.as_deref().and_then(parse_ts).or(started);
        let parent_id = forked_from.filter(|s| !s.trim().is_empty());
        let subagent = parent_id.is_some();

        let mut messages: Vec<Message> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        let mut edits: Vec<EditEvent> = Vec::new();

        if let Ok(mut stmt) = conn.prepare(
            "SELECT turn_item_id, type, ordinal, payload_json \
             FROM orchestration_v2_projection_turn_items \
             WHERE thread_id = ?1 ORDER BY ordinal ASC",
        ) {
            if let Ok(rows) = stmt.query_map(params![sid], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                ))
            }) {
                for (item_id, itype, _ordinal, pj_str) in rows.flatten() {
                    let Ok(pj) = serde_json::from_str::<Value>(&pj_str) else {
                        continue;
                    };
                    let ts = pj
                        .get("startedAt")
                        .or_else(|| pj.get("createdAt"))
                        .or_else(|| pj.get("updatedAt"))
                        .and_then(Value::as_str)
                        .and_then(parse_ts);

                    match itype.as_str() {
                        "user_message" => {
                            if let Some(text) = pj.get("text").and_then(Value::as_str) {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    messages.push(Message::plain(Role::User, trimmed, ts));
                                }
                            }
                        }
                        "assistant_message" | "reasoning" => {
                            if let Some(text) = pj.get("text").and_then(Value::as_str) {
                                let trimmed = text.trim();
                                if !trimmed.is_empty() {
                                    messages.push(Message::plain(Role::Assistant, trimmed, ts));
                                }
                            }
                        }
                        "command_execution" => {
                            let input = pj
                                .get("input")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            let output = pj
                                .get("output")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            let exit_code = pj.get("exitCode").and_then(Value::as_i64);
                            let call_id = Some(item_id.clone());

                            messages.push(Message::tool_call(
                                ts,
                                ToolCall {
                                    id: call_id.clone(),
                                    name: "command".to_string(),
                                    args: serde_json::json!({ "command": input }),
                                },
                            ));

                            let is_error = exit_code.is_some_and(|c| c != 0);
                            messages.push(Message::tool_result(
                                ts,
                                ToolResult {
                                    call_id,
                                    text: bounded_redacted_output(output),
                                    is_error,
                                },
                            ));
                        }
                        "file_change" => {
                            let file_name = pj
                                .get("fileName")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            let diff_str = pj
                                .get("diffStr")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            let call_id = Some(item_id.clone());

                            if let Some(cp) = clean_path(file_name) {
                                touched.push(cp.clone());
                                edits.push(EditEvent {
                                    path: cp,
                                    kind: EditKind::Edit,
                                    snippet: redacted_truncate(diff_str, SNIPPET_CAP),
                                    ts,
                                });
                            }

                            messages.push(Message::tool_call(
                                ts,
                                ToolCall {
                                    id: call_id.clone(),
                                    name: "file_change".to_string(),
                                    args: serde_json::json!({
                                        "path": file_name,
                                        "diff": diff_str
                                    }),
                                },
                            ));

                            messages.push(Message::tool_result(
                                ts,
                                ToolResult {
                                    call_id,
                                    text: "File change applied".to_string(),
                                    is_error: false,
                                },
                            ));
                        }
                        "dynamic_tool" => {
                            let tool_name = pj
                                .get("toolName")
                                .and_then(Value::as_str)
                                .unwrap_or("tool");
                            let input = pj.get("input");
                            let output = pj
                                .get("output")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .trim();
                            let status = pj.get("status").and_then(Value::as_str);
                            let call_id = Some(item_id.clone());

                            if let Some(inp) = input {
                                if let Some(cp) = inp
                                    .get("path")
                                    .or_else(|| inp.get("file_path"))
                                    .and_then(Value::as_str)
                                    .and_then(clean_path)
                                {
                                    touched.push(cp);
                                }
                            }

                            messages.push(Message::tool_call(
                                ts,
                                ToolCall {
                                    id: call_id.clone(),
                                    name: tool_name.to_string(),
                                    args: input.cloned().unwrap_or(serde_json::json!({})),
                                },
                            ));

                            let is_error = status == Some("failed");
                            messages.push(Message::tool_result(
                                ts,
                                ToolResult {
                                    call_id,
                                    text: bounded_redacted_output(output),
                                    is_error,
                                },
                            ));
                        }
                        _ => {}
                    }
                }
            }
        }

        if messages.is_empty() {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT role, created_at, payload_json \
                 FROM orchestration_v2_projection_messages \
                 WHERE thread_id = ?1 ORDER BY created_at ASC",
            ) {
                if let Ok(rows) = stmt.query_map(params![sid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                }) {
                    for (role_str, c_ts, pj_str) in rows.flatten() {
                        let Ok(pj) = serde_json::from_str::<Value>(&pj_str) else {
                            continue;
                        };
                        let text = pj.get("text").and_then(Value::as_str).unwrap_or("").trim();
                        if text.is_empty() {
                            continue;
                        }
                        let ts = c_ts.as_deref().and_then(parse_ts);
                        let role = if role_str == "user" {
                            Role::User
                        } else {
                            Role::Assistant
                        };
                        messages.push(Message::plain(role, text, ts));
                    }
                }
            }
        }

        if messages.is_empty() {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT role, text, created_at FROM projection_thread_messages \
                 WHERE thread_id = ?1 ORDER BY created_at ASC",
            ) {
                if let Ok(rows) = stmt.query_map(params![sid], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                }) {
                    for (role_str, text_opt, c_ts) in rows.flatten() {
                        let text = text_opt.unwrap_or_default().trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        let ts = c_ts.as_deref().and_then(parse_ts);
                        let role = if role_str == "user" {
                            Role::User
                        } else {
                            Role::Assistant
                        };
                        messages.push(Message::plain(role, text, ts));
                    }
                }
            }
        }

        let computed_title = title_from_messages(&messages);
        let final_title = title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(computed_title);

        Ok(Session {
            id: sid.to_string(),
            tool: "deepai-code",
            path: PathBuf::from(display_path(key)),
            project,
            started,
            ended,
            title: final_title,
            subagent,
            parent_id,
            messages,
            touched: dedup_paths(touched),
            edits,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deepai_code_parse_fixture() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_file = temp_dir.path().join("statev2.sqlite");
        let conn = Connection::open(&db_file).unwrap();

        conn.execute_batch(
            "CREATE TABLE projection_projects (
                project_id TEXT PRIMARY KEY,
                title TEXT,
                workspace_root TEXT
            );
            CREATE TABLE orchestration_v2_projection_threads (
                thread_id TEXT PRIMARY KEY,
                project_id TEXT,
                title TEXT,
                created_at TEXT,
                updated_at TEXT,
                archived_at TEXT,
                deleted_at TEXT,
                payload_json TEXT
            );
            CREATE TABLE orchestration_v2_projection_turn_items (
                turn_item_id TEXT PRIMARY KEY,
                thread_id TEXT,
                type TEXT,
                ordinal INTEGER,
                payload_json TEXT
            );",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO projection_projects (project_id, title, workspace_root) \
             VALUES ('proj-1', 'My Project', 'D:/my_project')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO orchestration_v2_projection_threads \
             (thread_id, project_id, title, created_at, updated_at, deleted_at, payload_json) \
             VALUES ('thread-1', 'proj-1', 'Test DeepAI Session', '2026-11-10T10:00:00Z', '2026-11-10T10:05:00Z', NULL, '{}')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, thread_id, type, ordinal, payload_json) \
             VALUES ('item-1', 'thread-1', 'user_message', 1, '{\"text\": \"hello\", \"startedAt\": \"2026-11-10T10:00:00Z\"}')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, thread_id, type, ordinal, payload_json) \
             VALUES ('item-2', 'thread-1', 'command_execution', 2, '{\"input\": \"cargo build\", \"output\": \"Finished\", \"exitCode\": 0, \"startedAt\": \"2026-11-10T10:01:00Z\"}')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, thread_id, type, ordinal, payload_json) \
             VALUES ('item-3', 'thread-1', 'file_change', 3, '{\"fileName\": \"D:/my_project/src/lib.rs\", \"diffStr\": \"+fn test() {}\", \"startedAt\": \"2026-11-10T10:02:00Z\"}')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO orchestration_v2_projection_turn_items (turn_item_id, thread_id, type, ordinal, payload_json) \
             VALUES ('item-4', 'thread-1', 'assistant_message', 4, '{\"text\": \"All done!\", \"startedAt\": \"2026-11-10T10:03:00Z\"}')",
            [],
        )
        .unwrap();

        drop(conn);

        let adapter = DeepAiCode;
        let key = make_key(&db_file, "thread-1");
        let session = adapter.parse_key(&key).unwrap();

        assert_eq!(session.id, "thread-1");
        assert_eq!(session.tool, "deepai-code");
        assert_eq!(session.project, "D:/my_project");
        assert_eq!(session.title, "Test DeepAI Session");
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "hello");
        assert_eq!(session.messages[1].role, Role::Tool);
        assert_eq!(session.messages[2].role, Role::Tool);
        assert_eq!(session.messages[3].role, Role::Tool);
        assert_eq!(session.messages[4].role, Role::Tool);
        assert_eq!(session.messages[5].role, Role::Assistant);
        assert_eq!(session.messages[5].text, "All done!");
        assert_eq!(session.touched, vec!["D:/my_project/src/lib.rs"]);
        assert_eq!(session.edits.len(), 1);
        assert_eq!(session.edits[0].path, "D:/my_project/src/lib.rs");
    }
}
