use crate::adapters::{
    clean_path, dedup_paths, ok_or_flag, title_from_messages, Adapter, Discovered,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall, ToolResult};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct Kimi;

impl Adapter for Kimi {
    fn name(&self) -> &'static str {
        "kimi"
    }

    fn root(&self) -> Option<PathBuf> {
        if let Some(k) = std::env::var_os("KIMI_CODE_HOME") {
            let p = PathBuf::from(k);
            if p.is_absolute() {
                return Some(p);
            }
        }
        crate::util::home_dir().map(|h| h.join(".kimi-code"))
    }

    fn discover(&self) -> Discovered {
        let Some(root) = self.root() else {
            return Vec::new().into();
        };
        let sessions_dir = root.join("sessions");
        if !sessions_dir.is_dir() {
            return Vec::new().into();
        }
        let mut files = Vec::new();
        let mut had_error = false;
        for entry in walkdir::WalkDir::new(&sessions_dir)
            .into_iter()
            .filter_map(|e| ok_or_flag(e, &mut had_error))
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if name == "wire.jsonl" || name == "session.jsonl" || name.ends_with(".jsonl") {
                files.push(entry.into_path());
            }
        }
        Discovered { files, had_error }
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let (raw_lines, _) = crate::util::session_lines_windowed(path, 10_000_000, 256, 256)
            .with_context(|| format!("read kimi session {}", path.display()))?;

        // Check companion state.json in the same folder if present
        let mut state_cwd: Option<String> = None;
        let mut state_title: Option<String> = None;
        let mut state_id: Option<String> = None;

        if let Some(parent_dir) = path.parent() {
            let state_file = parent_dir.join("state.json");
            if state_file.is_file() {
                if let Ok(state_raw) = std::fs::read_to_string(&state_file) {
                    if let Ok(sval) = serde_json::from_str::<Value>(&state_raw) {
                        state_cwd = sval
                            .get("cwd")
                            .or_else(|| sval.get("work_dir"))
                            .and_then(Value::as_str)
                            .map(String::from);
                        state_title = sval.get("title").and_then(Value::as_str).map(String::from);
                        state_id = sval
                            .get("id")
                            .or_else(|| sval.get("session_id"))
                            .and_then(Value::as_str)
                            .map(String::from);
                    }
                }
            }
        }

        let mut started: Option<DateTime<Utc>> = None;
        let mut ended: Option<DateTime<Utc>> = None;
        let mut messages = Vec::new();
        let mut touched = Vec::new();
        let mut edits = Vec::new();
        let mut subagent = false;
        let mut parent_id: Option<String> = None;

        for line in raw_lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            let time = v
                .get("timestamp")
                .or_else(|| v.get("time"))
                .or_else(|| v.get("created_at"))
                .and_then(parse_datetime);
            if time.is_some() {
                ended = time;
                if started.is_none() {
                    started = time;
                }
            }

            if let Some(p) = v
                .get("parent_id")
                .or_else(|| v.get("parentSessionId"))
                .and_then(Value::as_str)
            {
                if !p.is_empty() {
                    parent_id = Some(p.to_string());
                    subagent = true;
                }
            }

            let role_str = v
                .get("role")
                .or_else(|| v.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");

            match role_str {
                "user" | "user_message" => {
                    let text = v
                        .get("content")
                        .or_else(|| v.get("text"))
                        .and_then(extract_text)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        messages.push(Message {
                            role: Role::User,
                            text,
                            ts: time,
                            tool: None,
                            is_rewound: false,
                        });
                    }
                }
                "assistant" | "assistant_message" => {
                    let text = v
                        .get("content")
                        .or_else(|| v.get("text"))
                        .and_then(extract_text)
                        .unwrap_or_default();
                    if !text.is_empty() {
                        messages.push(Message {
                            role: Role::Assistant,
                            text,
                            ts: time,
                            tool: None,
                            is_rewound: false,
                        });
                    }

                    if let Some(calls) = v.get("tool_calls").and_then(Value::as_array) {
                        for call in calls {
                            let fn_obj = call.get("function").unwrap_or(call);
                            let name = fn_obj.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let call_id = call.get("id").and_then(Value::as_str).map(String::from);
                            let args = if let Some(arg_str) =
                                fn_obj.get("arguments").and_then(Value::as_str)
                            {
                                serde_json::from_str::<Value>(arg_str).unwrap_or(Value::Null)
                            } else {
                                fn_obj
                                    .get("arguments")
                                    .or_else(|| fn_obj.get("args"))
                                    .cloned()
                                    .unwrap_or(Value::Null)
                            };

                            extract_edits_from_tool(name, &args, time, &mut touched, &mut edits);

                            messages.push(Message::tool_call(
                                time,
                                ToolCall {
                                    id: call_id,
                                    name: name.to_string(),
                                    args,
                                },
                            ));
                        }
                    }
                }
                "tool" | "tool_result" => {
                    let call_id = v
                        .get("tool_call_id")
                        .or_else(|| v.get("id"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let text = v
                        .get("content")
                        .or_else(|| v.get("output"))
                        .and_then(extract_text)
                        .unwrap_or_default();
                    let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);

                    messages.push(Message::tool_result(
                        time,
                        ToolResult {
                            call_id,
                            text,
                            is_error,
                        },
                    ));
                }
                _ => {}
            }
        }

        let computed_title = title_from_messages(&messages);
        let final_title = state_title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or(computed_title);
        let sid = state_id.unwrap_or_else(|| short_id(&path.to_string_lossy()));

        Ok(Session {
            id: sid,
            tool: self.name(),
            path: path.to_path_buf(),
            project: state_cwd.unwrap_or_default(),
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

fn parse_datetime(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(ts_num) = v.as_i64() {
        if ts_num > 10_000_000_000 {
            DateTime::from_timestamp_millis(ts_num)
        } else {
            DateTime::from_timestamp(ts_num, 0)
        }
    } else if let Some(s) = v.as_str() {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    } else {
        None
    }
}

fn extract_text(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = v.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|p| {
                if let Some(s) = p.as_str() {
                    Some(s.to_string())
                } else {
                    p.get("text")
                        .and_then(Value::as_str)
                        .map(|text| text.to_string())
                }
            })
            .collect();
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }
    None
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
    fn test_kimi_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions/kimi-001");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"id":"kimi-001","cwd":"/work/kimi-proj","title":"Optimize tokenizer"}"#,
        )
        .unwrap();

        let wire_file = session_dir.join("wire.jsonl");
        let content = r#"{"role":"user","content":"Optimize the BPE tokenizer loop","timestamp":1700000000}
{"role":"assistant","content":"I will inspect the loop.","tool_calls":[{"id":"call_1","function":{"name":"write_file","arguments":"{\"path\":\"src/bpe.rs\",\"content\":\"// optimized\"}"}}],"timestamp":1700000001}
{"role":"tool","tool_call_id":"call_1","content":"Saved src/bpe.rs","timestamp":1700000002}
{"role":"assistant","content":"Done optimizing.","timestamp":1700000003}
"#;
        std::fs::write(&wire_file, content).unwrap();

        let session = Kimi.parse(&wire_file).unwrap();
        assert_eq!(session.id, "kimi-001");
        assert_eq!(session.project, "/work/kimi-proj");
        assert_eq!(session.title, "Optimize tokenizer");
        assert_eq!(session.messages.len(), 5);
        assert_eq!(session.touched, vec!["src/bpe.rs"]);
        assert_eq!(session.edits.len(), 1);
    }
}
