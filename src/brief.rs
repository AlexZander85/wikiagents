//! Extractive session briefing, no model involved.
//!
//! A long transcript is cut into turns (one user request and the work that
//! followed it), every sentence is scored for how much it says (rare terms of
//! this session, paths and identifiers, decision and result cues, position)
//! and picked with maximal marginal relevance so the brief does not repeat
//! itself. Tool calls are not quoted but counted: repeated commands collapse
//! into one line with their last outcome, errors are grouped by signature and
//! marked fixed when the same command later succeeded. The last exchange is
//! kept verbatim at the end, so whatever the heuristics miss, the current
//! state is still there.

use crate::model::{Role, Session, ToolEvent};
use std::collections::{HashMap, HashSet};

/// Sessions shorter than this many turns whose transcript fits the budget are
/// returned as the transcript itself: there is nothing to condense.
const MIN_TURNS_TO_CONDENSE: usize = 3;

/// The briefing for `session` within roughly `max_chars`. Falls back to the
/// head-and-tail transcript for tiny sessions. Every piece of text is run
/// through the credential redactor before it is cut or scored.
pub fn smart_brief(
    session: &Session,
    max_chars: usize,
    include_tools: bool,
    include_source: bool,
) -> String {
    let turns = segment(session);
    let raw_len: usize = session
        .messages
        .iter()
        .filter(|m| !m.is_rewound)
        .map(|m| m.text.len() + 16)
        .sum();
    if turns.len() < MIN_TURNS_TO_CONDENSE && raw_len <= max_chars {
        return crate::commands::brief_text(session, max_chars, include_tools, include_source);
    }
    let out = Brief::new(session, turns, max_chars.max(1500)).render(include_tools, include_source);
    crate::redact::redact(&out).into_owned()
}

/// One line for the session list: the request, what came of it, and counts -
/// `Fix the CSV parser → quoted fields now parse · 4 turns · ✎3 · ⚠1`. The
/// same analysis as [`smart_brief`], cut to its headline; instant, no model.
/// `None` when the session has no request to speak of.
pub fn short_summary(session: &Session) -> Option<String> {
    let turns = segment(session);
    let line = Brief::new(session, turns, 0).headline()?;
    Some(crate::redact::redact(&line).into_owned())
}

// ---------------------------------------------------------------------------
// Turns

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok,
    Error,
    Pending,
}

#[derive(Debug, Clone)]
struct ToolLine {
    name: String,
    arg: String,
    status: Status,
    error: String,
    output: Option<String>,
}

#[derive(Debug, Default)]
struct Turn {
    request: String,
    /// A bare `/command` (`/compact`, `/handoff:write`): it opens a turn, but
    /// says nothing about the work, so it is never picked to stand for it.
    slash: bool,
    replies: Vec<String>,
    tools: Vec<ToolLine>,
}

fn segment(session: &Session) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for m in session.messages.iter().filter(|m| !m.is_rewound) {
        let text = crate::redact::redact(m.text.trim()).into_owned();
        match m.role {
            Role::User => {
                let cleaned = clean(&text);
                if is_substantive_request(&cleaned) {
                    let slash =
                        cleaned.starts_with('/') && !cleaned.trim().contains(char::is_whitespace);
                    turns.push(Turn {
                        request: cleaned,
                        slash,
                        ..Default::default()
                    });
                }
            }
            Role::Assistant => {
                let cleaned = clean(&text);
                if !cleaned.is_empty() {
                    current(&mut turns).replies.push(cleaned);
                }
            }
            Role::Tool => {
                if let Some(line) = parse_tool(&text, m.tool.as_ref()) {
                    current(&mut turns).tools.push(line);
                }
            }
        }
    }
    turns
}

fn current(turns: &mut Vec<Turn>) -> &mut Turn {
    if turns.is_empty() {
        turns.push(Turn::default());
    }
    turns.last_mut().unwrap()
}

fn is_substantive_request(text: &str) -> bool {
    let words = text.split_whitespace().count();
    words >= 1
        && text.chars().filter(|c| c.is_alphanumeric()).count() >= 2
        && !HARNESS_REQUESTS.iter().any(|p| text.starts_with(p))
}

/// User-role messages the harness wrote, not the person.
const HARNESS_REQUESTS: &[&str] = &[
    "[Request interrupted",
    "Caveat:",
    "This session is being continued from a previous conversation",
    "Base directory for this skill:",
    "[SYSTEM NOTIFICATION",
    "None of the sessions you started",
];

/// Harness wrappers that are not what the person wrote or the agent said.
const NOISE_TAGS: &[&str] = &[
    "system-reminder",
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-message",
    "command-args",
    "task-notification",
    "ide_selection",
    "ide_opened_file",
    "user-prompt-submit-hook",
    "environment_details",
    "environment_context",
    "user_instructions",
    "permissions instructions",
    "turn_aborted",
    "side-sessions-event",
    "ci-monitor-event",
];

/// Drop harness wrappers, keep `/command` names, unwrap any other tag, and
/// squeeze blank runs.
fn clean(text: &str) -> String {
    let mut s = text.to_string();
    for tag in NOISE_TAGS {
        let open = format!("<{tag}");
        let close = format!("</{tag}>");
        while let Some(start) = s.find(&open) {
            let end = s[start..].find(&close).map(|e| start + e + close.len());
            match end {
                Some(end) => s.replace_range(start..end, ""),
                None => {
                    s.truncate(start);
                    break;
                }
            }
        }
    }
    let s = strip_tag_markers(&s);
    // "The handoff plugin sent a message: <request>" - keep the request.
    let s = match s.find(" plugin sent a message:") {
        Some(i) if i < 60 && s.starts_with("The ") => {
            s[i + " plugin sent a message:".len()..].to_string()
        }
        _ => s,
    };
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        if line.starts_with("This is how Claude Code surfaces") {
            continue;
        }
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_string()
}

/// Remove `<tag ...>` / `</tag>` markers (lowercase names only, so `a < b`
/// and generics survive) while keeping what they wrap.
fn strip_tag_markers(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == b'<' {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'/' {
                j += 1;
            }
            let name_start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_lowercase() || bytes[j] == b'-' || bytes[j] == b'_')
            {
                j += 1;
            }
            if j > name_start + 1 {
                if let Some(rel) = s[j..].find('>') {
                    let inner = &s[j..j + rel];
                    if rel <= 200 && !inner.contains('\n') && !inner.contains('<') {
                        i = j + rel + 1;
                        continue;
                    }
                }
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Drop terminal colour codes, with or without their escape byte.
fn strip_ansi(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        let esc = chars[i] == '\u{1b}';
        let start = if esc && chars.get(i + 1) == Some(&'[') {
            Some(i + 2)
        } else if chars[i] == '[' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(start) = start {
            let mut j = start;
            while j < chars.len() && (chars[j].is_ascii_digit() || chars[j] == ';') {
                j += 1;
            }
            if j > start && chars.get(j) == Some(&'m') {
                i = j + 1;
                continue;
            }
        }
        if !esc {
            out.push(chars[i]);
        }
        i += 1;
    }
    out
}

/// A compact tool line (`→ name(arg) ⇒ ok · N lines`), with the structured
/// summary's output when the session was parsed from its source file.
fn parse_tool(text: &str, event: Option<&ToolEvent>) -> Option<ToolLine> {
    let body = text.strip_prefix("→ ")?;
    let split = body.rfind(" ⇒ ")?;
    let (head, tail) = (&body[..split], &body[split + " ⇒ ".len()..]);
    let (name, arg) = match head.find('(') {
        Some(p) => (
            &head[..p],
            head[p + 1..].strip_suffix(')').unwrap_or(&head[p + 1..]),
        ),
        None => (head, ""),
    };
    let status = if tail.starts_with("ok") {
        Status::Ok
    } else if tail.starts_with("error") {
        Status::Error
    } else {
        Status::Pending
    };
    let error = tail
        .split_once(" — ")
        .map(|(_, e)| e.trim().to_string())
        .unwrap_or_default();
    let (output, full) = match event {
        Some(ToolEvent::Summary(s)) => (s.output.clone(), full_command(&s.args)),
        _ => (None, None),
    };
    // The compact line cuts a long command at 120 characters; the structured
    // call, when there is one, still has all of it.
    let arg = full.unwrap_or_else(|| arg.trim().to_string());
    Some(ToolLine {
        name: name.trim().to_string(),
        arg,
        status,
        error,
        output,
    })
}

fn full_command(args: &serde_json::Value) -> Option<String> {
    let v = args.get("command").or_else(|| args.get("cmd"))?;
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

fn is_edit_tool(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    if n.contains("todo") {
        return false;
    }
    matches!(
        n.as_str(),
        "edit"
            | "write"
            | "multiedit"
            | "notebookedit"
            | "apply_patch"
            | "str_replace_editor"
            | "str_replace_based_edit_tool"
            | "create_file"
            | "write_file"
            | "edit_file"
            | "replace"
            | "patch"
            | "insert"
            | "create"
            | "write_to_file"
            | "replace_in_file"
    ) || n.ends_with("_edit")
        || n.ends_with("_write")
}

fn is_command_tool(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "bash"
            | "shell"
            | "exec_command"
            | "powershell"
            | "run_terminal_cmd"
            | "run_command"
            | "execute_command"
            | "terminal"
            | "local_shell"
            | "run_shell_command"
            | "shell_command"
    )
}

/// What a command line is "the same command" as: `cargo test`, `git commit`,
/// `npm run build`, `python x.py` - ignoring `cd ... &&`, env assignments,
/// paths to the program and flags.
fn command_key(cmd: &str) -> String {
    let segment = cmd
        .split("&&")
        .flat_map(|s| s.split(';'))
        .map(str::trim)
        .find(|s| {
            !s.is_empty()
                && !s.starts_with("cd ")
                && !s.starts_with("export ")
                && !s.starts_with("set ")
                && !s.starts_with("source ")
                && !s.split_whitespace().all(is_assignment)
        })
        .unwrap_or(cmd);
    let segment = segment.split('|').next().unwrap_or(segment);
    // Skip `VAR=value` prefixes and wrappers like `timeout 450` or `sudo`.
    const WRAPPERS: &[&str] = &[
        "timeout", "sudo", "env", "nohup", "time", "nice", "exec", "command",
    ];
    let mut tokens = segment
        .split_whitespace()
        .filter(|t| !is_assignment(t))
        .peekable();
    while let Some(t) = tokens.peek() {
        if WRAPPERS.contains(t) || t.chars().all(|c| c.is_ascii_digit()) {
            tokens.next();
        } else {
            break;
        }
    }
    let Some(first) = tokens.next() else {
        return cmd.trim().chars().take(40).collect();
    };
    let prog = first
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == ';' || c == '…')
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(first)
        .trim_end_matches(".exe")
        .to_string();
    let all: Vec<&str> = tokens
        .map(|t| t.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ';' | ',' | ')')))
        .filter(|t| !t.is_empty())
        .collect();
    if prog.starts_with("python") || prog == "py" {
        if let Some(script) = all.iter().find(|t| t.ends_with(".py")) {
            let base = script.rsplit(['/', '\\']).next().unwrap_or(script);
            return format!("{prog} {base}");
        }
        if let Some(i) = all.iter().position(|t| *t == "-m") {
            if let Some(module) = all.get(i + 1) {
                return format!("{prog} -m {module}");
            }
        }
        return if all
            .iter()
            .any(|t| t.starts_with("<<") || *t == "-" || *t == "-c")
        {
            format!("{prog} (inline)")
        } else {
            prog
        };
    }
    let rest: Vec<&str> = all
        .iter()
        .copied()
        .filter(|t| !t.starts_with('-'))
        .take(2)
        .collect();
    let multi = [
        "cargo",
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "git",
        "go",
        "docker",
        "make",
        "dotnet",
        "gradle",
        "mvn",
        "pip",
        "uv",
        "deno",
        "npx",
        "kubectl",
        "gh",
        "terraform",
        "poetry",
    ];
    if multi.contains(&prog.as_str()) {
        if let Some(sub) = rest.first() {
            if matches!(prog.as_str(), "npm" | "pnpm" | "yarn" | "bun") && *sub == "run" {
                if let Some(script) = rest.get(1) {
                    return format!("{prog} run {script}");
                }
            }
            return format!("{prog} {sub}");
        }
    }
    if prog == "node" {
        if let Some(script) = rest.first() {
            let base = script.rsplit(['/', '\\']).next().unwrap_or(script);
            return format!("{prog} {base}");
        }
    }
    prog
}

