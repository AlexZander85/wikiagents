use super::{
    bounded_redacted_output, clean_path, dedup_paths, ok_or_flag, parse_ts, redacted_truncate,
    title_from_messages, Adapter, Discovered,
};
use crate::model::{EditEvent, EditKind, Message, Role, Session, ToolCall, ToolResult};
use crate::util::short_id;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Antigravity {
    root: Option<PathBuf>,
    dir: &'static str,
}

impl Default for Antigravity {
    fn default() -> Self {
        Self::ide()
    }
}

impl Antigravity {
    /// Stock Antigravity IDE transcripts rooted at `~/.gemini/antigravity/brain`.
    pub fn ide() -> Self {
        Antigravity {
            root: None,
            dir: "antigravity",
        }
    }

    /// Antigravity CLI (`agy`, which replaced Gemini CLI for personal accounts)
    /// rooted at `~/.gemini/antigravity-cli/brain`.
    pub fn cli() -> Self {
        Antigravity {
            root: None,
            dir: "antigravity-cli",
        }
    }

    /// An adapter for Antigravity IDE transcripts under `home/.gemini/antigravity/brain`.
    pub fn in_home(home: impl Into<PathBuf>) -> Self {
        Antigravity {
            root: Some(
                home.into()
                    .join(".gemini")
                    .join("antigravity")
                    .join("brain"),
            ),
            dir: "antigravity",
        }
    }

    /// An adapter for Antigravity CLI (`agy`) transcripts under `home/.gemini/antigravity-cli/brain`.
    pub fn cli_in_home(home: impl Into<PathBuf>) -> Self {
        Antigravity {
            root: Some(
                home.into()
                    .join(".gemini")
                    .join("antigravity-cli")
                    .join("brain"),
            ),
            dir: "antigravity-cli",
        }
    }
}

const SNIPPET_CAP: usize = 200;

