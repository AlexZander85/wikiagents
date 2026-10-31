use crate::adapters::{
    clean_path, dedup_paths, ok_or_flag, title_from_messages, Adapter, Discovered,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall, ToolResult};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub struct DeepSeek;

impl Adapter for DeepSeek {
    fn name(&self) -> &'static str {
        "deepseek"
    }

    fn root(&self) -> Option<PathBuf> {
        if let Some(dsh_home) = std::env::var_os("DSH_HOME") {
            let p = PathBuf::from(dsh_home);
            if p.is_absolute() {
                return Some(p.join("sessions"));
            }
        }
        crate::util::home_dir().map(|h| h.join(".dsh").join("sessions"))
    }

    fn discover(&self) -> Discovered {
        let Some(root) = self.root() else {
            return Vec::new().into();
        };
        if !root.is_dir() {
            return Vec::new().into();
        }
        let mut files = Vec::new();
        let mut had_error = false;
        for entry in walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| ok_or_flag(e, &mut had_error))
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if is_dsh_session_file(&name) {
                files.push(entry.into_path());
            }
        }
        Discovered { files, had_error }
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let (raw_lines, _) = crate::util::session_lines_windowed(path, 10_000_000, 256, 256)
            .with_context(|| format!("read deepseek session {}", path.display()))?;

        let mut session_id: Option<String> = None;
        let mut project: Option<String> = None;
        let mut started: Option<DateTime<Utc>> = None;
        let mut ended: Option<DateTime<Utc>> = None;
        let mut parent_id: Option<String> = None;
        let mut subagent = false;
        let mut messages = Vec::new();
        let mut touched = Vec::new();
        let mut edits = Vec::new();

        for line in raw_lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            let entry_type = v.get("type").and_then(Value::as_str).unwrap_or("");

            // 1. Header line
            if entry_type == "header"
                || entry_type == "session_header"
                || (session_id.is_none() && v.get("id").is_some() && v.get("cwd").is_some())
            {
                if let Some(id_val) = v.get("id").and_then(Value::as_str) {
                    session_id = Some(id_val.to_string());
                }
                if let Some(cwd_val) = v.get("cwd").and_then(Value::as_str) {
                    project = Some(cwd_val.to_string());
                }
                if let Some(created_val) = v
                    .get("createdAt")
                    .or_else(|| v.get("created_at"))
                    .or_else(|| v.get("time"))
                {
                    started = parse_datetime(created_val);
                }
                if let Some(parent_val) = v
                    .get("parentSession")
                    .or_else(|| v.get("parent_id"))
                    .or_else(|| v.get("parentSessionId"))
                    .and_then(Value::as_str)
                {
                    if !parent_val.is_empty() {
                        parent_id = Some(parent_val.to_string());
                        subagent = true;
                    }
                }
                if v.get("delegationDepth")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    > 0
                {
                    subagent = true;
                }
                continue;
            }

            let time = v
                .get("time")
                .or_else(|| v.get("timestamp"))
                .and_then(parse_datetime);
            if time.is_some() {
                ended = time;
                if started.is_none() {
                    started = time;
                }
            }

            let data = v.get("data").unwrap_or(&v);

            match entry_type {
                "user/message" | "user_message" => {
                    let text = data
                        .get("content")
                        .or_else(|| data.get("text"))
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
                "assistant/message" | "assistant_message" => {
                    let text = data
                        .get("content")
                        .or_else(|| data.get("text"))
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
                }
                "tool/call" | "tool_call" => {
                    let name = data
                        .get("name")
                        .or_else(|| data.get("tool"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let call_id = data
                        .get("call_id")
                        .or_else(|| data.get("id"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let args = data
                        .get("input")
                        .or_else(|| data.get("arguments"))
                        .or_else(|| data.get("args"))
                        .cloned()
                        .unwrap_or(Value::Null);

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
                "tool/result" | "tool_result" => {
                    let call_id = data
                        .get("call_id")
                        .or_else(|| data.get("id"))
                        .and_then(Value::as_str)
                        .map(String::from);
                    let text = data
                        .get("output")
                        .or_else(|| data.get("content"))
                        .and_then(extract_text)
                        .unwrap_or_default();
                    let is_error = data
                        .get("is_error")
                        .or_else(|| data.get("isError"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);

                    messages.push(Message::tool_result(
                        time,
                        ToolResult {
                            call_id,
                            text,
                            is_error,
                        },
                    ));
                }
                _ => {
                    // Generic role check if format is plain openai/anthropic style JSONL
                    if let Some(role_str) = v.get("role").and_then(Value::as_str) {
                        let text = v.get("content").and_then(extract_text).unwrap_or_default();
                        match role_str {
                            "user" => {
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
                            "assistant" => {
                                if !text.is_empty() {
                                    messages.push(Message {
                                        role: Role::Assistant,
                                        text,
                                        ts: time,
                                        tool: None,
                                        is_rewound: false,
                                    });
                                }
                            }
                            "tool" => {
                                messages.push(Message::tool_result(
                                    time,
                                    ToolResult {
                                        call_id: v
                                            .get("tool_call_id")
                                            .and_then(Value::as_str)
                                            .map(String::from),
                                        text,
                                        is_error: false,
                                    },
                                ));
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let sid = session_id.unwrap_or_else(|| short_id(&path.to_string_lossy()));
        let title = title_from_messages(&messages);

        Ok(Session {
            id: sid,
            tool: self.name(),
            path: path.to_path_buf(),
            project: project.unwrap_or_default(),
            started,
            ended,
            title,
            subagent,
            parent_id,
            messages,
            touched: dedup_paths(touched),
            edits,
        })
    }
}

fn is_dsh_session_file(name: &str) -> bool {
    let clean = name
        .strip_suffix(".zst")
        .or_else(|| name.strip_suffix(".zstd"))
        .unwrap_or(name);
    clean == "session.jsonl"
        || (clean.starts_with("session.v") && clean.ends_with(".jsonl"))
        || clean.ends_with(".jsonl")
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
        "edit_file" | "Edit" | "str_replace_editor" => {
            let snip = args
                .get("new_str")
                .or_else(|| args.get("new_string"))
                .or_else(|| args.get("replacement"))
                .and_then(Value::as_str)
                .unwrap_or("");
            (EditKind::Edit, snip.to_string())
        }
        "write_file" | "Write" => {
            let snip = args
                .get("content")
                .or_else(|| args.get("contents"))
                .and_then(Value::as_str)
                .unwrap_or("");
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
    fn test_deepseek_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.v1.jsonl");
        let content = r#"{"type":"header","id":"dsh-001","cwd":"/work/deepseek-proj","createdAt":1700000000}
{"type":"user/message","time":1700000001,"data":{"content":"Implement the kv-cache layer"}}
{"type":"tool/call","time":1700000002,"data":{"name":"write_file","input":{"path":"src/cache.rs","content":"pub struct KvCache;"}}}
{"type":"tool/result","time":1700000003,"data":{"output":"File created"}}
{"type":"assistant/message","time":1700000004,"data":{"content":"KvCache has been implemented."}}
"#;
        std::fs::write(&file, content).unwrap();

        let session = DeepSeek.parse(&file).unwrap();
        assert_eq!(session.id, "dsh-001");
        assert_eq!(session.project, "/work/deepseek-proj");
        assert_eq!(session.title, "Implement the kv-cache layer");
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.touched, vec!["src/cache.rs"]);
        assert_eq!(session.edits.len(), 1);
        assert_eq!(session.edits[0].kind, EditKind::Write);
    }
}
