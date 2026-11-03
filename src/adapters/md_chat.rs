//! Web chats saved by hand as Markdown, dropped into the same `web_session`
//! folders as the JSON exports.
//!
//! The source comes from the file name alone. One tool can hold both JSON and
//! Markdown, and reconciliation treats a store as speaking for every row of its
//! tool, so both have to be listed by one adapter: [`WebSource`] wraps the JSON
//! adapter, if the tool has one, and adds the Markdown files to its listing.

use super::web_chat::{
    display_path, file_mtime_ms, file_part_of_key, make_key, native_part_of_key, web_session_id,
    web_session_roots,
};
use super::{ok_or_flag, redacted_truncate, title_from_messages, Adapter, Discovered, Store};
use crate::model::{Message, Role, Session};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, Utc};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// A file name with no known vendor in it. Ends in `-web` like the others, because
/// the web-chat filters and reconciliation recognise web chats by that suffix.
const OTHER_TOOL: &str = "other-web";

const NAMED_SOURCES: &[(&str, &str)] = &[
    ("qwen", "qwen-web"),
    ("gpt", "gpt-web"),
    ("claude", "claude-web"),
    ("glm", "glm-web"),
    ("gemini", "gemini-web"),
    ("deepseek", "deepseek-web"),
    ("arena", "arena-web"),
    ("minimax", "minimax-web"),
    ("grok", "grok-web"),
    ("kimi", "kimi-web"),
    ("muse", "muse-web"),
];

/// Sources that exist only as Markdown, with no JSON export format behind them.
pub(crate) const MARKDOWN_ONLY_TOOLS: &[&str] = &[
    "arena-web",
    "minimax-web",
    "grok-web",
    "kimi-web",
    "muse-web",
    OTHER_TOOL,
];

const TITLE_MAX_CHARS: usize = 120;

/// Enough for the front matter or `> From:` line of any export; the rest of a
/// file that may hold a megabyte of chat is not read until it has to be parsed.
const HEAD_BYTES: u64 = 8 * 1024;

/// The earliest keyword wins: an exporter puts the vendor first, so
/// `ChatGPT_Обзор-Claude.md` is a ChatGPT chat about Claude.
fn source_of_file(file_name: &str) -> &'static str {
    let lower = file_name.to_lowercase();
    NAMED_SOURCES
        .iter()
        .filter_map(|(keyword, tool)| lower.find(keyword).map(|at| (at, *tool)))
        .min_by_key(|(at, _)| *at)
        .map_or(OTHER_TOOL, |(_, tool)| tool)
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// One web source: its Markdown files, plus its JSON exports if it has any.
pub struct WebSource {
    tool: &'static str,
    json: Option<Box<dyn Adapter>>,
}

impl WebSource {
    pub fn with_exports(json: Box<dyn Adapter>) -> Self {
        Self {
            tool: json.name(),
            json: Some(json),
        }
    }

    pub fn markdown_only(tool: &'static str) -> Self {
        Self { tool, json: None }
    }
}

impl Adapter for WebSource {
    fn name(&self) -> &'static str {
        self.tool
    }

    fn root(&self) -> Option<PathBuf> {
        web_session_roots().into_iter().next()
    }

    fn discover(&self) -> Discovered {
        Vec::new().into()
    }

    fn parse(&self, path: &Path) -> Result<Session> {
        if is_markdown(path) {
            return parse_file(self.tool, path, &file_native_id(path));
        }
        match &self.json {
            Some(json) => json.parse(path),
            None => anyhow::bail!("{} reads only Markdown: {}", self.tool, path.display()),
        }
    }

    fn store(&self) -> Option<Store> {
        let json = self.json.as_ref().and_then(|json| json.store());
        let markdown = scan_markdown(self.tool, &web_session_roots());
        Some(merge_stores(json, markdown))
    }

    fn parse_key(&self, key: &str) -> Result<Session> {
        let file = file_part_of_key(key).context("malformed web chat key")?;
        if is_markdown(Path::new(file)) {
            let native = native_part_of_key(key).context("malformed web chat key")?;
            return parse_file(self.tool, Path::new(file), native);
        }
        match &self.json {
            Some(json) => json.parse_key(key),
            None => anyhow::bail!("{} reads only Markdown: {file}", self.tool),
        }
    }
}

