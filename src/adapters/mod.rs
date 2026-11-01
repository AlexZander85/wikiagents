mod aider;
mod antigravity;
mod claude_code;
mod cline;
mod codex;
mod continue_dev;
mod deepseek;
mod free_pi;
mod freebuff;
mod gajae;
mod gemini;
mod gptme;
pub mod harness;
mod hermes;
mod kimi;
mod opencode;
mod prodex;
mod qoder;
pub mod web_chat;
mod zcode;
pub use antigravity::Antigravity;
pub use claude_code::ClaudeCode;
pub(crate) use codex::is_subagent_meta as codex_is_subagent_meta;
pub use codex::{Codex, CodexHistory};
pub use deepseek::DeepSeek;
pub use free_pi::{FreePiCli, Pi};
pub use freebuff::Freebuff;
pub(crate) use gemini::project_root as gemini_project_root;
pub use hermes::Hermes;
pub use kimi::Kimi;
pub use prodex::thread_url_for_task as prodex_thread_url;
pub use qoder::Qoder;
pub use web_chat::{ClaudeWeb, DeepSeekWeb, GeminiWeb, GlmWeb, GptWeb, QwenWeb};
pub use zcode::ZCode;

use crate::model::{Session, StoreReport};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};

/// One supported agent tool. An adapter knows where the tool keeps its
/// session files on disk and how to parse one file into a `Session`.
///
/// Adding support for a new tool means implementing this trait in a new
/// module and registering it in `all()`. PRs for new adapters are the main
/// way this project grows.
/// A store that holds many sessions in one place (e.g. a SQLite database)
/// rather than one file per session. Returned by [`Adapter::store`] when the
/// file-per-session model does not fit; the indexer then enumerates sessions
/// from `keys` (re-parsing only changed ones) instead of `discover`/`parse`.
pub struct Store {
    /// `(stable key, change-token)` for every session, cheap to compute without
    /// a full parse. The change-token (e.g. the session's updated-time in ms)
    /// drives incremental re-indexing; the key identifies the session for
    /// [`Adapter::parse_key`]. The key doubles as the session's stored path.
    pub keys: Vec<(String, i64)>,
    /// The backing files (the database), for `scan` size accounting.
    pub files: Vec<PathBuf>,
    /// True if a backing store existed but could not be read this run (locked,
    /// half-written, permissions). The indexer then skips deletion
    /// reconciliation, so a transient read failure cannot archive the whole
    /// corpus off an incomplete key set.
    pub had_error: bool,
}

/// The result of file discovery. `had_error` means the listing is PARTIAL: a
/// directory that exists could not be read (permissions, transient IO). The
/// indexer still indexes what was found but skips deletion reconciliation, so
/// a partial walk cannot archive live sessions off an incomplete listing - the
/// same guard [`Store::had_error`] gives shared-store adapters. A root that
/// simply does not exist on this machine is normal, not an error.
pub struct Discovered {
    pub files: Vec<PathBuf>,
    pub had_error: bool,
}

impl From<Vec<PathBuf>> for Discovered {
    fn from(files: Vec<PathBuf>) -> Self {
        Discovered {
            files,
            had_error: false,
        }
    }
}

/// `result.ok()` that records the failure in `had_error` instead of dropping
/// it silently - for walk loops that must report a partial listing.
pub(crate) fn ok_or_flag<T, E>(r: std::result::Result<T, E>, had_error: &mut bool) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(_) => {
            *had_error = true;
            None
        }
    }
}

