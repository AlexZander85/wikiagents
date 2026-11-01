use super::{clean_path, dedup_paths, ok_or_flag, title_from_messages, Adapter, Discovered, Store};
use crate::model::{Message, Role, Session};
use crate::util::short_id;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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

/// A web chat's own id, not the file it happens to be exported into: re-export
/// the same chats next month and they are still the same session.
fn web_session_id(tool: &str, native: &str) -> String {
    short_id(&format!("{tool}:{native}"))
}

/// The native id half of a store key (`path<SEP>native`), if this key is one.
pub(crate) fn native_part_of_key(key: &str) -> Option<&str> {
    key.split_once(KEY_SEP).map(|(_, native)| native)
}

/// The export-file half of a store key, if this key is one.
pub(crate) fn file_part_of_key(key: &str) -> Option<&str> {
    key.split_once(KEY_SEP).map(|(file, _)| file)
}

/// The reasoning a model showed for one turn. The exporters disagree about
/// where it lives: Claude web puts `thinking` blocks in the content array,
/// Qwen-style exporters use a `reasoning_content` field, and the v4 Gemini
/// exporter writes a plain `thinking` string on the message.
fn reasoning_of(msg: &Value) -> Option<String> {
    const REASON_FIELDS: [&str; 4] = [
        "thinking",
        "reasoning_content",
        "thinking_content",
        "reasoning",
    ];
    const BLOCK_ARRAYS: [&str; 3] = ["content", "content_list", "fragments"];
    const BLOCK_FIELDS: [&str; 4] = ["thinking", "reasoning_content", "text", "content"];

    let mut parts: Vec<&str> = Vec::new();
    for field in REASON_FIELDS {
        if let Some(s) = msg.get(field).and_then(Value::as_str) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed);
            }
        }
    }
    for array_field in BLOCK_ARRAYS {
        let Some(blocks) = msg.get(array_field).and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            let kind = block
                .get("type")
                .or_else(|| block.get("phase"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if kind != "thinking" && kind != "reasoning" {
                continue;
            }
            for field in BLOCK_FIELDS {
                if let Some(s) = block.get(field).and_then(Value::as_str) {
                    let trimmed = s.trim();
                    if !trimmed.is_empty() {
                        parts.push(trimmed);
                        break;
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n\n"))
}

/// How many turns an export item holds, from whichever container the vendor
/// uses. Only used to break a tie between two exports of one chat, so an
/// approximate count is enough.
fn message_count_of(item: &Value) -> usize {
    ["chat_messages", "messages", "history", "content_list"]
        .iter()
        .filter_map(|field| item.get(*field).and_then(Value::as_array))
        .map(Vec::len)
        .max()
        .or_else(|| {
            item.get("mapping")
                .and_then(Value::as_object)
                .map(|m| m.len())
        })
        .unwrap_or(0)
        // The v4 Gemini exporter marks a chat it read whole, so a complete
        // export beats a truncated one when both carry the same timestamp.
        + usize::from(item.get("complete").and_then(Value::as_bool) == Some(true))
}

/// One store key per chat id: the newest export wins, and a tie goes to the
/// export that holds more of the conversation.
#[derive(Default)]
struct StoreKeys {
    ordered: Vec<Entry>,
    best: HashMap<String, usize>,
}

struct Entry {
    key: String,
    token: i64,
    message_count: usize,
}

impl StoreKeys {
    fn add(&mut self, native: &str, file: &Path, token: i64, message_count: usize) {
        match self.best.get(native) {
            None => {
                self.best.insert(native.to_string(), self.ordered.len());
                self.ordered.push(Entry {
                    key: make_key(file, native),
                    token,
                    message_count,
                });
            }
            Some(at) => {
                let at = *at;
                let entry = &mut self.ordered[at];
                if token > entry.token
                    || (token == entry.token && message_count > entry.message_count)
                {
                    entry.key = make_key(file, native);
                    entry.token = token;
                    entry.message_count = message_count;
                }
            }
        }
    }

    fn finish(self) -> Vec<(String, i64)> {
        self.ordered
            .into_iter()
            .map(|entry| (entry.key, entry.token))
            .collect()
    }
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

/// An epoch time in seconds or milliseconds, as a number or a numeric string
/// (ChatGPT writes fractional seconds), or an RFC 3339 string.
fn parse_epoch_millis(v: &Value) -> Option<i64> {
    let num = match v.as_str() {
        Some(s) => match s.parse::<f64>() {
            Ok(num) => num,
            Err(_) => return parse_rfc3339_utc(s).map(|dt| dt.timestamp_millis()),
        },
        // Whole numbers come through here too: `as_f64` accepts them.
        None => v.as_f64()?,
    };
    if !num.is_finite() {
        return None;
    }
    let millis = if num.abs() > 100_000_000_000.0 {
        num
    } else {
        num * 1000.0
    };
    Some(millis.round() as i64)
}

fn parse_epoch(v: &Value) -> Option<DateTime<Utc>> {
    parse_epoch_millis(v).and_then(DateTime::<Utc>::from_timestamp_millis)
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
    // Next to the executable rather than to a machine-specific checkout: an
    // installed binary carries its own exports, wherever it is launched from.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("web_session");
            if p.is_dir() && !roots.contains(&p) {
                roots.push(p);
            }
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
    // Last, so an explicit root always wins: a checkout that keeps its exports
    // beside it is still found when the tool is run from inside that checkout.
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join("web_session");
        if p.is_dir() && !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots
}

#[derive(Default)]
struct CachedFile {
    path: PathBuf,
    mtime_ms: i64,
    json: Arc<Value>,
    /// Native id -> position in the top-level array, built the first time a
    /// session is looked up by id. Every adapter used to `find` its way through
    /// the array per session, which made re-indexing one big export quadratic -
    /// the Claude export alone holds thousands of chats.
    by_id: Option<Arc<HashMap<String, usize>>>,
}

#[derive(Default)]
pub struct JsonCache {
    inner: Mutex<Option<CachedFile>>,
}

/// The id field a top-level export item is keyed by. Claude web exports name it
/// `uuid`, the others `id`.
fn native_id_of_item(item: &Value) -> Option<&str> {
    item.get("uuid")
        .and_then(Value::as_str)
        .or_else(|| item.get("id").and_then(Value::as_str))
        .or_else(|| item.get("conversation_id").and_then(Value::as_str))
}

/// The array of chats a vendor puts at the top of the file.
fn top_level_items(json: &Value) -> Option<&Vec<Value>> {
    json.as_array()
}

/// Qwen wraps its chats in a `data` object; older exports are a bare array.
fn data_or_top_items(json: &Value) -> Option<&Vec<Value>> {
    json.get("data")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
}

impl JsonCache {
    fn get_or_load(&self, path: &Path) -> Result<Arc<Value>> {
        Ok(self.cached(path)?.json)
    }

    /// The document together with the position of `wanted` inside the array
    /// `items` selects. The id index is built once per file and kept with it, so
    /// listing a store and then re-parsing each of its sessions stays linear -
    /// the per-session `find` made re-indexing one big export quadratic, and the
    /// Claude export holds thousands of chats.
    fn locate(
        &self,
        path: &Path,
        wanted: &str,
        items: fn(&Value) -> Option<&Vec<Value>>,
    ) -> Result<(Arc<Value>, usize)> {
        let cached = self.cached(path)?;
        let index = match cached.by_id {
            Some(index) => index,
            None => {
                let mut map = HashMap::new();
                if let Some(arr) = items(&cached.json) {
                    for (i, item) in arr.iter().enumerate() {
                        if let Some(id) = native_id_of_item(item) {
                            // Two items with one id in a single file is a broken
                            // export; the first spelling is the one we index.
                            map.entry(id.to_string()).or_insert(i);
                        }
                    }
                }
                let index = Arc::new(map);
                if let Ok(mut guard) = self.inner.lock() {
                    if let Some(inner) = guard.as_mut() {
                        inner.by_id = Some(Arc::clone(&index));
                    }
                }
                index
            }
        };
        let at = index
            .get(wanted)
            .copied()
            .with_context(|| format!("session {wanted} not found in {}", path.display()))?;
        Ok((cached.json, at))
    }

    fn cached(&self, path: &Path) -> Result<CachedFile> {
        let mtime_ms = file_mtime_ms(path);
        // A poisoned lock still guards a usable cache; failing the whole lookup
        // over another thread's panic would be worse than reusing its entry.
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = guard.as_ref() {
            if cached.path == path && cached.mtime_ms == mtime_ms {
                return Ok(CachedFile {
                    path: cached.path.clone(),
                    mtime_ms: cached.mtime_ms,
                    json: Arc::clone(&cached.json),
                    by_id: cached.by_id.clone(),
                });
            }
        }

        let file = std::fs::File::open(path)
            .with_context(|| format!("open web session file {}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let parsed: Value = serde_json::from_reader(reader)
            .with_context(|| format!("parse JSON from {}", path.display()))?;
        let cached = CachedFile {
            path: path.to_path_buf(),
            mtime_ms,
            json: Arc::new(parsed),
            by_id: None,
        };
        *guard = Some(CachedFile {
            path: cached.path.clone(),
            mtime_ms: cached.mtime_ms,
            json: Arc::clone(&cached.json),
            by_id: None,
        });
        Ok(cached)
    }
}

/// Whether a vendor's name appears in the part of the path below the export
/// root: `web_session/gpt/...` is filed under GPT, while a root that happens to
/// sit in `D:\tools\gpt-tools` or `~/.claude` says nothing about its files.
fn filed_under(root: &Path, path: &Path, vendor_kw: &str) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rel| rel.to_string_lossy().to_lowercase().contains(vendor_kw))
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
            if filed_under(&root, &path, vendor_kw) {
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

/// The chat `JsonCache::locate` pointed at, read back through the same picker:
/// an index is only meaningful inside the array it was built from, and Qwen's
/// chats sit under `data` rather than at the top of the file.
fn item_at(json: &Value, at: usize, items: fn(&Value) -> Option<&Vec<Value>>) -> Option<&Value> {
    items(json).and_then(|arr| arr.get(at))
}

/// List every chat in these export files, one store key per conversation:
/// `pick` says where the chats sit in a file and `token_of` when a chat last
/// changed, which is what decides between two exports of one conversation.
fn scan_store(
    cache: &JsonCache,
    files: &[PathBuf],
    pick: fn(&Value) -> Option<&Vec<Value>>,
    token_of: fn(&Value, i64) -> i64,
) -> (StoreKeys, bool) {
    let mut keys = StoreKeys::default();
    let mut had_error = false;
    for file in files {
        let json = match cache.get_or_load(file) {
            Ok(json) => json,
            Err(_) => {
                had_error = true;
                continue;
            }
        };
        let Some(arr) = pick(&json) else {
            continue;
        };
        let mtime = file_mtime_ms(file);
        for item in arr {
            let Some(id) = native_id_of_item(item) else {
                continue;
            };
            keys.add(id, file, token_of(item, mtime), message_count_of(item));
        }
    }
    (keys, had_error)
}

// =============================================================================
// 1. Claude Web Adapter
// =============================================================================
#[derive(Default)]
pub struct ClaudeWeb {
    cache: JsonCache,
}

impl ClaudeWeb {
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("updated_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc)
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(mtime)
    }
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
        let (files, fs_error) = discover_vendor_files("claude");
        let (keys, read_error) = scan_store(&self.cache, &files, top_level_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, uuid) = key
            .split_once(KEY_SEP)
            .context("malformed claude-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, uuid, top_level_items)?;
        let item = item_at(&json, at, top_level_items)
            .context("claude web export changed while it was read")?;
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

            // The reasoning blocks come before the answer they produced, the way
            // they read in the chat.
            if let Some(reasoning) = reasoning_of(msg) {
                messages.push(Message::plain(Role::Assistant, reasoning, ts));
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
        id: web_session_id("claude-web", uuid),
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

impl DeepSeekWeb {
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("updated_at")
            .or_else(|| item.get("inserted_at"))
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc)
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(mtime)
    }
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
        let (files, fs_error) = discover_vendor_files("deepseek");
        let (keys, read_error) = scan_store(&self.cache, &files, top_level_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key
            .split_once(KEY_SEP)
            .context("malformed deepseek-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, id, top_level_items)?;
        let item =
            item_at(&json, at, top_level_items).context("web export changed while it was read")?;
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
            let Some(msg) = node.get("message").filter(|m| m.is_object()) else {
                continue;
            };
            let ts = msg
                .get("inserted_at")
                .and_then(Value::as_str)
                .and_then(parse_rfc3339_utc);

            // DeepSeek keeps the chain of thought on the message, beside the
            // fragments it produced. Thinking-typed fragments are read here
            // rather than in the loop below, so one turn is not shown twice.
            if let Some(reasoning) = reasoning_of(msg) {
                messages.push(Message::plain(Role::Assistant, reasoning, ts));
            }

            if let Some(fragments) = msg.get("fragments").and_then(Value::as_array) {
                for frag in fragments {
                    let ftype = frag.get("type").and_then(Value::as_str).unwrap_or("");
                    if matches!(
                        ftype.to_ascii_lowercase().as_str(),
                        "thinking" | "reasoning"
                    ) {
                        continue;
                    }
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
        id: web_session_id("deepseek-web", id),
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

impl GeminiWeb {
    /// The v4 exporter stamps each chat with the moment it was read out, so a
    /// re-export into a new file still compares as the newer copy of it.
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("exported_at")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339_utc)
            .or_else(|| item.get("exported_at").and_then(parse_epoch_secs_or_ms))
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(mtime)
    }
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
        let (files, fs_error) = discover_vendor_files("gemini");
        let (keys, read_error) = scan_store(&self.cache, &files, top_level_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key
            .split_once(KEY_SEP)
            .context("malformed gemini-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, id, top_level_items)?;
        let item =
            item_at(&json, at, top_level_items).context("web export changed while it was read")?;
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

    // The v4 exporter stamps the chat with the moment it was read out and marks
    // whether the whole thread came with it; older exports only have the file.
    let exported = item
        .get("exported_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc)
        .or_else(|| item.get("exported_at").and_then(parse_epoch_secs_or_ms))
        .or_else(|| DateTime::<Utc>::from_timestamp_millis(file_mtime_ms(path)));
    let started = exported;
    let ended = item
        .get("updated_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339_utc)
        .or(started);

    let mut messages: Vec<(Option<i64>, usize, Message)> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(msgs) = item.get("messages").and_then(Value::as_array) {
        for (i, m) in msgs.iter().enumerate() {
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
            let order = m.get("order").and_then(Value::as_i64);
            // v4 keeps the model's reasoning beside its answer; it reads as the
            // turn before the answer, which is where the chat showed it.
            if let Some(reasoning) = reasoning_of(m) {
                messages.push((order, i, Message::plain(Role::Assistant, reasoning, None)));
            }
            if !text.is_empty() {
                extract_code_block_paths(text, &mut touched);
                messages.push((order, i, Message::plain(role, text, None)));
            }
        }
    }

    // An out-of-order export is still read in the exporter's own order; files
    // without `order` keep the array order they came in.
    messages.sort_by_key(|entry| (entry.0.unwrap_or(entry.1 as i64), entry.1));
    let messages: Vec<Message> = messages.into_iter().map(|(_, _, m)| m).collect();

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: web_session_id("gemini-web", id),
        tool: "gemini-web",
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
// 4. Qwen Web Adapter
// =============================================================================
#[derive(Default)]
pub struct QwenWeb {
    cache: JsonCache,
}

impl QwenWeb {
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("updated_at")
            .or_else(|| item.get("created_at"))
            .and_then(parse_timestamp_ms)
            .unwrap_or(mtime)
    }
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
        let (files, fs_error) = discover_vendor_files("qwen");
        let (keys, read_error) = scan_store(&self.cache, &files, data_or_top_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key.split_once(KEY_SEP).context("malformed qwen-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, id, data_or_top_items)?;
        let item = item_at(&json, at, data_or_top_items)
            .context("web export changed while it was read")?;
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

            if let Some(reasoning) = reasoning_of(m) {
                messages.push(Message::plain(Role::Assistant, reasoning, ts));
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
        id: web_session_id("qwen-web", id),
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

impl GlmWeb {
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("updated_at")
            .or_else(|| item.get("created_at"))
            .and_then(parse_timestamp_ms)
            .unwrap_or(mtime)
    }
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
        let (files, fs_error) = discover_vendor_files("glm");
        let (keys, read_error) = scan_store(&self.cache, &files, top_level_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key.split_once(KEY_SEP).context("malformed glm-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, id, top_level_items)?;
        let item =
            item_at(&json, at, top_level_items).context("web export changed while it was read")?;
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
                if let Some(reasoning) = reasoning_of(m) {
                    messages.push(Message::plain(Role::Assistant, reasoning, ts));
                }
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
        id: web_session_id("glm-web", id),
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

// =============================================================================
// 6. GPT Web Adapter (ChatGPT data export)
// =============================================================================

fn is_gpt_file(root: &Path, path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !name.ends_with(".json") {
        return false;
    }
    // Exclude known non-conversation export artifacts
    if matches!(
        name.as_str(),
        "ads.json"
            | "codex.json"
            | "conversation_asset_file_names.json"
            | "export_manifest.json"
            | "library_files.json"
            | "sectioned_conversations.json"
            | "user_settings.json"
            | "user.json"
            | "message_feedback.json"
            | "shared_conversations.json"
    ) {
        return false;
    }
    const OTHER_VENDORS: [&str; 5] = ["claude", "deepseek", "gemini", "qwen", "glm"];
    for v in OTHER_VENDORS {
        if name.contains(v) {
            return false;
        }
    }
    // A bare `conversations.json` may be Claude's too; `gpt_top_level_items`
    // tells them apart by what is inside.
    name.starts_with("conversations") || filed_under(root, path, "gpt")
}

fn discover_gpt_files() -> (Vec<PathBuf>, bool) {
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
            if is_gpt_file(&root, &path) {
                files.push(path);
            }
        }
    }
    (files, had_error)
}

fn gpt_top_level_items(json: &Value) -> Option<&Vec<Value>> {
    let arr = json.as_array()?;
    // The message tree is what makes a chat. Other files of the export carry a
    // `conversation_id` as well, and each of them would become an empty session.
    match arr.first() {
        Some(first) if !first.get("mapping").is_some_and(Value::is_object) => None,
        _ => Some(arr),
    }
}

/// The answer as the reader saw it. ChatGPT keeps citations and widgets inline
/// as `\u{e200}kind\u{e202}payload\u{e201}` in private-use characters, and wraps
/// cited spans in more of them. Left in, they end up in search results.
fn strip_gpt_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('\u{e200}') {
        out.push_str(&rest[..start]);
        let inner = &rest[start + '\u{e200}'.len_utf8()..];
        let Some(end) = inner.find('\u{e201}') else {
            rest = inner;
            continue;
        };
        out.push_str(&gpt_markup_visible_text(&inner[..end]));
        rest = &inner[end + '\u{e201}'.len_utf8()..];
    }
    out.push_str(rest);
    out.retain(|c| !('\u{e000}'..='\u{f8ff}').contains(&c));
    out
}

/// What a marker shows in the sentence around it. Most show nothing of their
/// own (a citation, an image strip), but dropping a named entity or a link
/// would leave a hole in the sentence.
fn gpt_markup_visible_text(marker: &str) -> String {
    let mut fields = marker.split('\u{e202}');
    let kind = fields.next().unwrap_or("");
    let payload = fields.next().unwrap_or("");
    match kind {
        "url" | "link_title" => payload.to_string(),
        // `["type", "name", ...]`
        "entity" | "product_entity" => serde_json::from_str::<Value>(payload)
            .ok()
            .and_then(|v| v.get(1).and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default(),
        _ => String::new(),
    }
}

#[derive(Default)]
pub struct GptWeb {
    cache: JsonCache,
}

impl GptWeb {
    fn token(item: &Value, mtime: i64) -> i64 {
        item.get("update_time")
            .or_else(|| item.get("create_time"))
            .and_then(parse_epoch_millis)
            .unwrap_or(mtime)
    }
}

impl Adapter for GptWeb {
    fn name(&self) -> &'static str {
        "gpt-web"
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        let json = self.cache.get_or_load(path)?;
        if let Some(arr) = gpt_top_level_items(&json) {
            if let Some(first) = arr.first() {
                let id = native_id_of_item(first).unwrap_or("0");
                return parse_gpt_item(path, id, first);
            }
        }
        anyhow::bail!("empty or invalid gpt web export {}", path.display())
    }

    fn store(&self) -> Option<Store> {
        let (files, fs_error) = discover_gpt_files();
        let (keys, read_error) = scan_store(&self.cache, &files, gpt_top_level_items, Self::token);
        Some(Store {
            keys: keys.finish(),
            files,
            had_error: fs_error || read_error,
        })
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let (file_str, id) = key.split_once(KEY_SEP).context("malformed gpt-web key")?;
        let path = Path::new(file_str);
        let (json, at) = self.cache.locate(path, id, gpt_top_level_items)?;
        let item = item_at(&json, at, gpt_top_level_items)
            .context("gpt web export changed while it was read")?;
        parse_gpt_item(path, id, item)
    }
}

fn parse_gpt_item(path: &Path, id: &str, item: &Value) -> Result<Session> {
    let key = make_key(path, id);
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("new chat"))
        .map(|s| s.to_string());

    let started = item.get("create_time").and_then(parse_epoch);
    let ended = item.get("update_time").and_then(parse_epoch).or(started);

    let mut messages: Vec<Message> = Vec::new();
    let mut touched: Vec<String> = Vec::new();

    if let Some(mapping) = item.get("mapping").and_then(Value::as_object) {
        let mut ordered_node_ids = Vec::new();
        let mut visited = HashSet::new();
        let mut curr = item
            .get("current_node")
            .and_then(Value::as_str)
            .map(|s| s.to_string());

        while let Some(cid) = curr {
            let Some(node) = mapping.get(&cid) else {
                break;
            };
            if !visited.insert(cid.clone()) {
                break;
            }
            curr = node
                .get("parent")
                .and_then(Value::as_str)
                .map(|s| s.to_string());
            ordered_node_ids.push(cid);
        }
        ordered_node_ids.reverse();

        // Fallback: if traversal from current_node yielded nothing, sort all nodes by create_time
        if ordered_node_ids.is_empty() {
            let mut timed_nodes: Vec<(f64, String)> = Vec::new();
            for (nid, node) in mapping {
                let t = node
                    .get("message")
                    .and_then(|m| m.get("create_time"))
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                timed_nodes.push((t, nid.clone()));
            }
            timed_nodes.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            ordered_node_ids = timed_nodes.into_iter().map(|(_, nid)| nid).collect();
        }

        for nid in ordered_node_ids {
            let Some(node) = mapping.get(&nid) else {
                continue;
            };
            let Some(msg) = node.get("message").filter(|m| m.is_object()) else {
                continue;
            };

            let author_role = msg
                .get("author")
                .and_then(|a| a.get("role"))
                .and_then(Value::as_str)
                .unwrap_or("");

            let role = match author_role {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => Role::Tool,
                "system" => continue, // skip system prompts
                _ => continue,
            };

            // Custom instructions and other context the chat never showed.
            let hidden = msg
                .get("metadata")
                .and_then(|m| m.get("is_visually_hidden_from_conversation"))
                .and_then(Value::as_bool)
                == Some(true);
            if hidden {
                continue;
            }

            let ts = msg.get("create_time").and_then(parse_epoch).or(started);

            // Reasoning blocks from reasoning_of (if present in metadata or message fields)
            if let Some(reasoning) = reasoning_of(msg) {
                messages.push(Message::plain(Role::Assistant, reasoning, ts));
            }

            let Some(content) = msg.get("content").and_then(Value::as_object) else {
                continue;
            };
            let content_type = content
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("");

            match content_type {
                "thoughts" => {
                    if let Some(thoughts_arr) = content.get("thoughts").and_then(Value::as_array) {
                        let mut thoughts_text = Vec::new();
                        for th in thoughts_arr {
                            if let Some(c) = th.get("content").and_then(Value::as_str) {
                                let trimmed = c.trim();
                                if !trimmed.is_empty() {
                                    thoughts_text.push(trimmed);
                                }
                            }
                        }
                        if !thoughts_text.is_empty() {
                            messages.push(Message::plain(
                                Role::Assistant,
                                thoughts_text.join("\n\n"),
                                ts,
                            ));
                        }
                    }
                }
                // Only "Thought for 12 s": the thinking itself is in `thoughts`.
                "reasoning_recap" => {}
                "code" => {
                    if let Some(t) = content.get("text").and_then(Value::as_str) {
                        let language = content
                            .get("language")
                            .and_then(Value::as_str)
                            .filter(|l| *l != "unknown")
                            .unwrap_or("");
                        let code_formatted = format!("```{language}\n{}\n```", t.trim());
                        extract_code_block_paths(&code_formatted, &mut touched);
                        messages.push(Message::plain(Role::Tool, code_formatted, ts));
                    }
                }
                "execution_output" => {
                    if let Some(t) = content.get("text").and_then(Value::as_str) {
                        let out_formatted = format!("```\n{}\n```", t.trim());
                        messages.push(Message::plain(Role::Tool, out_formatted, ts));
                    }
                }
                _ => {
                    let mut text_parts = Vec::new();
                    if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(s) = part.as_str() {
                                let trimmed = s.trim();
                                if !trimmed.is_empty() {
                                    text_parts.push(trimmed.to_string());
                                }
                            } else if let Some(obj) = part.as_object() {
                                let p_type = obj
                                    .get("content_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                if p_type == "audio_transcription" {
                                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                                        let trimmed = t.trim();
                                        if !trimmed.is_empty() {
                                            text_parts.push(trimmed.to_string());
                                        }
                                    }
                                } else if p_type == "image_asset_pointer" {
                                    text_parts.push("[image]".to_string());
                                } else if p_type == "real_time_user_audio_video_asset_pointer" {
                                    if let Some(t) = obj
                                        .get("audio_asset_pointer")
                                        .and_then(|a| a.get("metadata"))
                                        .and_then(|m| m.get("transcription"))
                                        .and_then(Value::as_str)
                                    {
                                        let trimmed = t.trim();
                                        if !trimmed.is_empty() {
                                            text_parts.push(trimmed.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if text_parts.is_empty() {
                        if let Some(t) = content.get("text").and_then(Value::as_str) {
                            let trimmed = t.trim();
                            if !trimmed.is_empty() {
                                text_parts.push(trimmed.to_string());
                            }
                        }
                    }
                    let mut text = text_parts.join("\n\n");
                    if role == Role::Assistant {
                        text = strip_gpt_markup(&text).trim().to_string();
                    }
                    if !text.is_empty() {
                        extract_code_block_paths(&text, &mut touched);
                        messages.push(Message::plain(role, text, ts));
                    }
                }
            }
        }
    }

    let touched = dedup_paths(touched);
    let final_title = title.unwrap_or_else(|| title_from_messages(&messages));

    Ok(Session {
        id: web_session_id("gpt-web", id),
        tool: "gpt-web",
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

    // Synthetic exports shaped like the real vendor files, one chat with a
    // title and one without, so the fallback and the container of each vendor
    // are both read by every run. They live in tests/fixtures because a test
    // over someone's own chat history checks nothing: it passes silently when
    // the file is missing and fails for unrelated reasons when it is not.
    const CHAT1: &str = "11111111-aaaa-4bbb-8ccc-000000000001";
    const CHAT2: &str = "11111111-aaaa-4bbb-8ccc-000000000002";

    fn fixture(vendor: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/web_chat")
            .join(format!("{vendor}.json"))
    }

    fn parse(vendor: &str, adapter: &dyn Adapter, native: &str) -> Session {
        let path = fixture(vendor);
        adapter
            .parse_key(&make_key(&path, native))
            .unwrap_or_else(|e| panic!("{vendor} chat {native}: {e}"))
    }

    fn roles(session: &Session) -> Vec<Role> {
        session.messages.iter().map(|m| m.role).collect()
    }

    fn body<'a>(session: &'a Session, needle: &str) -> Option<&'a Message> {
        session.messages.iter().find(|m| m.text.contains(needle))
    }

    #[test]
    fn claude_chats_carry_their_thinking_and_the_paths_an_answer_names() {
        let first = parse("claude", &ClaudeWeb::default(), CHAT1);
        assert_eq!(first.tool, "claude-web");
        assert_eq!(first.title, "Fix the flaky upload test");
        assert_eq!(
            roles(&first),
            vec![Role::User, Role::Assistant, Role::Assistant, Role::User],
            "the reasoning block reads as its own turn, before the answer it produced"
        );
        assert!(
            body(&first, "usually means a fixed sleep").is_some(),
            "the thinking block was kept"
        );
        assert!(body(&first, "hard-coded sleep").is_some());
        assert_eq!(
            first.started.unwrap().to_rfc3339(),
            "2026-03-02T09:00:00+00:00"
        );

        let second = parse("claude", &ClaudeWeb::default(), CHAT2);
        assert!(
            second.title.starts_with("How do I list files"),
            "an unnamed chat is titled by its first prompt, got {:?}",
            second.title
        );
        assert_eq!(
            second.touched,
            vec!["D:/data/reports/summary.csv".to_string()],
            "a path the answer names is a file the chat touched"
        );
        assert_eq!(roles(&second), vec![Role::User, Role::Assistant]);
    }

    #[test]
    fn deepseek_walks_the_mapping_and_lists_its_attachments() {
        let adapter = DeepSeekWeb::default();
        let first = parse("deepseek", &adapter, "2222222222222222222222222222aaaa");
        assert_eq!(first.title, "Rotate the deploy keys");
        assert_eq!(
            roles(&first),
            vec![Role::User, Role::Assistant, Role::Assistant]
        );
        assert!(
            body(&first, "Two keys must be live at once").is_some(),
            "the message's reasoning_content was kept"
        );

        let second = parse("deepseek", &adapter, "2222222222222222222222222222bbbb");
        assert_eq!(roles(&second), vec![Role::User, Role::User]);
        assert!(
            body(&second, "images.txt").is_some(),
            "a FILE fragment names the attachment"
        );
    }

    #[test]
    fn gemini_uses_the_export_stamp_and_the_exporters_own_order() {
        let adapter = GeminiWeb::default();
        let first = parse("gemini", &adapter, "gemini-chat-0001");
        assert_eq!(first.title, "Summarise this changelog");
        assert_eq!(
            first.started.unwrap().to_rfc3339(),
            "2026-05-01T10:00:00+00:00",
            "the export stamp dates the chat, not the file it was read out into"
        );
        assert!(
            body(&first, "one line per theme").is_some(),
            "thinking kept"
        );

        let second = parse("gemini", &adapter, "gemini-chat-0002");
        assert_eq!(
            roles(&second),
            vec![Role::User, Role::Assistant],
            "the export wrote the model turn first; `order` says the prompt came before it"
        );
    }

    #[test]
    fn qwen_reads_the_wrapped_chats_and_prefers_the_plain_answer() {
        let adapter = QwenWeb::default();
        let first = parse("qwen", &adapter, "33333333-3333-4333-8333-000000000001");
        assert_eq!(first.title, "Postgres index bloat");
        assert_eq!(
            roles(&first),
            vec![Role::User, Role::Assistant, Role::Assistant]
        );
        assert!(
            body(&first, "update-heavy writes").is_some(),
            "reasoning_content was kept"
        );

        let second = parse("qwen", &adapter, "33333333-3333-4333-8333-000000000002");
        assert_eq!(roles(&second), vec![Role::User, Role::Assistant]);
        let repeated = second
            .messages
            .iter()
            .filter(|m| m.text.contains("docker compose logs"))
            .count();
        assert_eq!(
            repeated, 1,
            "a chat with both `content` and `content_list` is one answer, not two"
        );
    }

    #[test]
    fn glm_walks_the_reply_chain_and_falls_back_to_timestamps() {
        let adapter = GlmWeb::default();
        let first = parse("glm", &adapter, "44444444-4444-4444-8444-000000000001");
        assert_eq!(first.title, "Zig cross-compile for Android");
        assert_eq!(
            roles(&first),
            vec![Role::User, Role::Assistant, Role::Assistant]
        );
        assert!(
            body(&first, "people miss").is_some(),
            "reasoning_content was kept"
        );

        let second = parse("glm", &adapter, "44444444-4444-4444-8444-000000000002");
        assert_eq!(
            roles(&second),
            vec![Role::User, Role::Assistant],
            "with no currentId the chat is read in timestamp order"
        );
    }

    #[test]
    fn one_chat_is_one_session_whatever_file_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let reexported = dir.path().join("claude-2026-06.json");
        std::fs::copy(fixture("claude"), &reexported).unwrap();

        let adapter = ClaudeWeb::default();
        let original = parse("claude", &adapter, CHAT1);
        let moved = adapter.parse_key(&make_key(&reexported, CHAT1)).unwrap();
        assert_eq!(
            original.id, moved.id,
            "re-exporting a chat into a new file must not make a second session of it"
        );
        assert_ne!(
            original.path, moved.path,
            "each export still knows where it read the chat from"
        );
        assert_ne!(
            original.id,
            parse("claude", &adapter, CHAT2).id,
            "two chats of one export are two sessions"
        );
    }

    #[test]
    fn scan_prefers_the_newest_export_of_a_chat() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("claude-older.json");
        let newer = dir.path().join("claude-newer.json");
        write(&older, &chat(CHAT1, "2026-03-02T09:12:00.000Z", 3));
        write(&newer, &chat(CHAT1, "2026-06-11T09:12:00.000Z", 4));

        // Listed in either order, the same conversation comes back once and from
        // the export that has the newer timestamp.
        for files in [&[&older, &newer], &[&newer, &older]] {
            let owned: Vec<PathBuf> = files.iter().map(|p| (*p).clone()).collect();
            let (keys, had_error) = scan_store(
                &JsonCache::default(),
                &owned,
                top_level_items,
                ClaudeWeb::token,
            );
            assert!(!had_error);
            let listed = keys.finish();
            assert_eq!(listed.len(), 1, "one chat in two files is one session");
            assert_eq!(
                file_part_of_key(&listed[0].0),
                Some(newer.to_str().unwrap()),
                "the newest export of the chat is the one indexed"
            );
        }
    }

    #[test]
    fn an_equal_timestamp_goes_to_the_export_that_holds_more_of_the_chat() {
        let dir = tempfile::tempdir().unwrap();
        let short = dir.path().join("claude-short.json");
        let long = dir.path().join("claude-long.json");
        write(&short, &chat(CHAT1, "2026-03-02T09:12:00.000Z", 1));
        write(&long, &chat(CHAT1, "2026-03-02T09:12:00.000Z", 5));

        let owned = vec![short.clone(), long.clone()];
        let (keys, _) = scan_store(
            &JsonCache::default(),
            &owned,
            top_level_items,
            ClaudeWeb::token,
        );
        let listed = keys.finish();
        assert_eq!(file_part_of_key(&listed[0].0), Some(long.to_str().unwrap()));

        // The v4 Gemini exporter marks a chat it read whole, so a complete
        // export of one timestamp beats a truncated one.
        let mut truncated = chat(CHAT1, "2026-05-01T10:00:00.000Z", 2);
        truncated["complete"] = serde_json::json!(false);
        let mut complete = chat(CHAT1, "2026-05-01T10:00:00.000Z", 2);
        complete["complete"] = serde_json::json!(true);
        assert!(
            message_count_of(&complete) > message_count_of(&truncated),
            "the complete flag is what breaks the tie"
        );
    }

    #[test]
    fn tokens_come_from_the_chat_and_fall_back_to_the_file() {
        let updated = chat(CHAT1, "2026-03-02T09:12:00.000Z", 1);
        assert_eq!(
            ClaudeWeb::token(&updated, 7),
            DateTime::parse_from_rfc3339("2026-03-02T09:12:00.000Z")
                .unwrap()
                .timestamp_millis()
        );
        // An export with no stamp at all still compares by the file it came from.
        assert_eq!(ClaudeWeb::token(&serde_json::json!({}), 7), 7);
        assert_eq!(GeminiWeb::token(&serde_json::json!({}), 7), 7);

        let stamped = serde_json::json!({ "exported_at": "2026-05-01T10:00:00.000Z" });
        assert_eq!(
            GeminiWeb::token(&stamped, 7),
            DateTime::parse_from_rfc3339("2026-05-01T10:00:00.000Z")
                .unwrap()
                .timestamp_millis()
        );
        // ... and as milliseconds, the other spelling the exporter uses.
        assert_eq!(
            GeminiWeb::token(
                &serde_json::json!({ "exported_at": 1_777_000_000_000i64 }),
                7
            ),
            1_777_000_000_000
        );
    }

    #[test]
    fn reasoning_is_read_where_each_vendor_puts_it() {
        let as_block = serde_json::json!({
            "content": [{ "type": "thinking", "thinking": "in the block" },
                        { "type": "text", "text": "the answer" }]
        });
        assert_eq!(reasoning_of(&as_block).as_deref(), Some("in the block"));
        let as_field = serde_json::json!({ "reasoning_content": "in the field" });
        assert_eq!(reasoning_of(&as_field).as_deref(), Some("in the field"));
        assert_eq!(
            reasoning_of(&serde_json::json!({ "content": "plain" })),
            None
        );
        assert_eq!(
            reasoning_of(&serde_json::json!({ "reasoning_content": "   " })),
            None,
            "blank reasoning is not a turn"
        );
    }

    #[test]
    fn gpt_chats_walk_mapping_extract_thoughts_and_multimodal() {
        let adapter = GptWeb::default();
        let first = parse("gpt", &adapter, "33333333-aaaa-4bbb-8ccc-000000000001");
        assert_eq!(first.tool, "gpt-web");
        assert_eq!(first.title, "Optimize PostgreSQL Queries");
        assert_eq!(
            roles(&first),
            vec![Role::User, Role::Assistant, Role::Assistant]
        );
        assert!(
            body(&first, "indexes and analyzing query plans").is_some(),
            "the thoughts block was extracted as assistant turn"
        );
        assert_eq!(first.touched, vec!["src/db/queries.rs".to_string()]);

        let second = parse("gpt", &adapter, "33333333-aaaa-4bbb-8ccc-000000000002");
        assert!(second.title.starts_with("What is this image?"));
        assert_eq!(roles(&second), vec![Role::User, Role::Assistant]);
        assert!(
            body(&second, "[image]").is_some(),
            "the image asset pointer was rendered with placeholder"
        );
    }

    fn gpt_chat(native: &str) -> Session {
        parse(
            "gpt",
            &GptWeb::default(),
            &format!("33333333-aaaa-4bbb-8ccc-00000000000{native}"),
        )
    }

    #[test]
    fn gpt_reads_the_current_branch_and_the_real_code_language() {
        let chat = gpt_chat("3");
        assert!(body(&chat, "ABANDONED").is_none(), "an edited-away branch");
        assert!(body(&chat, "```rust\nfn main() {}\n```").is_some());
        assert!(body(&chat, "```\nraw\n```").is_some(), "language unknown");
        assert!(
            body(&chat, "Thought for").is_none(),
            "a recap is not an answer"
        );
    }

    #[test]
    fn gpt_current_node_outside_the_mapping_falls_back_to_every_node() {
        assert!(body(&gpt_chat("4"), "still readable").is_some());
    }

    #[test]
    fn gpt_times_read_whole_seconds_and_whole_milliseconds() {
        let expected = DateTime::<Utc>::from_timestamp(1_740_000_000, 0);
        assert_eq!(gpt_chat("5").started, expected);
        assert_eq!(gpt_chat("6").started, expected);
        assert_eq!(parse_epoch(&serde_json::json!("nan")), None);
        assert_eq!(
            parse_epoch_millis(&serde_json::json!(1_740_000_000.5)),
            Some(1_740_000_000_500)
        );
    }

    #[test]
    fn gpt_skips_context_the_chat_never_showed() {
        let chat = gpt_chat("7");
        assert!(body(&chat, "HIDDEN CONTEXT").is_none());
        assert_eq!(roles(&chat), vec![Role::User]);
    }

    #[test]
    fn gpt_markup_leaves_the_sentence_and_nothing_else() {
        assert_eq!(
            strip_gpt_markup(
                "\u{e203}Fact\u{e204}\u{e200}cite\u{e202}turn0search0\u{e201}\u{e206} by \
                 \u{e200}entity\u{e202}[\"org\",\"Acme\",\"a firm\"]\u{e201}, see \
                 \u{e200}url\u{e202}the docs\u{e202}https://x.test\u{e201}.\
                 \u{e200}image_group\u{e202}{\"query\":[\"a\"]}\u{e201}"
            ),
            "Fact by Acme, see the docs."
        );
        assert_eq!(strip_gpt_markup("cut \u{e200}cite"), "cut cite");
    }

    #[test]
    fn gpt_picker_wants_a_message_tree() {
        let tree = serde_json::json!([{ "id": "a", "mapping": {} }]);
        assert!(gpt_top_level_items(&tree).is_some());
        let feedback = serde_json::json!([{ "id": "a", "conversation_id": "a" }]);
        assert!(gpt_top_level_items(&feedback).is_none());
        let claude = serde_json::json!([{ "uuid": "a", "chat_messages": [] }]);
        assert!(gpt_top_level_items(&claude).is_none());
    }

    #[test]
    fn a_vendor_is_recognised_below_the_root_only() {
        let root = Path::new("/tools/gpt-claude/web_session");
        assert!(filed_under(root, &root.join("claude/a.json"), "claude"));
        assert!(filed_under(
            root,
            &root.join("Claude_export.json"),
            "claude"
        ));
        assert!(!filed_under(root, &root.join("a.json"), "claude"));

        assert!(is_gpt_file(root, &root.join("gpt/export.json")));
        assert!(is_gpt_file(root, &root.join("conversations-000.json")));
        assert!(!is_gpt_file(root, &root.join("notes.json")));
        assert!(!is_gpt_file(root, &root.join("gpt/user.json")));
        assert!(!is_gpt_file(root, &root.join("conversations_claude.json")));
    }

    #[test]
    fn a_store_key_split_back_out_is_the_key_it_came_from() {
        // index.rs reconciles the store with these halves: the file decides
        // whether a superseded export still exists, the id which chat won.
        let path = fixture("claude");
        let key = make_key(&path, CHAT1);
        assert_eq!(file_part_of_key(&key), Some(path.to_str().unwrap()));
        assert_eq!(native_part_of_key(&key), Some(CHAT1));
        assert_eq!(native_part_of_key("/store/plain.jsonl"), None);
    }

    /// A chat with `count` messages, stamped `updated_at`, in Claude's shape -
    /// enough for the store scan, which reads ids, stamps and message counts.
    fn chat(id: &str, updated_at: &str, count: usize) -> Value {
        let messages: Vec<Value> = (0..count)
            .map(|i| {
                serde_json::json!({
                    "sender": if i % 2 == 0 { "human" } else { "assistant" },
                    "text": format!("message {i}"),
                })
            })
            .collect();
        serde_json::json!({
            "uuid": id,
            "name": "One chat, two exports",
            "created_at": "2026-03-02T09:00:00.000Z",
            "updated_at": updated_at,
            "chat_messages": messages,
        })
    }

    fn write(path: &Path, item: &Value) {
        std::fs::write(path, serde_json::to_vec_pretty(&[item]).unwrap()).unwrap();
    }
}