/// A command that verifies the work: a word of it (script names split at `_`
/// and `.`, but `check-ignore` kept whole) is a test/build/lint word.
/// `NAME=value` as a shell sets a variable (the value may hold a path).
fn is_assignment(token: &str) -> bool {
    token.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit())
    })
}

fn is_check_command(key: &str) -> bool {
    const WORDS: &[&str] = &[
        "test",
        "tests",
        "build",
        "check",
        "lint",
        "clippy",
        "tsc",
        "pytest",
        "jest",
        "vitest",
        "fmt",
        "compile",
        "py_compile",
        "typecheck",
        "verify",
        "eslint",
        "ruff",
        "mypy",
        "smoke",
        "e2e",
    ];
    key.split_whitespace()
        .any(|token| WORDS.contains(&token) || token.split(['_', '.']).any(|w| WORDS.contains(&w)))
}

/// Commands that only look around (read, list, search, wait): counted on one
/// line instead of listed.
fn is_lookup_command(key: &str) -> bool {
    const PROGS: &[&str] = &[
        "grep",
        "rg",
        "sed",
        "cat",
        "ls",
        "head",
        "tail",
        "echo",
        "find",
        "wc",
        "for",
        "while",
        "cd",
        "pwd",
        "dir",
        "type",
        "awk",
        "sort",
        "uniq",
        "cut",
        "tr",
        "xargs",
        "printf",
        "sleep",
        "stat",
        "file",
        "which",
        "where",
        "tree",
        "less",
        "more",
        "true",
        "false",
        "test",
        "du",
        "df",
        "nl",
        "curl",
        "Invoke-WebRequest",
        "Invoke-RestMethod",
        "Start-Sleep",
        "Set-Location",
    ];
    const GIT: &[&str] = &[
        "git show",
        "git log",
        "git status",
        "git diff",
        "git branch",
        "git rev-parse",
        "git remote",
        "git ls-files",
        "git config",
        "git fetch",
        "git blame",
        "git grep",
    ];
    const PS: &[&str] = &[
        "Get-", "Select-", "Test-", "Write-", "Out-", "Format-", "Measure-", "Where-", "ForEach-",
        "Sort-", "Join-", "Split-", "Resolve-",
    ];
    let first = key.split_whitespace().next().unwrap_or("");
    PROGS.contains(&first)
        || GIT.contains(&key)
        || PS.iter().any(|p| first.starts_with(p))
        || first.starts_with(['$', '@', '{', '(', '[', '#', '\'', '"', '<', '&', '.'])
}

const PASS_KEYS: &[&str] = &[
    "test result:",
    "passed",
    "Passed",
    "✓",
    "success",
    "Success",
    "Finished",
    "Tests:",
    "OK",
    "BUILD SUCCESS",
];
const FAIL_KEYS: &[&str] = &[
    "error[",
    "error:",
    "Error:",
    "Error",
    "Exception",
    "FAILED",
    "FAIL",
    "failed",
    "denied",
    "not found",
    "No such",
    "cannot",
    "Cannot",
    "fatal:",
];

/// The line of a command's output that says how it went: a passing line for
/// a run that succeeded, a failing one for a run that did not.
fn outcome_line(output: &str, ok: bool) -> Option<String> {
    let keys = if ok { PASS_KEYS } else { FAIL_KEYS };
    output
        .lines()
        .rev()
        .map(|l| strip_ansi(l.trim()))
        .find(|l| {
            l.len() < 300 && !l.starts_with("Traceback") && keys.iter().any(|k| l.contains(k))
        })
        .map(|l| truncate_words(&l, 120))
}

/// A refusal is the person saying no, not the work failing.
fn is_rejection(x: &ToolLine) -> bool {
    let head: String = x
        .output
        .as_deref()
        .unwrap_or("")
        .chars()
        .take(300)
        .collect();
    let text = format!("{} {head}", x.error);
    text.contains("doesn't want to proceed")
        || text.contains("was rejected")
        || text.contains("User refused")
}

/// An error's identity across repeats: first line, numbers and paths blanked.
fn error_signature(line: &ToolLine) -> String {
    let from_output = line.output.as_deref().and_then(|o| outcome_line(o, false));
    let vague = line.error.is_empty()
        || line.error.starts_with("Exit code")
        || line.error.starts_with("Traceback");
    let src = match from_output {
        Some(l) if vague => l,
        _ if !line.error.is_empty() => line.error.clone(),
        _ => line
            .output
            .as_deref()
            .and_then(|o| o.lines().find(|l| !l.trim().is_empty()))
            .unwrap_or("")
            .to_string(),
    };
    let src = strip_tag_markers(&strip_ansi(&src));
    let words: Vec<String> = src
        .split_whitespace()
        .map(|w| {
            if w.contains('/') || w.contains('\\') {
                "…".to_string()
            } else if w.chars().any(|c| c.is_ascii_digit()) && !w.chars().any(|c| c.is_alphabetic())
            {
                "N".to_string()
            } else {
                w.to_string()
            }
        })
        .collect();
    truncate_words(&words.join(" "), 110)
}

// ---------------------------------------------------------------------------
// Sentences and scoring