pub trait Adapter {
    fn name(&self) -> &'static str;
    /// Store root, e.g. ~/.claude/projects. May not exist on this machine.
    fn root(&self) -> Option<PathBuf>;
    /// All session files under the root, with a partial-walk flag the indexer
    /// uses to protect deletion reconciliation (see [`Discovered`]).
    fn discover(&self) -> Discovered;
    /// Parse one session file. Must never panic on malformed input;
    /// skip bad lines and return what could be read.
    fn parse(&self, path: &Path) -> Result<Session>;
    /// A shared store (many sessions in one database) for tools that do not use
    /// one file per session. When `Some`, the indexer uses it instead of
    /// `discover`/`parse`. Default `None` = ordinary file-per-session adapter.
    fn store(&self) -> Option<Store> {
        None
    }
    /// Parse one session out of a shared store by its key (from `store().keys`).
    /// Only called for adapters that return a [`Store`].
    fn parse_key(&self, _key: &str) -> Result<Session> {
        anyhow::bail!("this adapter is not a shared store")
    }
    /// Limit deletion reconciliation to part of this tool's indexed rows. When
    /// `Some(prefix)`, only indexed rows whose key starts with `prefix` are
    /// considered for archiving after this adapter runs. Use it when the
    /// adapter's store holds only part of a tool's sessions - for example
    /// several installations of the same tool sharing one tool name, each
    /// listing only its own keys - so that one installation's sync cannot
    /// archive another's rows. Default `None` = the adapter speaks for every
    /// row of its tool.
    fn reconcile_scope(&self) -> Option<String> {
        None
    }
}

/// Parse a session through an adapter and normalize any structured tool parts.
/// Callers that consume adapter output should use this rather than `parse`
/// directly so indexing and live readers share identical tool summaries.
pub fn parse_session(adapter: &dyn Adapter, path: &Path) -> Result<Session> {
    let mut session = adapter.parse(path)?;
    crate::tool_summary::fold_tool_parts(&mut session);
    Ok(session)
}

/// Parse one key from a shared-store adapter and apply the same normalization
/// as the ordinary file-backed path.
pub fn parse_session_key(adapter: &dyn Adapter, key: &str) -> Result<Session> {
    let mut session = adapter.parse_key(key)?;
    crate::tool_summary::fold_tool_parts(&mut session);
    Ok(session)
}

/// The [`Adapter::reconcile_scope`] for an adapter pinned to `root`: every key
/// under that directory, as a path prefix.
pub(crate) fn root_scope(root: Option<&Path>) -> Option<String> {
    let root = root?;
    let mut prefix = root.to_string_lossy().into_owned();
    if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
        prefix.push(std::path::MAIN_SEPARATOR);
    }
    Some(prefix)
}

pub fn all() -> Vec<Box<dyn Adapter>> {
    let mut adapters: Vec<Box<dyn Adapter>> = vec![
        Box::new(ClaudeCode::default()),
        Box::new(Codex::default()),
        Box::new(Codex::archived()),
        Box::new(CodexHistory::default()),
        Box::new(gemini::Gemini),
        Box::new(antigravity::Antigravity::default()),
        Box::new(antigravity::Antigravity::cli()),
        Box::new(opencode::OpenCode),
        Box::new(cline::Cline),
        Box::new(cline::RooCode),
        Box::new(cline::KiloCode),
        Box::new(gajae::GajaeCode),
        Box::new(continue_dev::Continue),
        Box::new(gptme::Gptme),
        Box::new(aider::Aider::default()),
        Box::new(prodex::Prodex),
        Box::new(qoder::Qoder),
        Box::new(zcode::ZCode),
        Box::new(deepseek::DeepSeek),
        Box::new(hermes::Hermes),
        Box::new(kimi::Kimi),
        Box::new(freebuff::Freebuff),
        Box::new(free_pi::FreePiCli),
        Box::new(free_pi::Pi),
        Box::new(web_chat::ClaudeWeb::default()),
        Box::new(web_chat::DeepSeekWeb::default()),
        Box::new(web_chat::GeminiWeb::default()),
        Box::new(web_chat::QwenWeb::default()),
        Box::new(web_chat::GlmWeb::default()),
        Box::new(web_chat::GptWeb::default()),
    ];

    if let Some(home) = crate::util::home_dir() {
        let accounts_dir = home.join(".claude-accounts");
        if accounts_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(accounts_dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        let projects = entry.path().join("projects");
                        if projects.is_dir() {
                            adapters.push(Box::new(ClaudeCode::in_home(entry.path())));
                        }
                    }
                }
            }
        }
    }

    adapters
}