/// One conversation, one key: the same rule the JSON exports follow, so a chat
/// saved both ways is listed once, from whichever file changed last. A tie goes
/// to the JSON export, which holds more than the Markdown can.
fn merge_stores(json: Option<Store>, markdown: Store) -> Store {
    let mut merged = json.unwrap_or(Store {
        keys: Vec::new(),
        files: Vec::new(),
        had_error: false,
    });
    let mut listed: HashMap<String, usize> = merged
        .keys
        .iter()
        .enumerate()
        .filter_map(|(at, (key, _))| native_part_of_key(key).map(|n| (n.to_string(), at)))
        .collect();
    for (key, token) in markdown.keys {
        let native = native_part_of_key(&key).unwrap_or(&key).to_string();
        match listed.get(&native) {
            Some(&at) => {
                if token > merged.keys[at].1 {
                    merged.keys[at] = (key, token);
                }
            }
            None => {
                listed.insert(native, merged.keys.len());
                merged.keys.push((key, token));
            }
        }
    }
    merged.files.extend(markdown.files);
    merged.had_error |= markdown.had_error;
    merged
}

fn scan_markdown(tool: &str, roots: &[PathBuf]) -> Store {
    let mut store = Store {
        keys: Vec::new(),
        files: Vec::new(),
        had_error: false,
    };
    let mut listed: HashMap<String, usize> = HashMap::new();
    let mut walk_error = false;
    for root in roots {
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_map(|e| ok_or_flag(e, &mut walk_error))
        {
            if !entry.file_type().is_file() || !is_markdown(entry.path()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if source_of_file(&name) != tool {
                continue;
            }
            let path = entry.path();
            let Ok(head) = read_head(path) else {
                store.had_error = true;
                continue;
            };
            // An empty file is a save still in progress, not a chat.
            if head.trim().is_empty() {
                continue;
            }
            let native = split_header(&head)
                .url
                .and_then(native_id_of_url)
                .unwrap_or_else(|| relative_native_id(root, path));
            let token = file_mtime_ms(path);
            let key = make_key(path, &native);
            store.files.push(path.to_path_buf());
            match listed.get(&native) {
                Some(&at) => {
                    if token > store.keys[at].1 {
                        store.keys[at] = (key, token);
                    }
                }
                None => {
                    listed.insert(native, store.keys.len());
                    store.keys.push((key, token));
                }
            }
        }
    }
    store.had_error |= walk_error;
    store
}

fn read_head(path: &Path) -> std::io::Result<String> {
    let mut raw = Vec::new();
    std::fs::File::open(path)?
        .take(HEAD_BYTES)
        .read_to_end(&mut raw)?;
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// The chat's id in its vendor's URL. It survives a second save under a new
/// timestamp, and it is the id the JSON export uses for the same chat. The last
/// path segment only counts if it looks like an id: `/chat` or `/new` would
/// merge every chat saved from that page.
fn native_id_of_url(url: &str) -> Option<String> {
    let without_query = url.split(['?', '#']).next()?;
    let (_, after_scheme) = without_query.split_once("://")?;
    let (_, path) = after_scheme.split_once('/')?;
    let last = path.rsplit('/').find(|segment| !segment.is_empty())?;
    let looks_like_id = last.len() >= 8
        && last.chars().any(|c| c.is_ascii_digit())
        && last
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    looks_like_id.then(|| last.to_string())
}

fn relative_native_id(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    format!(
        "file:{}",
        relative.to_string_lossy().replace('\\', "/").to_lowercase()
    )
}

fn file_native_id(path: &Path) -> String {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    format!("file:{}", name.to_lowercase())
}

struct Header<'a> {
    url: Option<&'a str>,
    title: Option<&'a str>,
    body: &'a str,
}

/// Peel the front matter (`source:`, `title:`) and the exporter's `> From:` line
/// off the top of a file.
fn split_header(text: &str) -> Header<'_> {
    let mut header = Header {
        url: None,
        title: None,
        body: text.trim_start_matches('\u{feff}').trim_start(),
    };
    if let Some((front, rest)) = split_front_matter(header.body) {
        for line in front.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_matches(['"', '\'']);
            match name.trim().to_ascii_lowercase().as_str() {
                "source" | "url" | "link" if value.contains("://") => header.url = Some(value),
                "title" if !value.is_empty() => header.title = Some(value),
                _ => {}
            }
        }
        header.body = rest.trim_start();
    }
    let first_line = header.body.lines().next().unwrap_or("");
    if let Some(url) = first_line
        .strip_prefix("> From:")
        .or_else(|| first_line.strip_prefix("> Source:"))
    {
        header.url = header.url.or(Some(url.trim()));
        header.body = header.body[first_line.len()..].trim_start();
    }
    header
}