const STOP: &[&str] = &[
    "the",
    "and",
    "for",
    "that",
    "this",
    "with",
    "you",
    "are",
    "was",
    "but",
    "not",
    "have",
    "has",
    "can",
    "will",
    "from",
    "all",
    "its",
    "it's",
    "into",
    "then",
    "than",
    "there",
    "here",
    "now",
    "just",
    "also",
    "what",
    "when",
    "which",
    "would",
    "should",
    "could",
    "about",
    "some",
    "any",
    "one",
    "two",
    "out",
    "use",
    "get",
    "let",
    "lets",
    "see",
    "they",
    "them",
    "our",
    "your",
    "their",
    "been",
    "being",
    "were",
    "does",
    "did",
    "done",
    "only",
    "more",
    "most",
    "very",
    "how",
    "why",
    "who",
    "each",
    "both",
    "same",
    "other",
    "such",
    "these",
    "those",
    "like",
    "make",
    "made",
    "это",
    "что",
    "как",
    "для",
    "все",
    "всё",
    "так",
    "его",
    "она",
    "они",
    "оно",
    "был",
    "была",
    "было",
    "были",
    "есть",
    "уже",
    "еще",
    "ещё",
    "или",
    "если",
    "чтобы",
    "при",
    "без",
    "над",
    "под",
    "про",
    "через",
    "после",
    "перед",
    "между",
    "тоже",
    "также",
    "только",
    "можно",
    "нужно",
    "надо",
    "будет",
    "этот",
    "эта",
    "эти",
    "того",
    "тот",
    "там",
    "тут",
    "здесь",
    "сейчас",
    "теперь",
    "потом",
    "когда",
    "где",
    "чем",
    "кто",
    "ещё",
    "очень",
    "более",
    "менее",
    "вот",
    "даже",
    "ведь",
    "лишь",
    "них",
    "ним",
    "нем",
    "нём",
    "мне",
    "меня",
    "мы",
    "вы",
    "вам",
    "нас",
    "вас",
    "свой",
    "своё",
    "свои",
    "который",
    "которая",
    "которые",
    "которых",
    "один",
    "одна",
];

const DECISION: &[&str] = &[
    "решил",
    "решили",
    "выбрал",
    "вместо",
    "потому что",
    "так как",
    "причина",
    "дело в том",
    "оказалось",
    "корень",
    "поэтому",
    "decided",
    "instead",
    "because",
    "root cause",
    "turned out",
    "the cause",
    "the issue was",
    "the problem was",
    "chose",
    "trade-off",
    "tradeoff",
    "so that",
];
const RESULT: &[&str] = &[
    "исправ",
    "работает",
    "готово",
    "проход",
    "прошли",
    "добавил",
    "добавлен",
    "реализ",
    "заработал",
    "подтвержд",
    "закоммич",
    "запушен",
    "fixed",
    "works",
    "passing",
    "passes",
    "implemented",
    "added",
    "resolved",
    "confirmed",
    "committed",
    "pushed",
    "verified",
];
/// A reply that opens with one of these pushes back on what was done.
const CORRECTION_START: &[&str] = &[
    "нет",
    "не так",
    "не надо",
    "не нужно",
    "неправильно",
    "стоп",
    "подожди",
    "отмени",
    "верни",
    "no",
    "stop",
    "wait",
    "actually",
    "don't",
    "do not",
    "revert",
    "undo",
    "wrong",
];
/// Reproach anywhere near the start of a reply.
const CORRECTION_ANY: &[&str] = &[
    "не то ",
    "не так",
    "ты не ",
    "вы не ",
    "я же просил",
    "я просил",
    "зачем ты",
    "почему ты",
    "не нужно было",
    "сломал",
    "по-прежнему не",
    "всё ещё не",
    "все еще не",
    "опять не",
    "you didn't",
    "you did not",
    "not what i",
    "i asked",
    "why did you",
    "you broke",
    "still doesn't",
    "still not",
    "that's wrong",
    "that is wrong",
];

fn is_correction(request: &str) -> bool {
    let opening: String = request.chars().take(300).collect::<String>().to_lowercase();
    let first_word_ok = |cue: &str| {
        opening
            .strip_prefix(cue)
            .is_some_and(|rest| rest.chars().next().is_none_or(|c| !c.is_alphanumeric()))
    };
    // At a word start: "тексты не" does not contain "ты не".
    let at_word = |cue: &str| {
        opening.match_indices(cue).any(|(i, _)| {
            opening[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
        })
    };
    CORRECTION_START.iter().any(|c| first_word_ok(c)) || CORRECTION_ANY.iter().any(|c| at_word(c))
}
const TODO: &[&str] = &[
    "осталось",
    "следующ",
    "todo",
    "надо будет",
    "нужно будет",
    "не сделано",
    "не проверено",
    "не закоммич",
    "на потом",
    "отложи",
    "follow-up",
    "follow up",
    "next step",
    "remaining",
    "still need",
    "not yet",
    "deferred",
    "open question",
    "not verified",
    "not committed",
    "nothing committed",
    "left to do",
];
/// Replies that report a stop, not an outcome.
const NOT_A_RESULT: &[&str] = &[
    "you've hit your",
    "session limit",
    "waiting for",
    "ожидаю",
    "жду ",
    "usage limit",
    "rate limit",
];
/// Phrases that look like a TODO cue but say the opposite.
const NOT_TODO: &[&str] = &["не осталось", "nothing left", "nothing remaining"];
const BOILERPLATE: &[&str] = &[
    "давайте",
    "сейчас ",
    "теперь ",
    "отлично",
    "хорошо,",
    "понял",
    "смотрю",
    "проверяю",
    "посмотрю",
    "let me",
    "now i",
    "now let",
    "great",
    "perfect",
    "okay",
    "ok,",
    "i'll ",
    "i will ",
    "let's ",
    "first,",
    "looking at",
    "i need to",
    "i see",
    "now ",
    "next,",
    "далее ",
    "сначала ",
];

fn has_cue(lower: &str, cues: &[&str]) -> bool {
    cues.iter().any(|c| lower.contains(c))
}

/// Terms of a sentence: lowercase words of 3+ letters minus stopwords, cut to
/// six characters - a crude stem that folds "исправил"/"исправлено" and
/// "parse"/"parser" together.
fn terms(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.chars().count() >= 3)
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .map(|w| w.chars().take(6).collect())
        .collect()
}

/// Sentences of a message: code blocks dropped, markdown markers stripped,
/// lines split at sentence ends.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code || line.is_empty() {
            continue;
        }
        let line = if line.starts_with('|') {
            if line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) {
                continue;
            }
            line.trim_matches('|')
                .split('|')
                .map(str::trim)
                .filter(|c| !c.is_empty())
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            strip_markdown_prefix(line)
                .replace("**", "")
                .replace("__", "")
        };
        for s in split_sentences(&line) {
            let s = s.trim();
            if s.chars().count() >= 20 && s.chars().filter(|c| c.is_alphabetic()).count() >= 10 {
                out.push(truncate_words(s, 320));
            }
        }
    }
    out
}

fn strip_markdown_prefix(line: &str) -> String {
    let mut l = line.trim_start_matches('#').trim_start();
    for p in ["- ", "* ", "> ", "• "] {
        if let Some(rest) = l.strip_prefix(p) {
            l = rest;
        }
    }
    let digits = l.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 3 {
        let rest = &l[digits..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            l = r;
        }
    }
    l.to_string()
}

/// Split at `.`, `!`, `?`, `…` followed by a space and a capital, digit or
/// quote - so `v1.0.1`, `e.g. this` and `src/main.rs` stay whole.
fn split_sentences(line: &str) -> Vec<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?' | '…') && i + 2 < chars.len() && chars[i + 1] == ' '
        {
            let next = chars[i + 2];
            if next.is_uppercase() || next.is_ascii_digit() || matches!(next, '«' | '"' | '`' | '(')
            {
                out.push(chars[start..=i].iter().collect());
                start = i + 2;
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

#[derive(Debug, Clone)]
struct Sentence {
    text: String,
    turn: usize,
    /// Index of the message within its turn (0 = the request).
    msg: usize,
    role: Role,
    terms: HashSet<String>,
    score: f64,
}

fn looks_like_path(s: &str) -> bool {
    s.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != '/' && c != '\\' && c != '.' && c != '_'
        });
        (w.contains('/') || w.contains('\\')) && w.len() > 3
            || w.rsplit_once('.').is_some_and(|(stem, ext)| {
                !stem.is_empty()
                    && (1..=4).contains(&ext.len())
                    && ext.chars().all(|c| c.is_ascii_lowercase())
                    && stem.chars().any(|c| c.is_alphabetic())
            })
    })
}

fn score_all(turns: &[Turn]) -> Vec<Sentence> {
    let mut all = Vec::new();
    for (t, turn) in turns.iter().enumerate() {
        for s in sentences(&turn.request) {
            all.push(Sentence {
                terms: terms(&s),
                text: s,
                turn: t,
                msg: 0,
                role: Role::User,
                score: 0.0,
            });
        }
        for (m, reply) in turn.replies.iter().enumerate() {
            for s in sentences(reply) {
                all.push(Sentence {
                    terms: terms(&s),
                    text: s,
                    turn: t,
                    msg: m + 1,
                    role: Role::Assistant,
                    score: 0.0,
                });
            }
        }
    }
    let n = all.len().max(1) as f64;
    let mut df: HashMap<&str, usize> = HashMap::new();
    for s in &all {
        for w in &s.terms {
            *df.entry(w.as_str()).or_default() += 1;
        }
    }
    let idf: HashMap<String, f64> = df
        .into_iter()
        .map(|(w, d)| (w.to_string(), (1.0 + n / d as f64).ln()))
        .collect();
    let last_quarter = turns.len().saturating_sub((turns.len() / 4).max(1));
    for s in &mut all {
        if s.terms.is_empty() {
            continue;
        }
        let weight: f64 = s
            .terms
            .iter()
            .map(|w| idf.get(w).copied().unwrap_or(0.0))
            .sum();
        let mut score = weight / ((s.terms.len() as f64) + 4.0).sqrt();
        let lower = s.text.to_lowercase();
        if s.text.contains('`') || looks_like_path(&s.text) {
            score *= 1.2;
        }
        if s.text.chars().any(|c| c.is_ascii_digit()) {
            score *= 1.1;
        }
        if has_cue(&lower, DECISION) {
            score *= 1.5;
        }
        if has_cue(&lower, RESULT) {
            score *= 1.3;
        }
        if BOILERPLATE.iter().any(|b| lower.starts_with(b)) && !has_cue(&lower, RESULT) {
            score *= 0.4;
        }
        if s.turn >= last_quarter {
            score *= 1.25;
        }
        if s.role == Role::User {
            score *= 1.15;
        }
        s.score = score;
    }
    let max = all.iter().map(|s| s.score).fold(0.0, f64::max).max(1e-9);
    for s in &mut all {
        s.score /= max;
    }
    all
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len() + b.len()) as f64 * 2.0
}

