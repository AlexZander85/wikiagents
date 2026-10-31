use super::{clean_path, dedup_paths, ok_or_flag, title_from_messages, Adapter, Discovered, Store};
use crate::model::{Message, Role, Session};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use walkdir::WalkDir;

const KEY_SEP: char = '\u{1f}';

fn make_key(path: &Path, id: &str) -> String {
    format!("{}{KEY_SEP}{id}", path.display())
}

fn display_path(key: &str) -> String {
    key.replace(KEY_SEP, "#")
}

fn file_mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn parse_rfc3339_utc(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_epoch_secs_or_ms(v: &Value) -> Option<DateTime<Utc>> {
    let num = v.as_i64()?;
    if num > 100_000_000_000 {
        DateTime::<Utc>::from_timestamp_millis(num)
    } else {
        DateTime::<Utc>::from_timestamp(num, 0)
    }
}

fn parse_timestamp_ms(v: &Value) -> Option<i64> {
    let num = v.as_i64()?;
    if num > 100_000_000_000 {
        Some(num)
    } else {
        Some(num * 1000)
    }
}

pub fn web_session_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(env_dir) = std::env::var_os("WIKIAGENTS_WEB_SESSION_DIR")
        .or_else(|| std::env::var_os("WEB_SESSION_DIR"))
    {
        let p = PathBuf::from(env_dir);
        if p.is_dir() {
            roots.push(p);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("web_session");
        if p.is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    }
    #[cfg(windows)]
    {
        let p = PathBuf::from(r"D:\wikiagents\web_session");
        if p.is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    }
    if let Some(home) = crate::util::home_dir() {
        let p1 = home.join(".wikiagents").join("web_session");
        if p1.is_dir() && !roots.contains(&p1) {
            roots.push(p1);
        }
        let p2 = home.join(".agentwiki").join("web_session");
        if p2.is_dir() && !roots.contains(&p2) {
            roots.push(p2);
        }
    }
    roots
}

#[derive(Default)]
struct CachedFile {
    path: PathBuf,
    mtime_ms: i64,
    json: Arc<Value>,
}

#[derive(Default)]
pub struct JsonCache {
    inner: Mutex<Option<CachedFile>>,
}

impl JsonCache {
    fn get_or_load(&self, path: &Path) -> Result<Arc<Value>> {
        let mtime_ms = file_mtime_ms(path);
        let mut guard = self.inner.lock().unwrap();
        if let Some(cached) = guard.as_ref() {
            if cached.path == path && cached.mtime_ms == mtime_ms {
                return Ok(Arc::clone(&cached.json));
            }
        }

        let file = std::fs::File::open(path)
            .with_context(|| format!("open web session file {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let parsed: Value = serde_json::from_reader(reader)
            .with_context(|| format!("parse JSON from {}", path.display()))?;
        let arc = Arc::new(parsed);
        *guard = Some(CachedFile {
            path: path.to_path_buf(),
            mtime_ms,
            json: Arc::clone(&arc),
        });
        Ok(arc)
    }
}

fn discover_vendor_files(vendor_kw: &str) -> (Vec<PathBuf>, bool) {
    let mut files = Vec::new();
    let mut had_error = false;
    for root in web_session_roots() {
        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|e| ok_or_flag(e, &mut had_error))
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.into_path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let name_lower = path
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if name_lower.contains(vendor_kw) {
                files.push(path);
            }
        }
    }
    (files, had_error)
}

fn extract_code_block_paths(text: &str, touched: &mut Vec<String>) {
    for line in text.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("```") || trimmed.starts_with("#")) && trimmed.len() > 3 {
            let rest = trimmed
                .trim_start_matches('`')
                .trim_start_matches('#')
                .trim();
            if rest.contains('/') || rest.contains('\\') || rest.contains('.') {
                let first_word = rest.split_whitespace().next().unwrap_or("");
                let token = first_word.trim_matches(|c| c == ':' || c == '"' || c == '\'');
                if let Some(cleaned) = clean_path(token) {
                    if cleaned.contains('.') {
                        touched.push(cleaned);
                    }
                }
            }
        }
    }
}