/// `(inside, after)` of a leading `---` block. A horizontal rule at the top of
/// a document is not front matter, so the first line inside must be `key: value`.
fn split_front_matter(text: &str) -> Option<(&str, &str)> {
    const MAX_FRONT_MATTER_LINES: usize = 40;

    let first_line = text.lines().next()?;
    if first_line.trim_end() != "---" {
        return None;
    }
    let inside_start = first_line.len() + line_ending_len(text, first_line.len());
    let inside = &text[inside_start..];
    let first_inside = inside.lines().next()?;
    let (name, _) = first_inside.split_once(':')?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let mut offset = 0;
    for line in inside.lines().take(MAX_FRONT_MATTER_LINES) {
        if line.trim_end() == "---" {
            return Some((&inside[..offset], &inside[offset + line.len()..]));
        }
        offset += line.len();
        offset += line_ending_len(inside, offset);
    }
    None
}

fn line_ending_len(text: &str, at: usize) -> usize {
    let rest = &text.as_bytes()[at..];
    match rest {
        [b'\r', b'\n', ..] => 2,
        [b'\n', ..] => 1,
        _ => 0,
    }
}

/// The advert AI Exporter appends is not part of the conversation.
fn strip_exporter_footer(body: &str) -> &str {
    let trimmed = body.trim_end();
    let Some(rule_at) = trimmed.rfind("\n---") else {
        return trimmed;
    };
    let footer = trimmed[rule_at + "\n---".len()..].trim();
    if footer.starts_with("Powered by [AI Exporter]") && !footer.contains('\n') {
        trimmed[..rule_at].trim_end()
    } else {
        trimmed
    }
}

struct Turn {
    role: Role,
    text: String,
    ts: Option<DateTime<Utc>>,
}

/// The exporter's own turn markers: `# you asked` and `# chatgpt response`, each
/// after a `---`. A reply can contain any heading, so the rule and the vendor
/// name of the first reply are both required. `None` if this is another format.
fn split_exporter_turns(body: &str) -> Option<Vec<Turn>> {
    let mut turns: Vec<(Role, Vec<&str>)> = Vec::new();
    let mut response_marker: Option<&str> = None;
    let mut prev_line: Option<&str> = None;
    for line in body.lines() {
        let at_boundary = prev_line.is_none_or(|prev| prev == "---");
        let bare = line.trim_end();
        let starts = if !at_boundary {
            None
        } else if bare == "# you asked" {
            Some(Role::User)
        } else if is_response_marker(bare) && response_marker.is_none_or(|m| m == bare) {
            response_marker = Some(bare);
            Some(Role::Assistant)
        } else {
            None
        };
        if let Some(role) = starts {
            turns.push((role, Vec::new()));
        } else if let Some((_, lines)) = turns.last_mut() {
            lines.push(line);
        } else if !bare.is_empty() {
            return None;
        }
        if !bare.is_empty() {
            prev_line = Some(bare);
        }
    }
    if turns.is_empty() {
        return None;
    }
    Some(
        turns
            .into_iter()
            .map(|(role, lines)| exporter_turn(role, &lines))
            .filter(|turn| !turn.text.is_empty())
            .collect(),
    )
}