pub fn by_name(name: &str) -> Option<Box<dyn Adapter>> {
    let canonical = match name {
        "free-pi" => "free-pi-cli",
        "claude-web" | "claudeweb" => "claude-web",
        "deepseek-web" | "deepseekweb" => "deepseek-web",
        "gemini-web" | "geminiweb" => "gemini-web",
        "qwen-web" | "qwenweb" => "qwen-web",
        "glm-web" | "glmweb" | "glm" => "glm-web",
        "gpt-web" | "gptweb" | "gpt_web" | "chatgpt-web" | "chatgptweb" | "chatgpt_web"
        | "chatgpt" | "gpt" => "gpt-web",
        other => other,
    };
    all().into_iter().find(|a| a.name() == canonical)
}

/// Every registered store of one tool. Codex has three: its rollouts, its
/// archived rollouts and its thread history; [`by_name`] returns the first.
pub fn named(name: &str) -> Vec<Box<dyn Adapter>> {
    all().into_iter().filter(|a| a.name() == name).collect()
}

/// The tool slugs this build can index, deduplicated (several adapters share
/// one slug) and sorted so a version sweep visits them in a stable order.
pub fn tool_names() -> Vec<&'static str> {
    let mut names: std::collections::BTreeSet<&'static str> = std::collections::BTreeSet::new();
    for adapter in all() {
        names.insert(adapter.name());
    }
    names.into_iter().collect()
}

/// Per-tool overrides of the global reader version, as `(slug, version)`.
/// Add a line here to re-parse ONE tool after a parser fix; everything not
/// listed sits at the current version.
const READER_OVERRIDES: &[(&str, i64)] = &[
    // 4: prefer `transcript_full.jsonl` over `transcript.jsonl` so untruncated
    // text, complete tables, and diagrams are preserved.
    ("antigravity", 4),
    // 4: the web adapters now take a chat's id from the chat itself instead of
    // from the export file, and keep the model's thinking blocks. Only these
    // tools need their sessions read again.
    ("claude-web", 4),
    ("deepseek-web", 4),
    ("gemini-web", 4),
    ("qwen-web", 4),
    ("glm-web", 4),
    // 5: gpt-web drops citation markup, recaps and hidden context, and reads
    // whole-number times; chats indexed before that need reading again.
    ("gpt-web", 5),
];

/// The tools whose parser version is deliberately not the global one, so a test
/// can tell an expected override from a stray one.
pub fn reader_overrides() -> &'static [(&'static str, i64)] {
    READER_OVERRIDES
}

/// The parser version whose rows a tool's sessions should have. Stamped into
/// `files.reader` on index, and compared against it by
/// `index::reparse_stale_reader_versions`.
///
/// History: 1 = the original global reader; 2 = 1.3, so commit extraction
/// (shipped in 1.2.0) reaches sessions indexed before it, and web-chat
/// transcripts pick up thinking blocks; 3 = antigravity only, for its escaped
/// project paths. Raise it whenever a parser changes - sessions that never
/// change on disk are otherwise parsed exactly once.
pub fn reader_version(tool: &str) -> i64 {
    READER_OVERRIDES
        .iter()
        .find(|(slug, _)| *slug == tool)
        .map_or(crate::index::READER, |(_, version)| *version)
}