/// Maximal marginal relevance: best-scoring sentences that are not about the
/// same thing as one already chosen, until `budget` characters or `max` items.
fn mmr<'a>(
    pool: &[&'a Sentence],
    taken: &[HashSet<String>],
    budget: usize,
    max: usize,
) -> Vec<&'a Sentence> {
    const LAMBDA: f64 = 0.7;
    let mut chosen: Vec<&Sentence> = Vec::new();
    let mut seen: Vec<&HashSet<String>> = taken.iter().collect();
    let mut used = 0;
    let mut left: Vec<&Sentence> = pool.to_vec();
    while chosen.len() < max && !left.is_empty() {
        let mut best: Option<(usize, f64)> = None;
        for (i, s) in left.iter().enumerate() {
            let sim = seen
                .iter()
                .map(|t| jaccard(&s.terms, t))
                .fold(0.0, f64::max);
            if sim > 0.6 {
                continue;
            }
            let v = LAMBDA * s.score - (1.0 - LAMBDA) * sim;
            if best.is_none_or(|(_, b)| v > b) {
                best = Some((i, v));
            }
        }
        let Some((i, _)) = best else { break };
        let s = left.swap_remove(i);
        if used + s.text.len() > budget {
            if chosen.is_empty() {
                continue;
            }
            break;
        }
        used += s.text.len() + 3;
        seen.push(&s.terms);
        chosen.push(s);
    }
    chosen
}

// ---------------------------------------------------------------------------
// Rendering

struct Labels {
    header: &'static str,
    stats: fn(usize, usize, usize, usize, usize) -> String,
    goal: &'static str,
    outcome: &'static str,
    timeline: &'static str,
    more_turns: fn(usize) -> String,
    key: &'static str,
    corrections: &'static str,
    open: &'static str,
    files: &'static str,
    more_files: fn(usize) -> String,
    commands: &'static str,
    last: &'static str,
    runs: fn(usize, usize) -> String,
    errors: &'static str,
    fixed: &'static str,
    unfixed: &'static str,
    tail: &'static str,
    user: &'static str,
    agent: &'static str,
    calls: &'static str,
    other_tools: &'static str,
    lookups: &'static str,
    script: &'static str,
    turns_n: fn(usize) -> String,
    temp_files: fn(usize) -> String,
}

const RU: Labels = Labels {
    header: "Прошлая сессия",
    stats: |t, m, c, e, f| {
        format!("{t} ходов · {m} сообщений · {c} вызовов инструментов · {e} ошибок · {f} файлов изменено")
    },
    goal: "Задача",
    outcome: "Итог",
    timeline: "Ход работы",
    more_turns: |n| format!("… ещё {n} ходов пропущено"),
    key: "Ключевые моменты",
    corrections: "Поправки пользователя",
    open: "Что осталось",
    files: "Изменённые файлы",
    more_files: |n| format!("… и ещё {n}"),
    commands: "Команды и проверки",
    last: "последний",
    runs: |n, e| {
        if e > 0 {
            format!("×{n}, ошибок {e}")
        } else {
            format!("×{n}")
        }
    },
    errors: "Ошибки",
    fixed: "исправлено",
    unfixed: "не исправлено",
    tail: "Последний обмен",
    user: "Пользователь",
    agent: "Агент",
    calls: "Последние вызовы инструментов",
    other_tools: "Прочие инструменты",
    lookups: "Просмотр и поиск",
    script: "скрипты",
    turns_n: |n| {
        let word = match (n % 10, n % 100) {
            (1, r) if r != 11 => "ход",
            (2..=4, r) if !(12..=14).contains(&r) => "хода",
            _ => "ходов",
        };
        format!("{n} {word}")
    },
    temp_files: |n| format!("… и {n} временных файлов вне проекта"),
};

const EN: Labels = Labels {
    header: "Previous session",
    stats: |t, m, c, e, f| {
        format!("{t} turns · {m} messages · {c} tool calls · {e} errors · {f} files changed")
    },
    goal: "Goal",
    outcome: "Outcome",
    timeline: "How it went",
    more_turns: |n| format!("… {n} more turns omitted"),
    key: "Key points",
    corrections: "User corrections",
    open: "Still open",
    files: "Files changed",
    more_files: |n| format!("… and {n} more"),
    commands: "Commands and checks",
    last: "last",
    runs: |n, e| {
        if e > 0 {
            format!("×{n}, {e} failed")
        } else {
            format!("×{n}")
        }
    },
    errors: "Errors",
    fixed: "fixed",
    unfixed: "not fixed",
    tail: "Last exchange",
    user: "User",
    agent: "Agent",
    calls: "Last tool calls",
    other_tools: "Other tools",
    lookups: "Looking around",
    script: "scripts",
    turns_n: |n| format!("{n} {}", if n == 1 { "turn" } else { "turns" }),
    temp_files: |n| format!("… and {n} scratch files outside the project"),
};

struct ErrorGroup {
    sig: String,
    target: String,
    count: usize,
    turn: usize,
    fixed: bool,
}

struct Brief<'a> {
    session: &'a Session,
    turns: Vec<Turn>,
    sentences: Vec<Sentence>,
    budget: usize,
    l: &'static Labels,
}