fn is_response_marker(line: &str) -> bool {
    const MAX_VENDOR_NAME_CHARS: usize = 40;

    line.strip_prefix("# ")
        .and_then(|rest| rest.strip_suffix(" response"))
        .is_some_and(|name| {
            !name.trim().is_empty()
                && name.chars().count() <= MAX_VENDOR_NAME_CHARS
                && !name.contains('#')
        })
}

fn exporter_turn(role: Role, lines: &[&str]) -> Turn {
    let mut lines = lines;
    while let [rest @ .., last] = lines {
        if last.trim().is_empty() || last.trim_end() == "---" {
            lines = rest;
        } else {
            break;
        }
    }
    while let [first, rest @ ..] = lines {
        if first.trim().is_empty() {
            lines = rest;
        } else {
            break;
        }
    }
    let ts = lines
        .first()
        .and_then(|line| line.strip_prefix("message time:"))
        .and_then(parse_local_time);
    if ts.is_some() {
        lines = &lines[1..];
    }
    Turn {
        role,
        text: lines.join("\n").trim().to_string(),
        ts,
    }
}

/// The exporter writes the browser's local time with no zone.
fn parse_local_time(text: &str) -> Option<DateTime<Utc>> {
    NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S")
        .ok()?
        .and_local_timezone(Local)
        .earliest()
        .map(|local| local.with_timezone(&Utc))
}

/// `User` / `## Assistant` / `**You said:**` / `ChatGPT:` alone on a line.
fn label_role(line: &str) -> Option<Role> {
    let trimmed = line.trim();
    let decorated = trimmed.starts_with('#')
        || (trimmed.starts_with("**") && trimmed.ends_with("**"))
        || trimmed.ends_with(':');
    if !decorated {
        return None;
    }
    let label = trimmed
        .trim_matches(|c: char| matches!(c, '#' | '*' | '_' | ':') || c.is_whitespace())
        .to_lowercase();
    let speaker = label
        .strip_suffix(" said")
        .or_else(|| label.strip_suffix(" сказали"))
        .unwrap_or(&label);
    match speaker {
        "user" | "you" | "human" | "пользователь" | "вы" => Some(Role::User),
        "assistant" | "ассистент" | "ai" | "model" | "модель" | "bot" | "chatgpt" => {
            Some(Role::Assistant)
        }
        other => NAMED_SOURCES
            .iter()
            .any(|(keyword, _)| *keyword == other)
            .then_some(Role::Assistant),
    }
}

/// Turns marked by speaker labels. A design document can have a `## User` section
/// too, so this only applies when the very first thing in the file (after at most
/// a title) is a label and both sides speak.
fn split_labelled_turns(body: &str) -> Option<Vec<Turn>> {
    let mut turns: Vec<Turn> = Vec::new();
    let mut in_fence = false;
    let mut seen_title = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        let role = if in_fence { None } else { label_role(line) };
        if let Some(role) = role {
            turns.push(Turn {
                role,
                text: String::new(),
                ts: None,
            });
        } else if let Some(turn) = turns.last_mut() {
            turn.text.push_str(line);
            turn.text.push('\n');
        } else if line.starts_with("# ") && !seen_title {
            seen_title = true;
        } else if !line.trim().is_empty() {
            return None;
        }
    }
    let both_speak = turns.iter().any(|t| t.role == Role::User)
        && turns.iter().any(|t| t.role == Role::Assistant);
    if !both_speak {
        return None;
    }
    for turn in &mut turns {
        turn.text = turn.text.trim().to_string();
    }
    turns.retain(|turn| !turn.text.is_empty());
    Some(turns)
}

/// With no way to tell who spoke, the file is kept whole as one reply: searchable
/// and readable, with nothing invented about who said what.
fn split_turns(body: &str) -> Vec<Turn> {
    if let Some(turns) = split_exporter_turns(body).or_else(|| split_labelled_turns(body)) {
        return turns;
    }
    let text = body.trim();
    if text.is_empty() {
        return Vec::new();
    }
    vec![Turn {
        role: Role::Assistant,
        text: text.to_string(),
        ts: None,
    }]
}

