use super::{
    bounded_redacted_output, clean_path, dedup_paths, ok_or_flag, parse_ts, redacted_truncate,
    title_from_messages, Adapter, Discovered,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall, ToolResult};
use crate::util::short_id;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const SNIPPET_CAP: usize = 200;

/// free-pi-cli is an ad-supported distribution of the "pi" coding agent
/// harness (built on @earendil-works/pi-coding-agent). It keeps session
/// transcripts under `~/.free-pi/agent/sessions/<encoded-cwd>/<iso>_<uuid>.jsonl`.
pub struct FreePiCli;

impl Adapter for FreePiCli {
    fn name(&self) -> &'static str {
        "free-pi-cli"
    }

    fn root(&self) -> Option<PathBuf> {
        free_pi_sessions_dirs().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        discover_sessions(&free_pi_sessions_dirs())
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        parse_jsonl(self.name(), path)
    }
}

/// Upstream pi coding agent harness (Mario Zechner / pi-mono).
/// Keeps sessions under `~/.pi/agent/sessions/<encoded-cwd>/<iso>_<uuid>.jsonl`.
pub struct Pi;

impl Adapter for Pi {
    fn name(&self) -> &'static str {
        "pi"
    }

    fn root(&self) -> Option<PathBuf> {
        pi_sessions_dirs().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        discover_sessions(&pi_sessions_dirs())
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        parse_jsonl(self.name(), path)
    }
}

fn discover_sessions(dirs: &[PathBuf]) -> Discovered {
    let mut out = Vec::new();
    let mut had_error = false;
    for dir in dirs {
        if !dir.exists() {
            continue;
        }
        for entry in WalkDir::new(dir)
            .min_depth(1)
            .max_depth(3)
            .into_iter()
            .filter_map(|e| ok_or_flag(e, &mut had_error))
        {
            if entry.file_type().is_file() && entry.path().extension().is_some_and(|x| x == "jsonl")
            {
                out.push(entry.into_path());
            }
        }
    }
    Discovered {
        files: out,
        had_error,
    }
}

fn free_pi_sessions_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(agent) = std::env::var_os("FREE_PI_AGENT_DIR") {
        dirs.push(PathBuf::from(agent).join("sessions"));
    }
    let config_base = std::env::var_os("FREEPI_CONFIG_DIR")
        .or_else(|| std::env::var_os("FREE_PI_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".free-pi")));
    if let Some(base) = config_base {
        dirs.push(base.join("agent").join("sessions"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            dirs.push(p.join("free-pi").join("sessions"));
        }
    }
    dirs
}

fn pi_sessions_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(agent) = std::env::var_os("PI_CODING_AGENT_DIR") {
        dirs.push(PathBuf::from(agent).join("sessions"));
    }
    let config_base = std::env::var_os("PI_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| crate::util::home_dir().map(|h| h.join(".pi")));
    if let Some(base) = config_base {
        dirs.push(base.join("agent").join("sessions"));
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            dirs.push(p.join("pi").join("sessions"));
        }
    }
    dirs
}

const EDIT_TOOLS: &[&str] = &["write", "edit", "apply_patch", "ast_edit"];

fn parse_jsonl(tool: &'static str, path: &Path) -> Result<Session> {
    let raw = crate::util::read_to_string_capped(path)?;

    let mut session_id = String::new();
    let mut project = String::new();
    let mut started: Option<DateTime<Utc>> = None;
    let mut header_title: Option<String> = None;
    let mut last_ts: Option<DateTime<Utc>> = None;

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();
    let mut edits: Vec<EditEvent> = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        let outer_ts = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_ts);
        if let Some(t) = outer_ts {
            last_ts = Some(t);
        }

        match entry.get("type").and_then(Value::as_str) {
            Some("session") => {
                session_id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                project = entry
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                started = outer_ts;
                header_title = entry
                    .get("title")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .map(|t| redacted_truncate(t, 80));
            }
            Some("message") => {
                let Some(msg) = entry.get("message") else {
                    continue;
                };
                let raw_role = msg.get("role").and_then(Value::as_str);
                let role = match raw_role {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    Some("toolResult" | "tool") => Role::Tool,
                    _ => continue,
                };
                let ts = msg
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .and_then(DateTime::from_timestamp_millis)
                    .or(outer_ts);

                if role == Role::Tool {
                    messages.push(Message::tool_result(
                        ts,
                        ToolResult {
                            call_id: msg
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .filter(|id| !id.is_empty())
                                .map(str::to_owned),
                            text: bounded_redacted_output(&block_text(msg.get("content"))),
                            is_error: msg.get("isError").and_then(Value::as_bool).unwrap_or(false),
                        },
                    ));
                    continue;
                }

                match msg.get("content") {
                    Some(Value::String(s)) => push(&mut messages, role, s, ts),
                    Some(Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(Value::as_str) {
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(Value::as_str) {
                                        push(&mut messages, role, t, ts);
                                    }
                                }
                                Some("toolCall") => {
                                    let name =
                                        block.get("name").and_then(Value::as_str).unwrap_or("?");
                                    let args = block.get("arguments");

                                    if EDIT_TOOLS.contains(&name) {
                                        for p in arg_paths(args) {
                                            touched.push(p);
                                        }
                                        if let Some(event) = parse_edit_event(name, args, ts) {
                                            edits.push(event);
                                        }
                                    }

                                    let mut args =
                                        args.cloned().unwrap_or_else(|| serde_json::json!({}));
                                    redact_json_strings(&mut args);
                                    messages.push(Message::tool_call(
                                        ts,
                                        ToolCall {
                                            id: block
                                                .get("id")
                                                .and_then(Value::as_str)
                                                .filter(|id| !id.is_empty())
                                                .map(str::to_owned),
                                            name: name.to_owned(),
                                            args,
                                        },
                                    ));
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let _ = session_id;
    let title = header_title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&path.to_string_lossy()),
        tool,
        path: path.to_path_buf(),
        project,
        started,
        ended: last_ts,
        title,
        subagent: false,
        parent_id: None,
        messages,
        touched: dedup_paths(touched),
        edits,
    })
}

fn parse_edit_event(
    name: &str,
    args: Option<&Value>,
    ts: Option<DateTime<Utc>>,
) -> Option<EditEvent> {
    let args = args?;
    let path = clean_path(args.get("path").and_then(Value::as_str)?)?;

    match name {
        "write" => {
            let content = args.get("content").and_then(Value::as_str).unwrap_or("");
            let snippet = redacted_truncate(content.trim(), SNIPPET_CAP);
            Some(EditEvent {
                path,
                kind: EditKind::Write,
                snippet,
                ts,
            })
        }
        "edit" => {
            let snippet = args
                .get("edits")
                .and_then(Value::as_array)
                .and_then(|arr| arr.first())
                .and_then(|e| e.get("newText"))
                .and_then(Value::as_str)
                .map(|t| redacted_truncate(t.trim(), SNIPPET_CAP))
                .unwrap_or_default();
            Some(EditEvent {
                path,
                kind: EditKind::Edit,
                snippet,
                ts,
            })
        }
        _ => None,
    }
}

fn arg_paths(args: Option<&Value>) -> Vec<String> {
    let Some(args) = args else {
        return vec![];
    };
    if let Some(p) = args.get("path").and_then(Value::as_str) {
        if !p.is_empty() {
            return vec![p.to_string()];
        }
    }
    if let Some(Value::Array(ps)) = args.get("paths") {
        return ps
            .iter()
            .filter_map(Value::as_str)
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect();
    }
    vec![]
}

fn block_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = crate::redact::redact(text).into_owned(),
        Value::Array(values) => values.iter_mut().for_each(redact_json_strings),
        Value::Object(values) => values.values_mut().for_each(redact_json_strings),
        _ => {}
    }
}