impl<'a> Brief<'a> {
    fn new(session: &'a Session, turns: Vec<Turn>, budget: usize) -> Self {
        // Words only: paths, identifiers and pasted code are Latin in any
        // language.
        let words: String = turns
            .iter()
            .flat_map(|t| t.request.split_whitespace())
            .filter(|w| {
                !w.contains(['/', '\\', '.', '_', '=', ':'])
                    && w.chars()
                        .all(|c| c.is_alphabetic() || ",;!?()«»\"'-".contains(c))
            })
            .collect();
        let letters = words.chars().filter(|c| c.is_alphabetic()).count().max(1);
        let cyr = words
            .chars()
            .filter(|c| matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё'))
            .count();
        // A fifth is enough: pasted logs and code in a Russian session are
        // English, the person's own words are not.
        let l = if cyr * 5 >= letters { &RU } else { &EN };
        let sentences = score_all(&turns);
        Self {
            session,
            turns,
            sentences,
            budget,
            l,
        }
    }

    fn render(&self, include_tools: bool, include_source: bool) -> String {
        let b = self.budget;
        let mut used_terms: Vec<HashSet<String>> = Vec::new();
        let mut out = self.header(include_source);

        let goal = self.goal(b * 8 / 100);
        used_terms.push(terms(&goal));
        push_section(&mut out, self.l.goal, &goal);

        let outcome = self.outcome(b * 12 / 100);
        used_terms.push(terms(&outcome));
        push_section(&mut out, self.l.outcome, &outcome);

        // What is left is picked first and never dropped for repeating the
        // outcome - it is the part the next session acts on - and the key
        // points then avoid repeating any of these.
        let open = self.open_items(b * 6 / 100);
        used_terms.extend(open.1);
        push_section(&mut out, self.l.timeline, &self.timeline(b * 22 / 100));
        push_section(
            &mut out,
            self.l.key,
            &self.key_points(&mut used_terms, b * 12 / 100),
        );
        push_section(&mut out, self.l.corrections, &self.corrections(b * 6 / 100));
        push_section(&mut out, self.l.open, &open.0);
        push_section(&mut out, self.l.files, &self.files(b * 7 / 100));
        push_section(&mut out, self.l.commands, &self.commands(b * 7 / 100));
        push_section(&mut out, self.l.errors, &self.errors(b * 6 / 100));
        if include_tools {
            push_section(&mut out, self.l.calls, &self.last_calls(b * 6 / 100));
        }
        let room = b.saturating_sub(out.len() + 40).max(b * 8 / 100);
        push_section(&mut out, self.l.tail, &self.tail(room));
        out
    }

    fn header(&self, include_source: bool) -> String {
        let s = self.session;
        let tool_calls: usize = self.turns.iter().map(|t| t.tools.len()).sum();
        let errors: usize = self
            .turns
            .iter()
            .flat_map(|t| &t.tools)
            .filter(|t| t.status == Status::Error)
            .count();
        let messages = s.messages.iter().filter(|m| !m.is_rewound).count();
        let source = if include_source {
            format!(
                "\n- Source: {}",
                crate::redact::redact(&s.path.display().to_string())
            )
        } else {
            String::new()
        };
        format!(
            "# {}: {}\n\n- Tool: {} | Project: {} | Date: {}{}\n- {}\n",
            self.l.header,
            crate::redact::redact(&s.title),
            s.tool,
            crate::redact::redact(&s.project),
            crate::util::fmt_date(s.started),
            source,
            (self.l.stats)(
                self.turns.len(),
                messages,
                tool_calls,
                errors,
                self.changed_files().len()
            ),
        )
    }

    fn goal(&self, budget: usize) -> String {
        let Some(first) = self
            .turns
            .iter()
            .find(|t| !t.slash && !t.request.is_empty())
        else {
            return String::new();
        };
        condense(&first.request, budget.max(300))
    }

    /// The last substantial reply, its strongest sentences in their order.
    fn outcome(&self, budget: usize) -> String {
        let budget = budget.max(400);
        let Some((t, m)) = self.turns.iter().enumerate().rev().find_map(|(t, turn)| {
            turn.replies
                .iter()
                .enumerate()
                .rev()
                .find(|(_, r)| r.chars().count() >= 60)
                .map(|(m, _)| (t, m + 1))
        }) else {
            return String::new();
        };
        let reply = &self.turns[t].replies[m - 1];
        if reply.len() <= budget {
            return demote_headings(&strip_code(reply));
        }
        let pool: Vec<&Sentence> = self
            .sentences
            .iter()
            .filter(|s| s.turn == t && s.msg == m)
            .collect();
        let mut picked = mmr(&pool, &[], budget, 12);
        if let Some(first) = pool.first() {
            if !picked.iter().any(|p| std::ptr::eq(*p, *first)) {
                picked.insert(0, first);
            }
        }
        picked.sort_by_key(|s| pool.iter().position(|p| std::ptr::eq(*p, *s)));
        picked
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// One line per turn: request → result, with what the turn did. Long
    /// sessions keep the first, the last ones and the most distinct between.
    fn timeline(&self, budget: usize) -> String {
        let n = self.turns.len();
        let lines: Vec<String> = (0..n).map(|t| self.turn_line(t)).collect();
        let total: usize = lines.iter().map(|l| l.len() + 1).sum();
        let keep: Vec<usize> = if total <= budget {
            (0..n).collect()
        } else {
            // Anchors: the first real request and the last two; a bare
            // `/compact` never stands for a turn.
            let real: Vec<usize> = (0..n).filter(|&t| !self.turns[t].slash).collect();
            let mut keep: Vec<usize> = real.first().copied().into_iter().collect();
            for &t in real.iter().rev().take(2) {
                if !keep.contains(&t) {
                    keep.push(t);
                }
            }
            let mut used: usize = keep.iter().map(|&t| lines[t].len() + 1).sum();
            let mut seen: Vec<HashSet<String>> = keep
                .iter()
                .map(|&t| terms(&self.turns[t].request))
                .collect();
            let mut cand: Vec<(usize, f64, HashSet<String>)> = real
                .iter()
                .copied()
                .filter(|t| !keep.contains(t))
                .map(|t| (t, self.turn_weight(t), terms(&self.turns[t].request)))
                .collect();
            loop {
                let best = cand
                    .iter()
                    .enumerate()
                    .map(|(i, (_, w, ts))| {
                        let sim = seen.iter().map(|s| jaccard(ts, s)).fold(0.0, f64::max);
                        (i, 0.7 * w - 0.3 * sim)
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1));
                let Some((i, _)) = best else { break };
                let (t, _, ts) = cand.swap_remove(i);
                if used + lines[t].len() + 1 > budget {
                    continue;
                }
                used += lines[t].len() + 1;
                seen.push(ts);
                keep.push(t);
            }
            keep.sort_unstable();
            keep
        };
        let mut out = String::new();
        let mut prev: Option<usize> = None;
        for &t in &keep {
            if let Some(p) = prev {
                if t > p + 1 {
                    out.push_str(&format!("   {}\n", (self.l.more_turns)(t - p - 1)));
                }
            }
            out.push_str(&lines[t]);
            out.push('\n');
            prev = Some(t);
        }
        out
    }

    fn turn_weight(&self, t: usize) -> f64 {
        let turn = &self.turns[t];
        let best = self
            .sentences
            .iter()
            .filter(|s| s.turn == t)
            .map(|s| s.score)
            .fold(0.0, f64::max);
        let edits = turn
            .tools
            .iter()
            .filter(|x| is_edit_tool(&x.name))
            .count()
            .min(10) as f64;
        let errors = turn
            .tools
            .iter()
            .filter(|x| x.status == Status::Error)
            .count()
            .min(5) as f64;
        let correction = if is_correction(&turn.request) {
            0.3
        } else {
            0.0
        };
        best + 0.04 * edits + 0.05 * errors + correction
    }

    fn turn_line(&self, t: usize) -> String {
        let turn = &self.turns[t];
        let request = if turn.request.is_empty() {
            "—".to_string()
        } else {
            first_sentence(&turn.request, 110)
        };
        let result = self
            .sentences
            .iter()
            .filter(|s| s.turn == t && s.role == Role::Assistant)
            .max_by(|a, b| {
                let bonus = |s: &Sentence| {
                    s.score + 0.15 * (s.msg as f64 / turn.replies.len().max(1) as f64)
                };
                bonus(a).total_cmp(&bonus(b))
            })
            .map(|s| truncate_words(&s.text, 140));
        let edited: HashSet<&str> = turn
            .tools
            .iter()
            .filter(|x| is_edit_tool(&x.name))
            .map(|x| x.arg.as_str())
            .collect();
        let commands = turn
            .tools
            .iter()
            .filter(|x| is_command_tool(&x.name))
            .count();
        let errors = turn
            .tools
            .iter()
            .filter(|x| x.status == Status::Error)
            .count();
        let mut tags = Vec::new();
        if !edited.is_empty() {
            tags.push(format!("✎{}", edited.len()));
        }
        if commands > 0 {
            tags.push(format!("▶{commands}"));
        }
        if errors > 0 {
            tags.push(format!("⚠{errors}"));
        }
        let mut line = format!("{}. {}", t + 1, request);
        if let Some(r) = result {
            line.push_str(" → ");
            line.push_str(&r);
        }
        if !tags.is_empty() {
            line.push_str(&format!(" · {}", tags.join(" ")));
        }
        line
    }

    fn key_points(&self, used: &mut Vec<HashSet<String>>, budget: usize) -> String {
        let pool: Vec<&Sentence> = self
            .sentences
            .iter()
            .filter(|s| s.role == Role::Assistant)
            .collect();
        let picked = mmr(&pool, used, budget, 10);
        used.extend(picked.iter().map(|s| s.terms.clone()));
        let mut picked = picked;
        picked.sort_by_key(|s| (s.turn, s.msg));
        bullets(picked.iter().map(|s| s.text.clone()))
    }

    fn corrections(&self, budget: usize) -> String {
        let mut out = Vec::new();
        let mut used = 0;
        let mut seen: Vec<HashSet<String>> = Vec::new();
        for (t, turn) in self.turns.iter().enumerate().skip(1) {
            // A correction is said in so many words, near the start of a
            // reply; a pasted report that mentions "instead" deep inside is not.
            if turn.slash || turn.request.chars().count() > 1500 {
                continue;
            }
            if !is_correction(&turn.request) {
                continue;
            }
            let flat = one_line(&strip_code(&turn.request));
            let said = split_sentences(&flat)
                .into_iter()
                .find(|s| is_correction(s))
                .unwrap_or(flat);
            let text = format!("{} ({})", truncate_words(&said, 180), t + 1);
            let ts = terms(&text);
            if seen.iter().any(|s| jaccard(s, &ts) > 0.6) || used + text.len() > budget {
                continue;
            }
            used += text.len();
            seen.push(ts);
            out.push(text);
        }
        bullets(out.into_iter())
    }

    fn open_items(&self, budget: usize) -> (String, Vec<HashSet<String>>) {
        let from = self.turns.len() * 2 / 3;
        let pool: Vec<&Sentence> = self
            .sentences
            .iter()
            .filter(|s| {
                let lower = s.text.to_lowercase();
                s.role == Role::Assistant
                    && s.turn >= from
                    && has_cue(&lower, TODO)
                    && !has_cue(&lower, NOT_TODO)
            })
            .collect();
        let mut picked = mmr(&pool, &[], budget, 5);
        picked.sort_by_key(|s| (s.turn, s.msg));
        let terms = picked.iter().map(|s| s.terms.clone()).collect();
        (bullets(picked.iter().map(|s| s.text.clone())), terms)
    }

    /// Changed files as shown (relative to the project when inside it), how
    /// often each was edited, and whether it is a scratch file; the same file
    /// written as absolute and relative path counts once.
    fn changed_files(&self) -> Vec<(String, usize, bool)> {
        // A stored edit or touched row is structured: the plausibility filter
        // below exists for transcript text, and running the rows through it is
        // what left a brief saying "0 files changed" beside turns that showed
        // eleven edits - every path with a parenthesis or a quote in it, which
        // is what antigravity records for a project folder named like that.
        let rows: Vec<String> = if !self.session.edits.is_empty() {
            self.session.edits.iter().map(|e| e.path.clone()).collect()
        } else {
            self.session.touched.clone()
        };
        let from_rows = rows.iter().any(|p| !p.trim().is_empty());
        let raw: Vec<String> = if from_rows {
            rows
        } else {
            self.turns
                .iter()
                .flat_map(|t| &t.tools)
                .filter(|x| is_edit_tool(&x.name))
                .map(|x| x.arg.clone())
                .collect()
        };
        let project = normalize_path(&self.session.project);
        let prefix = format!("{}/", project.trim_end_matches('/').to_lowercase());
        let mut counts: Vec<(String, usize, bool)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        let usable = |p: &&String| {
            if from_rows {
                is_recorded_path(p)
            } else {
                is_plausible_path(p)
            }
        };
        for path in raw.iter().filter(usable) {
            let norm = normalize_path(path);
            let shown = if !project.is_empty() && norm.to_lowercase().starts_with(&prefix) {
                norm[prefix.len()..].to_string()
            } else {
                norm
            };
            let key = shown.to_lowercase();
            match index.get(&key) {
                Some(&i) => counts[i].1 += 1,
                None => {
                    index.insert(key.clone(), counts.len());
                    counts.push((shown, 1, is_scratch_path(&key)));
                }
            }
        }
        counts.sort_by(|a, b| {
            a.2.cmp(&b.2)
                .then(b.1.cmp(&a.1))
                .then_with(|| a.0.cmp(&b.0))
        });
        counts
    }

    fn files(&self, budget: usize) -> String {
        let files = self.changed_files();
        let scratch = files.iter().filter(|f| f.2).count();
        let mut lines = Vec::new();
        let mut used = 0;
        let real: Vec<&(String, usize, bool)> = files.iter().filter(|f| !f.2).collect();
        for (i, (shown, n, _)) in real.iter().enumerate() {
            let line = if *n > 1 {
                format!("`{shown}` ×{n}")
            } else {
                format!("`{shown}`")
            };
            if used + line.len() > budget && i > 0 {
                lines.push((self.l.more_files)(real.len() - i));
                break;
            }
            used += line.len() + 3;
            lines.push(line);
        }
        if scratch > 0 {
            lines.push((self.l.temp_files)(scratch));
        }
        bullets(lines.into_iter())
    }

    /// Commands collapsed by what they run, checks first, with the last
    /// outcome; other tools as one line of counts.
    fn commands(&self, budget: usize) -> String {
        struct Group {
            key: String,
            runs: usize,
            failed: usize,
            last: Status,
            outcome: Option<String>,
            order: usize,
        }
        let mut groups: Vec<Group> = Vec::new();
        let mut others: HashMap<String, usize> = HashMap::new();
        let mut lookups: HashMap<String, usize> = HashMap::new();
        for (i, x) in self.turns.iter().flat_map(|t| &t.tools).enumerate() {
            if !is_command_tool(&x.name) {
                if !is_edit_tool(&x.name) {
                    *others.entry(x.name.clone()).or_default() += 1;
                }
                continue;
            }
            let key = command_key(&x.arg);
            if is_lookup_command(&key) {
                let first = key.split_whitespace().next().unwrap_or("").to_string();
                let label = if first
                    .starts_with(['$', '@', '{', '(', '[', '#', '\'', '"', '<', '&', '.'])
                {
                    self.l.script.to_string()
                } else if first == "git" {
                    key
                } else {
                    first
                };
                *lookups.entry(label).or_default() += 1;
                continue;
            }
            let g = match groups.iter_mut().position(|g| g.key == key) {
                Some(p) => &mut groups[p],
                None => {
                    groups.push(Group {
                        key,
                        runs: 0,
                        failed: 0,
                        last: Status::Pending,
                        outcome: None,
                        order: i,
                    });
                    groups.last_mut().unwrap()
                }
            };
            g.runs += 1;
            if x.status == Status::Error {
                g.failed += 1;
            }
            g.last = x.status;
            let outcome = x
                .output
                .as_deref()
                .and_then(|o| outcome_line(o, x.status == Status::Ok))
                .or_else(|| {
                    (x.status == Status::Error && !x.error.is_empty())
                        .then(|| truncate_words(&strip_ansi(&x.error), 120))
                });
            if outcome.is_some() || x.status == Status::Ok {
                g.outcome = outcome;
            }
            g.order = i;
        }
        groups.sort_by(|a, b| {
            is_check_command(&b.key)
                .cmp(&is_check_command(&a.key))
                .then(b.runs.cmp(&a.runs))
                .then(b.order.cmp(&a.order))
        });
        let mut lines = Vec::new();
        let mut used = 0;
        for g in groups.iter().take(12) {
            let status = match g.last {
                Status::Ok => "ok",
                Status::Error => "error",
                Status::Pending => "?",
            };
            let mut line = format!(
                "`{}` {} — {}: {}",
                g.key,
                (self.l.runs)(g.runs, g.failed),
                self.l.last,
                status
            );
            if let Some(o) = &g.outcome {
                line.push_str(&format!(" ({o})"));
            }
            if used + line.len() > budget && !lines.is_empty() {
                break;
            }
            used += line.len() + 3;
            lines.push(line);
        }
        let counted = |map: HashMap<String, usize>| {
            let mut o: Vec<(String, usize)> = map.into_iter().collect();
            o.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            o.iter()
                .take(8)
                .map(|(n, c)| format!("{n} ×{c}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if !lookups.is_empty() {
            lines.push(format!("{}: {}", self.l.lookups, counted(lookups)));
        }
        if !others.is_empty() {
            lines.push(format!("{}: {}", self.l.other_tools, counted(others)));
        }
        bullets(lines.into_iter())
    }

    /// Errors grouped by signature; fixed when the same command, or any later
    /// call of the same non-shell tool, succeeded after the last failure.
    /// Refusals by the person are not errors of the work.
    fn error_groups(&self) -> Vec<ErrorGroup> {
        let flat: Vec<(usize, &ToolLine)> = self
            .turns
            .iter()
            .enumerate()
            .flat_map(|(t, turn)| turn.tools.iter().map(move |x| (t, x)))
            .collect();
        let target = |x: &ToolLine| {
            if is_command_tool(&x.name) {
                command_key(&x.arg)
            } else {
                x.name.clone()
            }
        };
        let mut groups: Vec<(ErrorGroup, usize)> = Vec::new();
        for (i, (t, x)) in flat.iter().enumerate() {
            if x.status != Status::Error || is_rejection(x) {
                continue;
            }
            let sig = error_signature(x);
            let tgt = target(x);
            match groups
                .iter_mut()
                .find(|(g, _)| g.sig == sig && g.target == tgt)
            {
                Some((g, last)) => {
                    g.count += 1;
                    *last = i;
                }
                None => groups.push((
                    ErrorGroup {
                        sig,
                        target: tgt,
                        count: 1,
                        turn: *t,
                        fixed: false,
                    },
                    i,
                )),
            }
        }
        groups
            .into_iter()
            .map(|(mut g, last)| {
                g.fixed = flat[last + 1..]
                    .iter()
                    .any(|(_, x)| x.status == Status::Ok && target(x) == g.target);
                g
            })
            .collect()
    }

    fn errors(&self, budget: usize) -> String {
        let mut errs = self.error_groups();
        // Unfixed ones first: they are what the next session inherits.
        errs.sort_by_key(|e| (e.fixed, e.turn));
        let mut lines = Vec::new();
        let mut used = 0;
        for e in &errs {
            let mark = if e.fixed {
                format!("✓ {}", self.l.fixed)
            } else {
                format!("✗ {}", self.l.unfixed)
            };
            let sig = if e.sig.is_empty() {
                "error".to_string()
            } else {
                e.sig.clone()
            };
            let times = if e.count > 1 {
                format!(" ×{}", e.count)
            } else {
                String::new()
            };
            let line = format!(
                "{mark}: {sig}{times} — `{}` ({})",
                truncate_words(&e.target, 60),
                e.turn + 1
            );
            if e.fixed && lines.len() >= 6 {
                break;
            }
            if used + line.len() > budget && !lines.is_empty() {
                break;
            }
            used += line.len() + 3;
            lines.push(line);
        }
        bullets(lines.into_iter())
    }

    /// The list line: request → result · counts.
    fn headline(&self) -> Option<String> {
        let first = self
            .turns
            .iter()
            .find(|t| !t.slash && !t.request.is_empty())?;
        let mut line = first_sentence(&first.request, 90);
        // The last turns that said something; a reply cut off mid-work
        // ("Now let me read…", "Waiting for…", a usage-limit notice) does not.
        let says_something = |s: &Sentence| {
            let lower = s.text.to_lowercase();
            !s.text.trim_end().ends_with(':')
                && !BOILERPLATE.iter().any(|b| lower.starts_with(b))
                && !NOT_A_RESULT.iter().any(|b| lower.contains(b))
        };
        let result = self
            .turns
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, t)| !t.replies.is_empty())
            .take(3)
            .find_map(|(t, turn)| {
                let replies = turn.replies.len().max(1) as f64;
                let weight = |s: &Sentence| {
                    let cue = if has_cue(&s.text.to_lowercase(), RESULT) {
                        0.3
                    } else {
                        0.0
                    };
                    s.score + cue + 0.1 * s.msg as f64 / replies
                };
                self.sentences
                    .iter()
                    .filter(|s| s.turn == t && s.role == Role::Assistant && says_something(s))
                    .max_by(|a, b| weight(a).total_cmp(&weight(b)))
            })
            .map(|s| truncate_words(&s.text, 110));
        if let Some(r) = result {
            line.push_str(" → ");
            line.push_str(&r);
        }
        let real_turns = self.turns.iter().filter(|t| !t.slash).count();
        let files = self.changed_files().iter().filter(|f| !f.2).count();
        let unfixed = self.error_groups().iter().filter(|e| !e.fixed).count();
        let mut counts = vec![(self.l.turns_n)(real_turns)];
        if files > 0 {
            counts.push(format!("✎{files}"));
        }
        if unfixed > 0 {
            counts.push(format!("⚠{unfixed}"));
        }
        line.push_str(" · ");
        line.push_str(&counts.join(" · "));
        Some(one_line(&line))
    }

    fn last_calls(&self, budget: usize) -> String {
        let all: Vec<&ToolLine> = self.turns.iter().flat_map(|t| &t.tools).collect();
        let mut lines: Vec<String> = Vec::new();
        let mut used = 0;
        for x in all.iter().rev().take(12) {
            let status = match x.status {
                Status::Ok => "ok",
                Status::Error => "error",
                Status::Pending => "pending",
            };
            let line = format!("`{}({})` ⇒ {status}", x.name, truncate_words(&x.arg, 80));
            if used + line.len() > budget {
                break;
            }
            used += line.len() + 3;
            lines.push(line);
        }
        lines.reverse();
        bullets(lines.into_iter())
    }

    /// The last real request and the last reply of the session, verbatim
    /// within the room left. A trailing `/compact` is skipped for both.
    fn tail(&self, budget: usize) -> String {
        let Some(last) = self
            .turns
            .iter()
            .rev()
            .find(|t| !t.slash && !t.request.is_empty())
        else {
            return String::new();
        };
        let reply = self
            .turns
            .iter()
            .rev()
            .flat_map(|t| t.replies.iter().rev())
            .find(|r| r.chars().count() >= 20);
        let req_budget = (budget / 3).max(300);
        let mut out = String::new();
        if !last.request.is_empty() {
            out.push_str(&format!(
                "**{}:**\n{}\n\n",
                self.l.user,
                demote_headings(&truncate_words(&last.request, req_budget))
            ));
        }
        if let Some(r) = reply {
            let room = budget.saturating_sub(out.len()).max(400);
            out.push_str(&format!(
                "**{}:**\n{}",
                self.l.agent,
                demote_headings(&truncate_words(r, room))
            ));
        }
        out
    }
}

/// Forward slashes, no doubled separators, no surrounding quotes, and the
/// `\\` that some stores keep escaped folded back.
fn normalize_path(p: &str) -> String {
    let p = p
        .trim()
        .trim_matches('"')
        .replace("\\\\", "\\")
        .replace('\\', "/");
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        if c == '/' && out.ends_with('/') && out.len() > 1 {
            continue;
        }
        out.push(c);
    }
    out
}

/// Some stores record a whole inline script where a path belongs.
/// A path taken from a stored row: only its shape is checked. A real file name
/// may hold parentheses or quotes, so the characters below are not evidence it
/// is transcript noise.
fn is_recorded_path(p: &str) -> bool {
    let p = p.trim();
    !p.is_empty() && p.len() <= 260 && !p.contains('\n')
}

fn is_plausible_path(p: &str) -> bool {
    let p = p.trim();
    is_recorded_path(p)
        && !p.contains(['"', '\'', '(', ')', '=', '{', '}', ';', '<', '>'])
        && !p.starts_with("import ")
}

fn is_scratch_path(lower: &str) -> bool {
    [
        "/tmp/",
        "/temp/",
        "scratchpad",
        "/.claude/",
        "/appdata/",
        "/.cache/",
        "/.gemini/",
        "/.codex/",
    ]
    .iter()
    .any(|m| lower.contains(m))
}

/// `## Heading` lines of quoted text as bold lines, so they do not read as
/// sections of the brief.
fn demote_headings(text: &str) -> String {
    let mut in_code = false;
    text.lines()
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
            }
            let t = line.trim_start();
            if !in_code && t.starts_with('#') {
                let title = t.trim_start_matches('#').trim();
                if title.is_empty() {
                    String::new()
                } else {
                    format!("**{title}**")
                }
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn push_section(out: &mut String, title: &str, body: &str) {
    let body = body.trim();
    if body.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {title}\n\n{body}\n"));
}

fn bullets(items: impl Iterator<Item = String>) -> String {
    items.map(|s| format!("- {}\n", one_line(&s))).collect()
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_code(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if !in_code {
                out.push("[…code…]");
            }
            in_code = !in_code;
            continue;
        }
        if !in_code {
            out.push(line);
        }
    }
    out.join("\n").trim().to_string()
}

/// A request in brief: its first sentences up to `max` characters, code out.
fn condense(text: &str, max: usize) -> String {
    let text = strip_code(text);
    if text.chars().count() <= max {
        return text;
    }
    truncate_words(&text, max)
}

fn first_sentence(text: &str, max: usize) -> String {
    let flat = one_line(&strip_code(text));
    let first = split_sentences(&flat)
        .into_iter()
        .next()
        .unwrap_or_default();
    let first = if first.chars().count() < 25 {
        flat
    } else {
        first
    };
    truncate_words(&first, max)
}

/// At most `max` characters, cut at a word boundary, with an ellipsis.
fn truncate_words(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let at = cut
        .rfind(char::is_whitespace)
        .filter(|&i| i > max / 2)
        .unwrap_or(cut.len());
    format!("{}…", cut[..at].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Message, ToolSummary};

    fn msg(role: Role, text: &str) -> Message {
        Message {
            role,
            text: text.into(),
            ts: None,
            tool: None,
            is_rewound: false,
        }
    }

    fn tool(name: &str, arg_key: &str, arg: &str, ok: bool, output: &str) -> Message {
        let summary = ToolSummary {
            name: name.into(),
            args: serde_json::json!({ arg_key: arg }),
            output: Some(output.into()),
            is_error: Some(!ok),
            lines: Some(output.lines().count()),
        };
        let text = crate::tool_summary::render_summary(&summary);
        Message {
            role: Role::Tool,
            text,
            ts: None,
            tool: Some(ToolEvent::Summary(summary)),
            is_rewound: false,
        }
    }

    fn session(messages: Vec<Message>) -> Session {
        Session {
            id: "s".into(),
            tool: "claude-code",
            path: "/p/s.jsonl".into(),
            project: "/proj".into(),
            started: None,
            ended: None,
            title: "parser work".into(),
            subagent: false,
            parent_id: None,
            messages,
            touched: vec![],
            edits: vec![],
        }
    }

    fn long_session() -> Session {
        let mut m = vec![
            msg(Role::User, "<system-reminder>ignore me</system-reminder>Fix the CSV parser: quoted fields with commas are split in two."),
            msg(Role::Assistant, "Let me look at the parser."),
            tool("Read", "file_path", "/proj/src/csv.rs", true, "fn parse() {}"),
            msg(Role::Assistant, "The root cause is that `split(',')` ignores quotes, so I decided to write a small state machine instead."),
            tool("Edit", "file_path", "/proj/src/csv.rs", true, "ok"),
            tool("Bash", "command", "cd /proj && cargo test", false, "error[E0308]: mismatched types at src/csv.rs:41\nerror: could not compile"),
            tool("Edit", "file_path", "/proj/src/csv.rs", true, "ok"),
            tool("Bash", "command", "cargo test", true, "running 12 tests\ntest result: ok. 12 passed; 0 failed"),
            msg(Role::Assistant, "Fixed: quoted fields now parse correctly and all 12 tests pass."),
            msg(Role::User, "No, don't change the public API, keep parse() returning Vec<String>."),
            msg(Role::Assistant, "Reverted the signature change; parse() returns Vec<String> again and the state machine stays internal."),
            tool("Edit", "file_path", "/proj/src/lib.rs", true, "ok"),
            msg(Role::User, "Add a test for escaped quotes."),
            msg(Role::Assistant, "Added `escaped_quotes_are_unescaped` in tests/csv.rs."),
            tool("Bash", "command", "cargo clippy", false, "warning: unused variable\nerror: aborting due to previous error"),
            msg(Role::Assistant, "Clippy still fails on an unused variable in src/bench.rs; that is left to do, nothing committed yet."),
        ];
        for i in 0..30 {
            m.push(msg(
                Role::Assistant,
                &format!("Filler reasoning step {i} about nothing in particular here."),
            ));
        }
        m.push(msg(Role::User, "Ok, summarize where we are."));
        m.push(msg(Role::Assistant, "The parser handles quotes and escapes; tests pass; clippy still fails in src/bench.rs and nothing is committed."));
        session(m)
    }

    #[test]
    fn a_long_session_gets_sections_built_from_its_turns() {
        let out = smart_brief(&long_session(), 6000, false, false);
        assert!(out.starts_with("# Previous session: parser work"), "{out}");
        assert!(
            out.contains("## Goal") && out.contains("Fix the CSV parser"),
            "{out}"
        );
        assert!(
            !out.contains("ignore me"),
            "harness wrappers are dropped: {out}"
        );
        assert!(
            out.contains("## How it went") && out.contains("1. Fix the CSV parser"),
            "{out}"
        );
        assert!(
            out.contains("root cause"),
            "decisions are key points: {out}"
        );
        assert!(
            out.contains("## User corrections") && out.contains("don't change the public API"),
            "{out}"
        );
        assert!(
            out.contains("`src/csv.rs` ×2"),
            "files counted, project prefix dropped: {out}"
        );
        assert!(
            out.contains(
                "`cargo test` ×2, 1 failed — last: ok (test result: ok. 12 passed; 0 failed)"
            ),
            "{out}"
        );
        assert!(
            out.contains("✓ fixed: error[E0308]: mismatched types at …"),
            "{out}"
        );
        assert!(
            out.contains("✗ not fixed"),
            "clippy's error stays open: {out}"
        );
        assert!(
            out.contains("## Still open") && out.contains("nothing committed"),
            "{out}"
        );
        assert!(
            out.contains("## Last exchange") && out.contains("summarize where we are"),
            "{out}"
        );
        assert!(
            !out.contains("Filler reasoning step 7"),
            "low-information filler is not quoted: {out}"
        );
        assert!(out.len() <= 6500, "within budget: {}", out.len());
    }

    #[test]
    fn a_recorded_path_survives_the_transcript_filter() {
        use crate::model::{EditEvent, EditKind};
        // What this covers: a brief whose header said "0 files changed" beside
        // turns that showed the edits, because every stored path came from a
        // folder named `game (demo)` and the transcript filter dropped it for
        // the parenthesis.
        let mut s = session(vec![
            msg(Role::User, "Move the save files to the new format."),
            msg(
                Role::Assistant,
                "Rewrote the loader and the writer; both builds pass.",
            ),
            tool(
                "edit_file",
                "file_path",
                "D:/game (demo)/src/save.cs",
                true,
                "ok",
            ),
            tool(
                "write_file",
                "file_path",
                "D:/game (demo)/src/loader.cs",
                true,
                "ok",
            ),
        ]);
        s.tool = "antigravity";
        s.project = "D:/game (demo)".into();
        for path in ["D:/game (demo)/src/save.cs", "D:/game (demo)/src/loader.cs"] {
            s.edits.push(EditEvent {
                path: path.into(),
                kind: EditKind::Edit,
                snippet: "old -> new".into(),
                ts: None,
            });
        }

        let brief = Brief::new(&s, segment(&s), 4000);
        let mut names: Vec<String> = brief
            .changed_files()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["src/loader.cs".to_string(), "src/save.cs".to_string()],
            "the recorded paths are the changed files, project prefix dropped"
        );
        assert!(
            brief.header(false).contains("2 files changed"),
            "the header counts what the turns show: {}",
            brief.header(false)
        );
        assert!(
            short_summary(&s).unwrap().contains("✎2"),
            "and so does the list line"
        );
    }