// =============================================================================
// 1. Claude Web Adapter
// =============================================================================
#[derive(Default)]
pub struct ClaudeWeb {
    cache: JsonCache,
}

impl Adapter for ClaudeWeb {
    fn name(&self) -> &'static str {
        "claude-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        if let Some(arr) = json.as_array() {
            if let Some(first) = arr.first() {
                let id = first.get("uuid").and_then(Value::as_str).unwrap_or("0");
                return parse_claude_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid claude web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, mut had_error) = discover_vendor_files("claude");
        let mut keys: Vec<(String, i64)> = Vec::new();

        for file in &files {
            let json = match self.cache.get_or_load(file) {
                Ok(j) => j,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            let Some(arr) = json.as_array() else {
                continue;
            };
            let mtime = file_mtime_ms(file);
            for item in arr {
                let Some(uuid) = item.get("uuid").and_then(Value::as_str) else {
                    continue;
                };
                let token = item
                    .get("updated_at")
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_utc)
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or(mtime);
                keys.push((make_key(file, uuid), token));
            }
        }

        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, uuid) = key
            .split_once(KEY_SEP)
            .context("malformed claude-web key")?;
        let path = Path::new(file_str);
        let json = self.cache.get_or_load(path)?;
        let arr = json
            .as_array()
            .context("claude web export is not an array")?;
        let item = arr
            .iter()
            .find(|item| item.get("uuid").and_then(Value::as_str) == Some(uuid))
            .with_context(|| format!("session {uuid} not found in {}", path.display()))?;
        parse_claude_item(path, uuid, item)
    }
}

fn parse_claude_item(path: &Path, uuid: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, uuid);
    let title = item
        .get("name")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let started = item
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc);
    let ended = item
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc)
        .or(started);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(chat_msgs) = item.get("chat_messages").and_then(Value::as_array) {
        for msg in chat_msgs {
            let sender = msg.get("sender").and_then(Value::as_str).unwrap_or("human");
            let role = if sender.eq_ignore_ascii_case("human") {
                Role::User
            } else {
                Role::Assistant
            };
            let ts = msg
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_utc);

            let mut text = msg
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                if let Some(content) = msg.get("content").and_then(Value::as_array) {
                    let mut parts = Vec::new();
                    for block in content {
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                let trimmed = t.trim();
                                if !trimmed.is_empty() {
                                    parts.push(trimmed);
                                }
                            }
                        }
                    }
                    text = parts.join("\n\n");
                }
            }

            if !text.is_empty() {
                extract_code_block_paths(&text, &mut touched);
                messages.push(Message::plain(role, text, ts));
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&key),
        tool: "claude-web",
        path: PathBuf::from(display_path(&key)),
        project: "web".to_string(),
        started,
        ended,
        title: final_title,
        subagent: false,
        parent_id: None,
        messages,
        touched,
        edits: Vec::new(),
    })
}

// =============================================================================
// 2. DeepSeek Web Adapter
// =============================================================================
#[derive(Default)]
pub struct DeepSeekWeb {
    cache: JsonCache,
}

impl Adapter for DeepSeekWeb {
    fn name(&self) -> &'static str {
        "deepseek-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        if let Some(arr) = json.as_array() {
            if let Some(first) = arr.first() {
                let id = first.get("id").and_then(Value::as_str).unwrap_or("0");
                return parse_deepseek_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid deepseek web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, mut had_error) = discover_vendor_files("deepseek");
        let mut keys: Vec<(String, i64)> = Vec::new();