/// `ChatGPT_Дизайн-SaaS-платформы_20261103-2308` -> `Дизайн SaaS платформы`.
fn title_from_exporter_name(stem: &str) -> Option<String> {
    const DATE_SUFFIX_CHARS: usize = "_20261103-2308".len();

    let cut = stem.len().checked_sub(DATE_SUFFIX_CHARS)?;
    let (rest, suffix) = (stem.get(..cut)?, stem.get(cut..)?);
    let digits = suffix.strip_prefix('_')?;
    let date_shaped = digits.len() == 13
        && digits.as_bytes()[8] == b'-'
        && digits
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 8 || b.is_ascii_digit());
    if !date_shaped {
        return None;
    }
    let (_vendor, title) = rest.split_once('_')?;
    let title = title.replace(['-', '_'], " ");
    (!title.trim().is_empty()).then(|| title.trim().to_string())
}

fn title_of(stem: &str, header_title: Option<&str>, messages: &[Message]) -> String {
    if let Some(title) = header_title {
        return title.to_string();
    }
    if let Some(title) = title_from_exporter_name(stem) {
        return title;
    }
    if messages.iter().any(|m| m.role == Role::User) {
        return title_from_messages(messages);
    }
    // A browser's "save page" names the file after the site, so the document's
    // own heading says more about this particular chat than the name does.
    messages
        .first()
        .and_then(|m| m.text.lines().find_map(|line| line.strip_prefix("# ")))
        .map(str::trim)
        .filter(|heading| !heading.is_empty())
        .unwrap_or(stem)
        .to_string()
}