    #[test]
    fn the_list_line_is_request_result_and_counts() {
        let line = short_summary(&long_session()).unwrap();
        assert!(line.starts_with("Fix the CSV parser"), "{line}");
        assert!(line.contains(" → "), "{line}");
        assert!(
            line.ends_with("4 turns · ✎2 · ⚠1"),
            "two files, clippy still failing: {line}"
        );
        assert!(!line.contains('\n'));
        let ru = session(vec![
            msg(Role::User, "Почини сборку"),
            msg(Role::Assistant, "Исправил импорт, сборка проходит."),
        ]);
        assert_eq!(
            short_summary(&ru).unwrap(),
            "Почини сборку → Исправил импорт, сборка проходит. · 1 ход"
        );
        assert_eq!(
            short_summary(&session(vec![msg(
                Role::Assistant,
                "hello there, nothing asked"
            )])),
            None
        );
        let cut = session(vec![
            msg(Role::User, "The handoff plugin sent a message: Прочитай C:/tmp/handoff.md и продолжай работу над D:/game/src/main.rs"),
            msg(Role::Assistant, "Перенёс сохранения в новый формат, тесты проходят."),
            msg(Role::User, "дальше"),
            msg(Role::Assistant, "Now let me read the remaining files:"),
            msg(Role::Assistant, "You've hit your session limit · resets 10:40pm"),
        ]);
        let line = short_summary(&cut).unwrap();
        assert!(
            line.starts_with("Прочитай C:/tmp/handoff.md"),
            "relay prefix dropped: {line}"
        );
        assert!(
            line.contains("→ Перенёс сохранения в новый формат"),
            "a cut-off reply is not the outcome: {line}"
        );
        assert!(
            line.ends_with("· 2 хода"),
            "Russian count despite Latin paths: {line}"
        );
    }