/// Filesystem-only summary of a store, used by `scan`. No parsing involved.
pub fn report(adapter: &dyn Adapter) -> Option<StoreReport> {
    let root = adapter.root()?;
    // Shared store (e.g. SQLite): presence comes from the store itself (the db
    // can live outside `root` via OPENCODE_DB), so this is checked before the
    // root-exists gate. Size is the backing files, count is the sessions, and
    // the time span comes from the per-session change-tokens.
    if let Some(store) = adapter.store() {
        if store.keys.is_empty() {
            return None; // present but no sessions yet - not worth a scan row
        }
        let bytes = store
            .files
            .iter()
            .filter_map(|f| f.metadata().ok())
            .map(|m| m.len())
            .sum();
        // Tokens are last-activity ms; drop the 0 sentinel (a missing timestamp)
        // so it cannot backdate the span to 1970.
        let oldest = store
            .keys
            .iter()
            .map(|(_, t)| *t)
            .filter(|t| *t > 0)
            .min()
            .and_then(DateTime::from_timestamp_millis);
        let newest = store
            .keys
            .iter()
            .map(|(_, t)| *t)
            .filter(|t| *t > 0)
            .max()
            .and_then(DateTime::from_timestamp_millis);
        return Some(StoreReport {
            tool: adapter.name(),
            root,
            files: store.keys.len(),
            bytes,
            oldest,
            newest,
        });
    }
    if !root.exists() {
        return None;
    }
    let files = adapter.discover().files;
    let mut bytes: u64 = 0;
    let mut oldest: Option<DateTime<Utc>> = None;
    let mut newest: Option<DateTime<Utc>> = None;
    let mut count = 0usize;
    for f in &files {
        let Ok(meta) = f.metadata() else { continue };
        count += 1;
        bytes += meta.len();
        if let Ok(modified) = meta.modified() {
            let t: DateTime<Utc> = modified.into();
            if oldest.is_none_or(|o| t < o) {
                oldest = Some(t);
            }
            if newest.is_none_or(|n| t > n) {
                newest = Some(t);
            }
        }
    }
    if count == 0 {
        return None; // root exists but holds no session files
    }
    Some(StoreReport {
        tool: adapter.name(),
        root,
        files: count,
        bytes,
        oldest,
        newest,
    })
}

/// Shared helper: pick a session title from messages when the tool does not
/// store one. First user message that is not harness boilerplate wins.
pub(crate) fn title_from_messages(messages: &[crate::model::Message]) -> String {
    messages
        .iter()
        .find(|m| {
            m.role == crate::model::Role::User
                && !m.text.trim_start().starts_with('<')
                && !m.text.trim().is_empty()
        })
        .map(|m| redacted_truncate(&m.text, 80))
        .unwrap_or_else(|| "(no user prompt)".into())
}

/// Redact the complete logical value before bounding a derived field. The
/// order matters: truncating first can leave a credential prefix too short for
/// the redactor to recognize.
pub(crate) fn redacted_truncate(text: &str, max: usize) -> String {
    crate::util::truncate(&crate::redact::redact(text), max)
}

const MAX_TOOL_OUTPUT_BYTES: usize = 8 * 1024;

/// Redact a complete tool result, then cap it without splitting a UTF-8 code
/// point. Redacting before truncation keeps credential detection intact.
pub(crate) fn bounded_redacted_output(text: &str) -> String {
    let redacted = crate::redact::redact(text);
    let mut end = redacted.len().min(MAX_TOOL_OUTPUT_BYTES);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_owned()
}

/// First-line title variant for adapters whose existing format uses a hard
/// character cap without an ellipsis. Redaction happens before line selection
/// so a multi-line credential is considered as one logical value.
pub(crate) fn redacted_first_line(text: &str, max: usize) -> String {
    let redacted = crate::redact::redact(text);
    redacted
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(max)
        .collect()
}

pub(crate) fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// Path hygiene shared by `touched` and `edits`: trim, and reject the shapes
/// that are not a real edited path (empty, embedded newline, absurdly long).
/// Returns the cleaned path or None to drop it, so the two provenance layers
/// apply IDENTICAL rules and never diverge.
pub(crate) fn clean_path(p: &str) -> Option<String> {
    let p = p.trim();
    (!p.is_empty() && !p.contains('\n') && p.len() <= 4096).then(|| p.to_string())
}

/// Tidy the list of files a session touched: `clean_path` each, then de-duplicate
/// while preserving first-seen order. A session edits the same file many times;
/// the link cares only that it did.
pub(crate) fn dedup_paths(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter_map(|p| clean_path(&p))
        .filter(|p| seen.insert(p.clone()))
        .collect()
}