        for file in &files {
            let json = match self.cache.get_or_load(file) {
                Ok(j) => j,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            let Some(arr) = json.as_array() else {
                continue;
            };
            let mtime = file_mtime_ms(file);
            for item in arr {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let token = item
                    .get("updated_at")
                    .or_else(|| item.get("inserted_at"))
                    .and_then(Value::as_str)
                    .and_then(parse_rfc3339_utc)
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or(mtime);
                keys.push((make_key(file, id), token));
            }
        }

        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key
            .split_once(KEY_SEP)
            .context("malformed deepseek-web key")?;
        let path = Path::new(file_str);
        let json = self.cache.get_or_load(path)?;
        let arr = json
            .as_array()
            .context("deepseek web export is not an array")?;
        let item = arr
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
            .with_context(|| format!("session {id} not found in {}", path.display()))?;
        parse_deepseek_item(path, id, item)
    }
}

fn parse_deepseek_item(path: &Path, id: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let started = item
        .get("inserted_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc);
    let ended = item
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc)
        .or(started);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(mapping) = item.get("mapping").and_then(Value::as_object) {
        // Find root node
        let mut root_id = None;
        for (nid, node) in mapping {
            let parent = node.get("parent");
            if parent.is_none() || parent.and_then(Value::as_str).is_none() || nid == "root" {
                root_id = Some(nid.clone());
                if nid == "root" {
                    break;
                }
            }
        }

        let mut ordered_node_ids = Vec::new();
        let mut visited = HashSet::new();
        let mut curr = root_id;

        while let Some(cid) = curr {
            if !visited.insert(cid.clone()) {
                break;
            }
            ordered_node_ids.push(cid.clone());
            let next = mapping.get(&cid).and_then(|n| {
                n.get("children")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(Value::as_str)
                    .map(|s| s.to_string())
            });
            curr = next;
        }

        // If DAG traversal was too small compared to mapping size, fallback to remaining nodes
        if ordered_node_ids.len() < mapping.len() {
            for nid in mapping.keys() {
                if visited.insert(nid.clone()) {
                    ordered_node_ids.push(nid.clone());
                }
            }
        }

        for nid in ordered_node_ids {
            let Some(node) = mapping.get(&nid) else {
                continue;
            };
            let Some(msg) = node.get("message").and_then(Value::as_object) else {
                continue;
            };
            let ts = msg
                .get("inserted_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_utc);

            if let Some(fragments) = msg.get("fragments").and_then(Value::as_array) {
                for frag in fragments {
                    let ftype = frag.get("type").and_then(Value::as_str).unwrap_or("");
                    let mut content = frag
                        .get("content")
                        .or_else(|| frag.get("text"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    if content.is_empty() && ftype == "FILE" {
                        if let Some(files) = frag.get("files").and_then(Value::as_array) {
                            let file_names: Vec<&str> = files
                                .iter()
                                .filter_map(|f| f.get("file_name").and_then(Value::as_str))
                                .collect();
                            if !file_names.is_empty() {
                                content = format!("[Файлы: {}]", file_names.join(", "));
                            }
                        }
                    }

                    if content.is_empty() {
                        continue;
                    }

                    let role = match ftype {
                        "REQUEST" | "FILE" => Role::User,
                        "TOOL_OPEN" | "TOOL_SEARCH" | "SEARCH" | "TOOL_FIND" => Role::Tool,
                        _ => Role::Assistant,
                    };

                    extract_code_block_paths(&content, &mut touched);
                    messages.push(Message::plain(role, content, ts));
                }
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&key),
        tool: "deepseek-web",
        path: PathBuf::from(display_path(&key)),
        project: "web".to_string(),
        started,
        ended,
        title: final_title,
        subagent: false,
        parent_id: None,
        messages,
        touched,
        edits: Vec::new(),
    })
}

// =============================================================================
// 3. Gemini Web Adapter
// =============================================================================
#[derive(Default)]
pub struct GeminiWeb {
    cache: JsonCache,
}

impl Adapter for GeminiWeb {
    fn name(&self) -> &'static str {
        "gemini-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        if let Some(arr) = json.as_array() {
            if let Some(first) = arr.first() {
                let id = first.get("id").and_then(Value::as_str).unwrap_or("0");
                return parse_gemini_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid gemini web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, mut had_error) = discover_vendor_files("gemini");
        let mut keys: Vec<(String, i64)> = Vec::new();

        for file in &files {
            let json = match self.cache.get_or_load(file) {
                Ok(j) => j,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            let Some(arr) = json.as_array() else {
                continue;
            };
            let mtime = file_mtime_ms(file);
            for item in arr {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                keys.push((make_key(file, id), mtime));
            }
        }

        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key
            .split_once(KEY_SEP)
            .context("malformed gemini-web key")?;
        let path = Path::new(file_str);
        let json = self.cache.get_or_load(path)?;
        let arr = json
            .as_array()
            .context("gemini web export is not an array")?;
        let item = arr
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
            .with_context(|| format!("session {id} not found in {}", path.display()))?;
        parse_gemini_item(path, id, item)
    }
}

fn parse_gemini_item(path: &Path, id: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let mtime = file_mtime_ms(path);
    let started = DateTime::<Utc>::from_timestamp_millis(mtime);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(msgs) = item.get("messages").and_then(Value::as_array) {
        for m in msgs {
            let role_str = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let role = if role_str.eq_ignore_ascii_case("user") {
                Role::User
            } else {
                Role::Assistant
            };
            let text = m
                .get("markdown")
                .or_else(|| m.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if !text.is_empty() {
                extract_code_block_paths(text, &mut touched);
                messages.push(Message::plain(role, text, None));
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&key),
        tool: "gemini-web",
        path: PathBuf::from(display_path(&key)),
        project: "web".to_string(),
        started,
        ended: started,
        title: final_title,
        subagent: false,
        parent_id: None,
        messages,
        touched,
        edits: Vec::new(),
    })
}

// =============================================================================
// 4. Qwen Web Adapter
// =============================================================================
#[derive(Default)]
pub struct QwenWeb {
    cache: JsonCache,
}

impl Adapter for QwenWeb {
    fn name(&self) -> &'static str {
        "qwen-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        let items = json
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| json.as_array());
        if let Some(arr) = items {
            if let Some(first) = arr.first() {
                let id = first.get("id").and_then(Value::as_str).unwrap_or("0");
                return parse_qwen_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid qwen web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, mut had_error) = discover_vendor_files("qwen");
        let mut keys: Vec<(String, i64)> = Vec::new();

        for file in &files {
            let json = match self.cache.get_or_load(file) {
                Ok(j) => j,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            let items = json
                .get("data")
                .and_then(Value::as_array)
                .or_else(|| json.as_array());
            let Some(arr) = items else {
                continue;
            };
            let mtime = file_mtime_ms(file);
            for item in arr {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let token = item
                    .get("updated_at")
                    .or_else(|| item.get("created_at"))
                    .and_then(parse_timestamp_ms)
                    .unwrap_or(mtime);
                keys.push((make_key(file, id), token));
            }
        }

        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key.split_once(KEY_SEP).context("malformed qwen-web key")?;
        let path = Path::new(file_str);
        let json = self.cache.get_or_load(path)?;
        let items = json
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| json.as_array())
            .context("qwen web export missing data array")?;
        let item = items
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
            .with_context(|| format!("session {id} not found in {}", path.display()))?;
        parse_qwen_item(path, id, item)
    }
}

fn parse_qwen_item(path: &Path, id: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let started = item.get("created_at").and_then(parse_epoch_secs_or_ms);
    let ended = item
        .get("updated_at")
        .and_then(parse_epoch_secs_or_ms)
        .or(started);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(msgs) = item
        .get("chat")
        .and_then(|c| c.get("messages"))
        .and_then(Value::as_array)
    {
        for m in msgs {
            let role_str = m.get("role").and_then(Value::as_str).unwrap_or("user");
            let role = if role_str.eq_ignore_ascii_case("user") {
                Role::User
            } else {
                Role::Assistant
            };
            let ts = m.get("timestamp").and_then(parse_epoch_secs_or_ms);

            let mut text = m
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if text.is_empty() {
                if let Some(list) = m.get("content_list").and_then(Value::as_array) {
                    let mut parts = Vec::new();
                    for part in list {
                        if let Some(c) = part.get("content").and_then(Value::as_str) {
                            let trimmed = c.trim();
                            if !trimmed.is_empty() {
                                parts.push(trimmed);
                            }
                        }
                    }
                    text = parts.join("\n\n");
                }
            }

            if !text.is_empty() {
                extract_code_block_paths(&text, &mut touched);
                messages.push(Message::plain(role, text, ts));
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&key),
        tool: "qwen-web",
        path: PathBuf::from(display_path(&key)),
        project: "web".to_string(),
        started,
        ended,
        title: final_title,
        subagent: false,
        parent_id: None,
        messages,
        touched,
        edits: Vec::new(),
    })
}

// =============================================================================
// 5. GLM Web Adapter
// =============================================================================
#[derive(Default)]
pub struct GlmWeb {
    cache: JsonCache,
}

impl Adapter for GlmWeb {
    fn name(&self) -> &'static str {
        "glm-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        if let Some(arr) = json.as_array() {
            if let Some(first) = arr.first() {
                let id = first.get("id").and_then(Value::as_str).unwrap_or("0");
                return parse_glm_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid glm web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, mut had_error) = discover_vendor_files("glm");
        let mut keys: Vec<(String, i64)> = Vec::new();

        for file in &files {
            let json = match self.cache.get_or_load(file) {
                Ok(j) => j,
                Err(_) => {
                    had_error = true;
                    continue;
                }
            };
            let Some(arr) = json.as_array() else {
                continue;
            };
            let mtime = file_mtime_ms(file);
            for item in arr {
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let token = item
                    .get("updated_at")
                    .or_else(|| item.get("created_at"))
                    .and_then(parse_timestamp_ms)
                    .unwrap_or(mtime);
                keys.push((make_key(file, id), token));
            }
        }

        Some(Store {
            keys,
            files,
            had_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key.split_once(KEY_SEP).context("malformed glm-web key")?;
        let path = Path::new(file_str);
        let json = self.cache.get_or_load(path)?;
        let arr = json.as_array().context("glm web export is not an array")?;
        let item = arr
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(id))
            .with_context(|| format!("session {id} not found in {}", path.display()))?;
        parse_glm_item(path, id, item)
    }
}

fn parse_glm_item(path: &Path, id: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let started = item.get("created_at").and_then(parse_epoch_secs_or_ms);
    let ended = item
        .get("updated_at")
        .and_then(parse_epoch_secs_or_ms)
        .or(started);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(history) = item.get("chat").and_then(|c| c.get("history")) {
        if let Some(msgs_map) = history.get("messages").and_then(Value::as_object) {
            let current_id = history.get("currentId").and_then(Value::as_str);

            let mut chain = Vec::new();
            let mut visited = HashSet::new();
            let mut curr = current_id;

            while let Some(cid) = curr {
                if !visited.insert(cid) {
                    break;
                }
                chain.push(cid);
                curr = msgs_map
                    .get(cid)
                    .and_then(|m| m.get("parentId"))
                    .and_then(Value::as_str);
            }
            chain.reverse();

            // If chain empty or missing messages, collect all messages sorted by timestamp
            let msg_list: Vec<&Value> = if !chain.is_empty() {
                chain.iter().filter_map(|cid| msgs_map.get(*cid)).collect()
            } else {
                let mut all: Vec<&Value> = msgs_map.values().collect();
                all.sort_by_key(|m| m.get("timestamp").and_then(Value::as_i64).unwrap_or(0));
                all
            };

            for m in msg_list {
                let role_str = m.get("role").and_then(Value::as_str).unwrap_or("user");
                let role = if role_str.eq_ignore_ascii_case("user") {
                    Role::User
                } else {
                    Role::Assistant
                };
                let ts = m.get("timestamp").and_then(parse_epoch_secs_or_ms);
                let text = m
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if !text.is_empty() {
                    extract_code_block_paths(text, &mut touched);
                    messages.push(Message::plain(role, text, ts));
                }
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: short_id(&key),
        tool: "glm-web",
        path: PathBuf::from(display_path(&key)),
        project: "web".to_string(),
        started,
        ended,
        title: final_title,
        subagent: false,
        parent_id: None,
        messages,
        touched,
        edits: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_claude_web_parsing() {
        let claude = ClaudeWeb::default();
        let sample_path = Path::new(r"D:\wikiagents\web_session\conversations_claude.json");
        if sample_path.is_file() {
            let session = claude.parse(sample_path).expect("parse claude");
            assert_eq!(session.tool, "claude-web");
            assert!(!session.messages.is_empty());
        }
    }

    #[test]
    fn test_deepseek_web_parsing() {
        let deepseek = DeepSeekWeb::default();
        let sample_path = Path::new(r"D:\wikiagents\web_session\conversations_deepseek.json");
        if sample_path.is_file() {
            let session = deepseek.parse(sample_path).expect("parse deepseek");
            assert_eq!(session.tool, "deepseek-web");
            assert!(!session.messages.is_empty());
        }
    }

    #[test]
    fn test_gemini_web_parsing() {
        let gemini = GeminiWeb::default();
        let sample_path =
            Path::new(r"D:\wikiagents\web_session\gemini_conversations_ALL_34_chats.json");
        if sample_path.is_file() {
            let session = gemini.parse(sample_path).expect("parse gemini");
            assert_eq!(session.tool, "gemini-web");
        }
    }

    #[test]
    fn test_qwen_web_parsing() {
        let qwen = QwenWeb::default();
        let sample_path =
            Path::new(r"D:\wikiagents\web_session\qwen_chat-export-1793387130342.json");
        if sample_path.is_file() {
            let session = qwen.parse(sample_path).expect("parse qwen");
            assert_eq!(session.tool, "qwen-web");
            assert!(!session.messages.is_empty());
        }
    }

    #[test]
    fn test_glm_web_parsing() {
        let glm = GlmWeb::default();
        let sample_path =
            Path::new(r"D:\wikiagents\web_session\archived-chat-export-1793387426982_glm.json");
        if sample_path.is_file() {
            let session = glm.parse(sample_path).expect("parse glm");
            assert_eq!(session.tool, "glm-web");
            assert!(!session.messages.is_empty());
        }
    }

    #[test]
    fn test_web_stores_and_keys() {
        let claude = ClaudeWeb::default();
        if let Some(store) = claude.store() {
            assert_eq!(store.keys.len(), 209);
            let first_key = &store.keys[0].0;
            let session = claude.parse_key(first_key).expect("parse_key claude");
            assert_eq!(session.tool, "claude-web");
            assert!(!session.messages.is_empty());
        }

        let deepseek = DeepSeekWeb::default();
        if let Some(store) = deepseek.store() {
            assert_eq!(store.keys.len(), 177);
            let first_key = &store.keys[0].0;
            let session = deepseek.parse_key(first_key).expect("parse_key deepseek");
            assert_eq!(session.tool, "deepseek-web");
            assert!(!session.messages.is_empty());
        }

        let gemini = GeminiWeb::default();
        if let Some(store) = gemini.store() {
            assert_eq!(store.keys.len(), 34);
            let first_key = &store.keys[0].0;
            let session = gemini.parse_key(first_key).expect("parse_key gemini");
            assert_eq!(session.tool, "gemini-web");
        }

        let qwen = QwenWeb::default();
        if let Some(store) = qwen.store() {
            assert_eq!(store.keys.len(), 307);
            let first_key = &store.keys[0].0;
            let session = qwen.parse_key(first_key).expect("parse_key qwen");
            assert_eq!(session.tool, "qwen-web");
            assert!(!session.messages.is_empty());
        }

        let glm = GlmWeb::default();
        if let Some(store) = glm.store() {
            assert_eq!(store.keys.len(), 144);
            let first_key = &store.keys[0].0;
            let session = glm.parse_key(first_key).expect("parse_key glm");
            assert_eq!(session.tool, "glm-web");
            assert!(!session.messages.is_empty());
        }
    }
}