    #[test]
    fn a_russian_session_gets_russian_headings() {
        let s = session(vec![
            msg(
                Role::User,
                "Исправь парсер CSV, поля в кавычках разбиваются.",
            ),
            msg(
                Role::Assistant,
                "Причина в том, что split не учитывает кавычки, поэтому решил написать автомат.",
            ),
            msg(Role::User, "Добавь тест на экранированные кавычки."),
            msg(Role::Assistant, "Добавил тест, все тесты проходят."),
            msg(Role::User, "Что осталось?"),
            msg(
                Role::Assistant,
                "Осталось поправить предупреждение clippy в bench.rs, изменения не закоммичены.",
            ),
        ]);
        let out = smart_brief(&s, 4000, false, false);
        assert!(
            out.contains("# Прошлая сессия")
                && out.contains("## Задача")
                && out.contains("## Ход работы"),
            "{out}"
        );
        assert!(
            out.contains("## Что осталось") && out.contains("clippy"),
            "{out}"
        );
    }

    #[test]
    fn a_tiny_session_is_its_transcript() {
        let s = session(vec![
            msg(Role::User, "hi there"),
            msg(Role::Assistant, "hello, what do you need?"),
        ]);
        let out = smart_brief(&s, 4000, false, false);
        assert!(out.contains("**User:**\nhi there"), "{out}");
    }