fn parse_file(tool: &'static str, path: &Path, native: &str) -> Result<Session> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8_lossy(&raw);
    let header = split_header(&text);
    let body = strip_exporter_footer(header.body);
    let turns = split_turns(body);

    let saved_at = DateTime::<Utc>::from_timestamp_millis(file_mtime_ms(path));
    let started = turns.iter().find_map(|turn| turn.ts).or(saved_at);
    let ended = turns.iter().rev().find_map(|turn| turn.ts).or(saved_at);
    let messages: Vec<Message> = turns
        .into_iter()
        .map(|turn| Message::plain(turn.role, turn.text, turn.ts))
        .collect();

    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let title = title_of(&stem, header.title, &messages);

    Ok(Session {
        id: web_session_id(tool, native),
        tool,
        path: PathBuf::from(display_path(&make_key(path, native))),
        project: "web".to_string(),
        started,
        ended,
        title: redacted_truncate(&title, TITLE_MAX_CHARS),
        subagent: false,
        parent_id: None,
        messages,
        // Numbered headings read as file names to the JSON adapters' path
        // heuristic, and a saved page rarely names real files anyway.
        touched: Vec::new(),
        edits: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORTED: &str = "> From: https://chatgpt.com/c/6ae7922b-08ac-83eb-8ff5-48a62f36cf09\n\n\
# you asked\n\nmessage time: 2026-11-02 00:25:49\n\nпервый вопрос\n\n---\n\n\
# chatgpt response\n\nответ\n\n---\n\n# Заголовок в ответе\n\nещё текст\n\n---\n\n\
# you asked\n\nmessage time: 2026-11-02 00:30:00\n\nвторой вопрос\n\n---\n\n\
# chatgpt response\n\nвторой ответ\n\n---\nPowered by [AI Exporter](https://saveai.net)\n";

    fn parse_body(text: &str) -> Vec<Turn> {
        split_turns(strip_exporter_footer(split_header(text).body))
    }

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn the_source_comes_from_the_file_name() {
        for (name, tool) in [
            ("Qwen_chat.md", "qwen-web"),
            ("ChatGPT_Дизайн_20261103-2308.md", "gpt-web"),
            ("CLAUDE.md", "claude-web"),
            ("Z - Advanced AI powered by GLM-5.3-Flash.md", "glm-web"),
            ("gemini.md", "gemini-web"),
            ("DeepSeek_x.md", "deepseek-web"),
            ("lmarena-chat.md", "arena-web"),
            ("MiniMax.md", "minimax-web"),
            ("Grok 4.md", "grok-web"),
            ("kimi k2.md", "kimi-web"),
            ("Muse_x.md", "muse-web"),
            ("Заметки про код.md", "other-web"),
        ] {
            assert_eq!(source_of_file(name), tool, "{name}");
        }
    }

    #[test]
    fn the_first_vendor_in_the_name_wins_over_a_later_mention() {
        assert_eq!(source_of_file("ChatGPT_Обзор-Claude.md"), "gpt-web");
        assert_eq!(source_of_file("Claude_Обзор-ChatGPT.md"), "claude-web");
    }

    #[test]
    fn exporter_turns_keep_their_roles_times_and_inner_rules() {
        let turns = parse_body(EXPORTED);
        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
        assert_eq!(turns[0].text, "первый вопрос");
        assert!(turns[0].ts.is_some());
        assert!(
            turns[1].text.contains("---\n\n# Заголовок в ответе"),
            "a rule and a heading inside a reply stay in that reply: {}",
            turns[1].text
        );
        assert_eq!(turns[3].text, "второй ответ", "the advert is dropped");
    }

    #[test]
    fn a_heading_that_only_resembles_a_marker_stays_in_the_reply() {
        let text = "# you asked\n\nвопрос\n\n---\n\n# claude response\n\nответ\n\n---\n\n\
# final response\n\nэто раздел ответа\n";
        let turns = parse_body(text);
        assert_eq!(turns.len(), 2);
        assert!(turns[1].text.contains("# final response"));
    }

    #[test]
    fn consecutive_turns_of_one_role_are_kept_apart() {
        let text =
            "# you asked\n\nраз\n\n---\n\n# you asked\n\nдва\n\n---\n\n# qwen response\n\nда\n";
        let turns = parse_body(text);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[1].text, "два");
    }

    #[test]
    fn a_saved_page_with_no_roles_is_kept_whole_under_its_own_heading() {
        let text = "---\nsource: https://chat.z.ai/c/199effbe-18b9-463d-9036-9cd71b40fe06\n\
description: \"x: y\"\n---\n\n# V17 VERDICT\n\nтекст\n\n---\n\n# СВОДКА\n\nещё\n";
        let header = split_header(text);
        assert_eq!(
            header.url.and_then(native_id_of_url).as_deref(),
            Some("199effbe-18b9-463d-9036-9cd71b40fe06")
        );
        assert!(!header.body.contains("source:"));
        let turns = parse_body(text);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, Role::Assistant);
        assert!(turns[0].text.contains("# СВОДКА"));
    }

    #[test]
    fn speaker_labels_split_a_dialogue() {
        let text = "# Чат про код\n\n**You said:**\n\nвопрос\n\n```yaml\nuser:\n```\n\n\
**ChatGPT said:**\n\nответ\n\nUser:\n\nещё вопрос\n\nAssistant:\n\nещё ответ\n";
        let turns = parse_body(text);
        let roles: Vec<Role> = turns.iter().map(|t| t.role).collect();
        assert_eq!(
            roles,
            [Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
        assert!(
            turns[0].text.contains("user:"),
            "a label in code is not a turn"
        );
    }

    #[test]
    fn a_document_with_user_and_model_sections_is_not_a_dialogue() {
        let text = "# Схема данных\n\nВведение.\n\n## User\n\nполя\n\n## Model\n\nполя\n";
        let turns = parse_body(text);
        assert_eq!(turns.len(), 1);
        assert!(turns[0].text.contains("## User"));
    }

    #[test]
    fn only_a_real_looking_url_segment_identifies_a_chat() {
        for (url, expected) in [
            (
                "https://gemini.google.com/u/1/app/b0807785f619f088?hl=ru&pageId=none",
                Some("b0807785f619f088"),
            ),
            (
                "https://chat.deepseek.com/a/chat/s/0e666b26-1928-422d-9df4-27c9bc88d545/",
                Some("0e666b26-1928-422d-9df4-27c9bc88d545"),
            ),
            ("https://grok.com/chat", None),
            ("https://example.com/", None),
            ("not a url", None),
        ] {
            assert_eq!(native_id_of_url(url).as_deref(), expected, "{url}");
        }
    }

    #[test]
    fn titles_prefer_the_exporter_name_then_the_chat_then_the_file() {
        assert_eq!(
            title_from_exporter_name("ChatGPT_Дизайн-SaaS-платформы_20261103-2308").as_deref(),
            Some("Дизайн SaaS платформы")
        );
        assert_eq!(title_from_exporter_name("Просто заметка"), None);
        let reply = [Message::plain(Role::Assistant, "без заголовка", None)];
        assert_eq!(title_of("имя файла", None, &reply), "имя файла");
        let heading = [Message::plain(Role::Assistant, "# Вердикт\n\nтекст", None)];
        assert_eq!(title_of("имя файла", None, &heading), "Вердикт");
    }

    #[test]
    fn parse_file_builds_a_session_with_the_chats_own_times() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "ChatGPT_Дизайн-платформы_20261103-2308.md",
            EXPORTED,
        );
        let session = parse_file("gpt-web", &path, "6ae7922b").unwrap();
        assert_eq!(session.tool, "gpt-web");
        assert_eq!(session.title, "Дизайн платформы");
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.project, "web");
        assert!(session.started.unwrap() < session.ended.unwrap());
    }

    #[test]
    fn a_later_save_of_one_chat_replaces_the_earlier_and_other_tools_keep_out() {
        let dir = tempfile::tempdir().unwrap();
        let old = write(dir.path(), "ChatGPT_Чат_20261101-1000.md", EXPORTED);
        let new = write(dir.path(), "ChatGPT_Чат_20261103-1000.md", EXPORTED);
        write(dir.path(), "Qwen_Чат.md", "# you asked\n\nx\n");
        write(dir.path(), "chatgpt-empty.md", "  \n");
        let file = std::fs::File::options().write(true).open(&old).unwrap();
        file.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000))
            .unwrap();

        let store = scan_markdown("gpt-web", &[dir.path().to_path_buf()]);
        assert!(!store.had_error);
        assert_eq!(store.keys.len(), 1, "one chat, one key: {:?}", store.keys);
        assert_eq!(
            file_part_of_key(&store.keys[0].0),
            Some(new.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn a_chat_saved_both_ways_is_listed_once_from_the_newer_file() {
        let json = |token| Store {
            keys: vec![(make_key(Path::new("a.json"), "chat-1"), token)],
            files: vec![PathBuf::from("a.json")],
            had_error: false,
        };
        let markdown = || Store {
            keys: vec![
                (make_key(Path::new("a.md"), "chat-1"), 50),
                (make_key(Path::new("b.md"), "chat-2"), 50),
            ],
            files: vec![PathBuf::from("a.md"), PathBuf::from("b.md")],
            had_error: false,
        };

        let merged = merge_stores(Some(json(10)), markdown());
        assert_eq!(merged.keys.len(), 2);
        assert_eq!(file_part_of_key(&merged.keys[0].0), Some("a.md"));

        let merged = merge_stores(Some(json(50)), markdown());
        assert_eq!(
            file_part_of_key(&merged.keys[0].0),
            Some("a.json"),
            "a tie goes to the richer format"
        );
    }

    #[test]
    fn a_source_reads_its_markdown_keys_and_refuses_unknown_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "muse-chat.md", "просто текст\n");
        let source = WebSource::markdown_only("muse-web");
        let session = source
            .parse_key(&make_key(&path, "file:muse-chat.md"))
            .unwrap();
        assert_eq!(session.tool, "muse-web");
        assert_eq!(session.messages.len(), 1);
        assert!(source
            .parse_key(&make_key(Path::new("x.json"), "1"))
            .is_err());
    }
}