fn push(messages: &mut Vec<Message>, role: Role, text: &str, ts: Option<DateTime<Utc>>) {
    let text = text.trim();
    if !text.is_empty() {
        messages.push(Message {
            role,
            text: text.to_string(),
            ts,
            tool: None,
            is_rewound: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_free_pi_session_fixture() {
        let jsonl = r#"
{"type":"session","version":3,"id":"01a167c3-0e41-76fb-9707-bb749f3fe878","timestamp":"2026-10-23T07:56:38.850Z","cwd":"/projects/my-app"}
{"type":"model_change","id":"m1","timestamp":"2026-10-23T07:56:39.000Z","modelId":"deepseek-v4"}
{"type":"message","id":"msg1","timestamp":"2026-10-23T07:56:40.000Z","message":{"role":"user","content":[{"type":"text","text":"Implement feature X"}]}}
{"type":"message","id":"msg2","timestamp":"2026-10-23T07:56:45.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"write","arguments":{"path":"src/feature.rs","content":"pub fn feature() {}"}}]}}
{"type":"message","id":"msg3","timestamp":"2026-10-23T07:56:46.000Z","message":{"role":"toolResult","toolCallId":"c1","content":[{"type":"text","text":"wrote 20 bytes"}],"isError":false}}
{"type":"message","id":"msg4","timestamp":"2026-10-23T07:56:48.000Z","message":{"role":"assistant","content":[{"type":"toolCall","name":"edit","arguments":{"path":"src/feature.rs","edits":[{"oldText":"pub fn feature() {}","newText":"pub fn feature() -> bool { true }"}]}}]}}
{"type":"message","id":"msg5","timestamp":"2026-10-23T07:56:50.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done! Feature X is implemented."}]}}
"#;
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("session.jsonl");
        std::fs::write(&file_path, jsonl).unwrap();

        let adapter = FreePiCli;
        let session = adapter.parse(&file_path).unwrap();

        assert_eq!(session.tool, "free-pi-cli");
        assert_eq!(session.project, "/projects/my-app");
        assert_eq!(session.title, "Implement feature X");
        assert_eq!(session.touched, vec!["src/feature.rs"]);
        assert_eq!(session.edits.len(), 2);
        assert_eq!(session.edits[0].kind, EditKind::Write);
        assert_eq!(session.edits[0].path, "src/feature.rs");
        assert_eq!(session.edits[0].snippet, "pub fn feature() {}");
        assert_eq!(session.edits[1].kind, EditKind::Edit);
        assert_eq!(session.edits[1].path, "src/feature.rs");
        assert_eq!(
            session.edits[1].snippet,
            "pub fn feature() -> bool { true }"
        );

        assert_eq!(session.messages.len(), 5);
        assert_eq!(session.messages[0].role, Role::User);
        assert_eq!(session.messages[0].text, "Implement feature X");
        assert_eq!(session.messages[1].role, Role::Tool);
        assert!(session.messages[1].tool.is_some());
        assert_eq!(session.messages[2].role, Role::Tool);
        assert_eq!(session.messages[3].role, Role::Tool);
        assert!(session.messages[3].tool.is_some());
        assert_eq!(session.messages[4].role, Role::Assistant);
        assert_eq!(session.messages[4].text, "Done! Feature X is implemented.");
    }

    #[test]
    fn discovers_real_free_pi_sessions_if_present() {
        let adapter = FreePiCli;
        let discovered = adapter.discover();
        assert!(!discovered.had_error);
        if !discovered.files.is_empty() {
            let session = adapter.parse(&discovered.files[0]).unwrap();
            assert_eq!(session.tool, "free-pi-cli");
            assert!(!session.title.is_empty());
        }
    }
}