    #[test]
    fn credentials_never_reach_the_brief() {
        // split so GitHub secret-scanning doesn't flag this fake test token
        let secret = concat!("s", "k-abcdefghijklmnopqrstuvwxyz0123456789ABCD");
        let mut s = long_session();
        s.messages[0].text = format!("Fix the parser, the key is {secret} for the API.");
        s.messages.push(msg(
            Role::Assistant,
            &format!("{} {secret}", "x".repeat(5000)),
        ));
        let out = smart_brief(&s, 3000, true, true);
        assert!(!out.contains(secret), "{out}");
    }

    #[test]
    fn commands_collapse_to_what_they_run() {
        assert_eq!(
            command_key("cd /d/x && cargo test --release 2>&1 | tail"),
            "cargo test"
        );
        assert_eq!(
            command_key("S=/tmp; RUST_LOG=1 npm run build"),
            "npm run build"
        );
        assert_eq!(
            command_key("python -I scripts/probe.py a b"),
            "python probe.py"
        );
        assert_eq!(command_key("D:/tools/x.exe sync --tool codex"), "x");
        assert_eq!(command_key("python -X utf8 - <<'EOF'"), "python (inline)");
        assert_eq!(command_key("python -m py_compile a.py"), "python a.py");
        assert_eq!(command_key("python -m pytest -q"), "python -m pytest");
        assert_eq!(
            command_key("node scripts/v17_check.mjs';"),
            "node v17_check.mjs"
        );
        assert_eq!(
            command_key("S=/d/tmp/scratchpad; SESSIONWIKI_DATA=$S/dataC timeout 450 $S/sw.exe web"),
            "sw"
        );
        assert_eq!(command_key("RUST_LOG=debug cargo run"), "cargo run");
        assert!(is_check_command("python -m pytest") && is_check_command("node v17_check.mjs"));
        assert!(!is_check_command("git check-ignore") && !is_check_command("git sparse-checkout"));
        assert!(
            is_lookup_command("grep")
                && is_lookup_command("Get-Content")
                && is_lookup_command("git status")
        );
    }

    #[test]
    fn paths_fold_and_scripts_are_not_files() {
        assert_eq!(normalize_path("\"d:\\\\crypto\""), "d:/crypto");
        assert_eq!(
            normalize_path("D://Docker//compose.yaml"),
            "D:/Docker/compose.yaml"
        );
        assert!(is_plausible_path("D:\\tools\\notes.md"));
        assert!(!is_plausible_path("import re, sys\nROOT = r'd:/x'"));
    }

    #[test]
    fn corrections_are_pushback_not_any_mention() {
        assert!(is_correction("No, don't change the public API."));
        assert!(is_correction("нет, верни как было"));
        assert!(is_correction("ты не ответил на мой вопрос: что с тестами?"));
        assert!(!is_correction("да запускай, больше никто пока не работает"));
        assert!(!is_correction(
            "Плюс станет 111 уроков с углублением вместо 108."
        ));
        assert!(!is_correction("nothing else to add, notes look fine"));
        assert!(!is_correction("Тексты не изменятся, только их разместят."));
    }

    #[test]
    fn quoted_headings_are_demoted_outside_code() {
        let out = demote_headings("## Plan\ntext\n```\n# comment\n```");
        assert_eq!(out, "**Plan**\ntext\n```\n# comment\n```");
    }

    #[test]
    fn colour_codes_are_dropped() {
        assert_eq!(
            strip_ansi("\u{1b}[31;1mParserError: \u{1b}[0mbad"),
            "ParserError: bad"
        );
        assert_eq!(strip_ansi("[31;1msqlite3: [0m"), "sqlite3: ");
        assert_eq!(strip_ansi("a [b] c"), "a [b] c");
    }

    #[test]
    fn sentences_split_at_ends_but_keep_versions_and_paths() {
        let s =
            split_sentences("Bumped to v1.0.1 in src/main.rs. Then ran the tests! All 12 pass.");
        assert_eq!(
            s,
            vec![
                "Bumped to v1.0.1 in src/main.rs.",
                "Then ran the tests!",
                "All 12 pass."
            ]
        );
    }
}