fn edited_path(name: &str, input: Option<&Value>) -> Option<String> {
    let writes = matches!(name, "write_file" | "edit_file");
    if !writes {
        return None;
    }
    let input = input?;
    for key in ["Path", "path", "file_path"] {
        if let Some(p) = input.get(key).and_then(Value::as_str) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

fn edit_event(
    name: &str,
    input: Option<&Value>,
    ts: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<EditEvent> {
    let path = clean_path(&edited_path(name, input)?)?;
    let input = input?;
    let kind = match name {
        "edit_file" => EditKind::Edit,
        "write_file" => EditKind::Write,
        _ => return None,
    };
    let raw = match kind {
        EditKind::Edit => input
            .get("Replacement")
            .or_else(|| input.get("replacement"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        EditKind::Write => input
            .get("Content")
            .or_else(|| input.get("content"))
            .and_then(Value::as_str)
            .unwrap_or(""),
        _ => "",
    };
    let snippet = redacted_truncate(raw.trim(), SNIPPET_CAP);
    Some(EditEvent {
        path,
        kind,
        snippet,
        ts,
    })
}

fn redact_json_strings(value: &mut Value) {
    match value {
        Value::String(text) => *text = crate::redact::redact(text).into_owned(),
        Value::Array(values) => values.iter_mut().for_each(redact_json_strings),
        Value::Object(values) => values.values_mut().for_each(redact_json_strings),
        _ => {}
    }
}

fn extract_user_request(text: &str) -> String {
    if let Some(start) = text.find("<USER_REQUEST>") {
        let after = &text[start + "<USER_REQUEST>".len()..];
        if let Some(end) = after.find("</USER_REQUEST>") {
            return after[..end].trim().to_string();
        }
    }
    text.trim().to_string()
}

/// The transcript of one conversation folder: the first directory (in
/// `conversation_id`'s nesting order) that holds one, preferring
/// `transcript_full.jsonl` over `transcript.jsonl` beside it so full,
/// untruncated model responses and tables are preserved.
fn conversation_transcript(conv: &Path) -> Option<PathBuf> {
    let logs = conv.join(".system_generated").join("logs");
    let dirs = [
        logs,
        conv.join(".system_generated"),
        conv.join("logs"),
        conv.to_path_buf(),
    ];
    dirs.iter().find_map(|dir| {
        ["transcript_full.jsonl", "transcript.jsonl"]
            .iter()
            .map(|name| dir.join(name))
            .find(|p| p.is_file())
    })
}

fn conversation_id(path: &Path) -> Option<String> {
    let mut current = path.parent()?;
    if current.file_name().is_some_and(|n| n == "logs") {
        current = current.parent()?;
    }
    if current
        .file_name()
        .is_some_and(|n| n == ".system_generated")
    {
        current = current.parent()?;
    }
    current
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

impl Adapter for Antigravity {
    fn name(&self) -> &'static str {
        "antigravity"
    }

    fn root(&self) -> Option<PathBuf> {
        if let Some(r) = &self.root {
            return Some(r.clone());
        }
        Some(
            crate::util::home_dir()?
                .join(".gemini")
                .join(self.dir)
                .join("brain"),
        )
    }

    fn reconcile_scope(&self) -> Option<String> {
        super::root_scope(self.root().as_deref())
    }

    fn discover(&self) -> Discovered {
        let Some(root) = self.root() else {
            return Vec::new().into();
        };
        if !root.exists() {
            return Vec::new().into();
        }
        // Probe the few places a conversation keeps its transcript instead of
        // walking the brain: each conversation folder also holds artifacts,
        // screenshots and recordings, and a walk over them (50k entries for
        // ~100 conversations) dominated every sync.
        let mut had_error = false;
        let mut files: Vec<PathBuf> = Vec::new();
        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => {
                return Discovered {
                    files,
                    had_error: true,
                }
            }
        };
        for entry in entries {
            let Some(entry) = ok_or_flag(entry, &mut had_error) else {
                continue;
            };
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if let Some(found) = conversation_transcript(&entry.path()) {
                files.push(found);
            }
        }
        files.sort();

        Discovered { files, had_error }
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let (lines, _windowed) = crate::util::session_lines_keeping(path, |line| {
            line.contains("\"type\":\"USER_INPUT\"")
                || line.contains("\"type\":\"PLANNER_RESPONSE\"")
        })?;

        let mut messages: Vec<Message> = Vec::new();
        let mut touched: Vec<String> = Vec::new();
        let mut edits: Vec<EditEvent> = Vec::new();
        let mut project: Option<String> = None;
        let mut started = None;
        let mut ended = None;

        for line in &lines {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };

            let ts = v
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_ts);
            if let Some(t) = ts {
                if started.is_none() {
                    started = Some(t);
                }
                ended = Some(t);
            }

            let step_type = v.get("type").and_then(Value::as_str).unwrap_or("");
            match step_type {
                "USER_INPUT" => {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        let clean = extract_user_request(c);
                        if !clean.is_empty() {
                            messages.push(Message {
                                role: Role::User,
                                text: clean,
                                ts,
                                tool: None,
                                is_rewound: false,
                            });
                        }
                    }
                }
                "PLANNER_RESPONSE" => {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        let trimmed = c.trim();
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

                    if let Some(Value::Array(tcs)) = v.get("tool_calls") {
                        for tc in tcs {
                            let name = tc.get("name").and_then(Value::as_str).unwrap_or("?");
                            let input = tc.get("args").or_else(|| tc.get("arguments"));
                            if project.is_none() {
                                if let Some(inp) = input {
                                    if let Some(cwd) = inp
                                        .get("Cwd")
                                        .or_else(|| inp.get("cwd"))
                                        .and_then(Value::as_str)
                                    {
                                        let clean_cwd = crate::util::collapse_backslashes(
                                            cwd.trim().trim_matches(['"', '\'']).trim(),
                                        );
                                        if !clean_cwd.is_empty() {
                                            project = Some(clean_cwd);
                                        }
                                    } else if let Some(p) = inp
                                        .get("Path")
                                        .or_else(|| inp.get("path"))
                                        .and_then(Value::as_str)
                                    {
                                        let clean_p = crate::util::collapse_backslashes(
                                            p.trim().trim_matches(['"', '\'']).trim(),
                                        );
                                        if let Some(parent) = Path::new(&clean_p).parent() {
                                            let s = parent.to_string_lossy().trim().to_string();
                                            if !s.is_empty() {
                                                project = Some(s);
                                            }
                                        }
                                    }
                                }
                            }

                            if let Some(ev) = edit_event(name, input, ts) {
                                touched.push(ev.path.clone());
                                edits.push(ev);
                            }

                            let mut args = input.cloned().unwrap_or_else(|| json!({}));
                            redact_json_strings(&mut args);
                            messages.push(Message::tool_call(
                                ts,
                                ToolCall {
                                    id: None,
                                    name: name.to_string(),
                                    args,
                                },
                            ));
                        }
                    }
                }
                "GENERIC" => {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        let output = bounded_redacted_output(c);
                        messages.push(Message::tool_result(
                            ts,
                            ToolResult {
                                call_id: None,
                                text: output,
                                is_error: v.get("status").and_then(Value::as_str) == Some("ERROR"),
                            },
                        ));
                    }
                }
                _ => {}
            }
        }

        let cid = conversation_id(path).unwrap_or_else(|| short_id(&path.to_string_lossy()));
        let title = title_from_messages(&messages);

        Ok(Session {
            id: cid,
            tool: "antigravity",
            path: path.to_path_buf(),
            project: project.unwrap_or_else(|| "antigravity".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_antigravity_edit_event() {
        let input = json!({
            "Path": "d:/project/main.rs",
            "Replacement": "fn main() { println!(\"hello\"); }",
            "Target": "fn main() {}"
        });
        let ev =
            edit_event("edit_file", Some(&input), None).expect("edit_event should parse edit_file");
        assert_eq!(ev.path, "d:/project/main.rs");
        assert!(ev.snippet.contains("println!"));

        let write_input = json!({
            "Path": "d:/project/new.rs",
            "Content": "let a = 123;"
        });
        let wev = edit_event("write_file", Some(&write_input), None)
            .expect("edit_event should parse write_file");
        assert_eq!(wev.path, "d:/project/new.rs");
        assert!(wev.snippet.contains("let a = 123;"));
    }

    #[test]
    fn test_antigravity_cli_discovery_and_parse() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("cli_session_jwt_test")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&session_dir).unwrap();
        let transcript_path = session_dir.join("transcript.jsonl");

        let content = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-24T12:00:00.000Z","content":"<USER_REQUEST>Refactor auth module with JWT support</USER_REQUEST>"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-24T12:00:05.000Z","content":"Updating auth implementation now.","tool_calls":[{"name":"edit_file","args":{"Path":"d:/crypto/src/auth.rs","Replacement":"pub fn verify_jwt(token: &str) -> bool { true }","Target":"pub fn verify_token() {}"}}]}
{"step_index":2,"source":"GENERIC","type":"GENERIC","status":"DONE","created_at":"2026-10-24T12:00:06.000Z","content":"Successfully edited file d:/crypto/src/auth.rs"}
{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-24T12:00:10.000Z","content":"Authentication refactoring complete with JWT support."}
"#;
        std::fs::write(&transcript_path, content).unwrap();

        let cli_adapter = Antigravity::cli_in_home(temp.path());
        assert_eq!(cli_adapter.name(), "antigravity");
        assert!(cli_adapter
            .root()
            .unwrap()
            .ends_with(Path::new(".gemini").join("antigravity-cli").join("brain")));

        let discovered = cli_adapter.discover();
        assert!(!discovered.had_error);
        assert_eq!(discovered.files.len(), 1);
        assert_eq!(discovered.files[0], transcript_path);

        let session = cli_adapter
            .parse(&transcript_path)
            .expect("parse should succeed");
        assert_eq!(session.tool, "antigravity");
        assert_eq!(session.id, "cli_session_jwt_test");
        assert_eq!(session.title, "Refactor auth module with JWT support");
        assert_eq!(session.messages.len(), 5);
        assert_eq!(session.touched, vec!["d:/crypto/src/auth.rs"]);
        assert_eq!(session.edits.len(), 1);
        assert_eq!(session.edits[0].path, "d:/crypto/src/auth.rs");
        assert_eq!(session.edits[0].kind, EditKind::Edit);

        // Verify IDE adapter ignores the CLI directory
        let ide_adapter = Antigravity::in_home(temp.path());
        let ide_discovered = ide_adapter.discover();
        assert_eq!(ide_discovered.files.len(), 0);
    }

    /// Some exports escape each separator twice, so a naive read put
    /// `D:\\crypto` in `files.project` - a path no filter and no shell agrees
    /// with. The project is un-escaped while parsing, before the index keys it.
    #[test]
    fn a_double_escaped_cwd_is_stored_as_one_path() {
        let temp = tempfile::tempdir().unwrap();
        let session_dir = temp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("brain")
            .join("cli_session_esc")
            .join(".system_generated")
            .join("logs");
        std::fs::create_dir_all(&session_dir).unwrap();
        let transcript_path = session_dir.join("transcript.jsonl");
        std::fs::write(
            &transcript_path,
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-24T12:00:00.000Z","content":"fix the build"}
{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-10-24T12:00:05.000Z","content":"editing","tool_calls":[{"name":"edit_file","args":{"Cwd":"D:\\\\crypto\\\\src","Path":"D:\\\\crypto\\\\src\\\\main.rs","Replacement":"fn main() {}","Target":"fn main() {}"}}]}
"#,
        )
        .unwrap();

        let session = Antigravity::cli_in_home(temp.path())
            .parse(&transcript_path)
            .expect("parse should succeed");
        assert_eq!(session.project, r"D:\crypto\src");
        assert_eq!(crate::util::project_key(&session.project), "d:/crypto/src");
    }

    #[test]
    fn test_prefers_transcript_full_over_transcript() {
        let temp = tempfile::tempdir().unwrap();
        let logs_dir = temp.path().join(".system_generated").join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let compact_path = logs_dir.join("transcript.jsonl");
        let full_path = logs_dir.join("transcript_full.jsonl");

        std::fs::write(&compact_path, "compact").unwrap();
        std::fs::write(&full_path, "full").unwrap();

        let chosen = conversation_transcript(temp.path()).expect("found transcript");
        assert_eq!(
            chosen, full_path,
            "transcript_full.jsonl must be chosen over transcript.jsonl"
        );
    }
}
