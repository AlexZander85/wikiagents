<div align="center">

# WikiAgents 🧠⚡

**The Universal Local Memory & Desktop Command Center for AI Coding Agents.**  
*Single self-contained ~3.9 MB binary written in 100% Rust. Zero dependencies. Zero cloud. Zero Electron.*

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20macOS%20%7C%20Linux-lightgrey.svg)](https://github.com/AlexZander85/WikiAgents)
[![Binary Size](https://img.shields.io/badge/Binary%20Size-~3.9%20MB-brightgreen.svg)]()
[![Language](https://img.shields.io/badge/Language-100%25%20Rust-orange.svg)]()
[![Privacy](https://img.shields.io/badge/Privacy-100%25%20Local%20%26%20Offline-success.svg)]()

[English](README.md) • [Русский](README.ru.md)

</div>

> [!NOTE]
> **Lineage & Mission**: **WikiAgents** is an enhanced standalone fork based on the excellent work of [youdie006/sessionwiki](https://github.com/youdie006/sessionwiki). While the upstream project focuses strictly on a CLI and web browser viewer, **WikiAgents** evolves the codebase into a single self-contained desktop application with a native OS GUI (`wry`/WebView2), deep GUI usability refinements, long-term agent memory via MCP, instant extractive smart briefs, Activity Dashboard 2.0, autonomous background database compaction, server-side sorting, lazy streaming pagination, and major performance optimizations.

---

## ⚡ What is WikiAgents?

Every day, coding agents like **Claude Code, Codex, Antigravity, OpenCode, Qoder, ZCode, Cline, and DeepSeek** generate thousands of lines of code, architectural decisions, and bug investigations. By default, each tool stores its history in disparate, siloed formats across hidden directories. When tools prune history or when you switch agents, valuable context and solutions vanish.

**WikiAgents** unifies all your AI coding sessions into a single, blazing-fast, durable local database. It is delivered as a **single, dependency-free binary** containing:
1. **A Native Desktop GUI** (powered by native OS WebViews &mdash; no heavy Electron or Chromium bundled).
2. **Instant Extractive Summaries & Smart Brief** (offline TF-IDF + MMR engine generating instant `«task → result · turns · ✎files · ⚠errors»` previews and structured AI briefings in milliseconds without external LLM costs).
3. **Activity Dashboard 2.0** (local timezone awareness, 6 KPI cards with sparklines, streak counters, agent time breakdown, weekday × hour circadian heatmap, and project/file interactive filters).
4. **Autonomous Self-Compacting Database** (automatic background vacuum, zero-maintenance storage management, and contention cut from 1,091ms to 3ms via `IMMEDIATE` transactions).
5. **Rich Markdown-it 15 Engine** (GitHub alerts/admonitions `> [!NOTE]`, collapsible long code blocks >40 lines, emoji shortcodes, and lazy-loading).
6. **One-Click & Scheduled Background Sync** (manual `🔄 Sync` button with spinner + background auto-sync every 10 minutes).
7. **⭐ Favorites & Starred Sessions** (star crucial tasks, filter instantly with `⭐ Favorites (N)`, query via CLI & MCP).
8. **Durable Agent Memory via MCP** (`save_memory`, `list_snippets`, and `get_session_brief` for Claude, Codex, Cursor, and Antigravity to remember architectural decisions across turns).
9. **Microsecond Full-Text Search (FTS5 + Trigram)** across millions of messages, code changes, and projects.
10. **Curated Knowledge Base & Bookmarks** with multi-filter sorting and language detection.
11. **Code Provenance & AI Blame** tracing any modified line of code back to the conversation that created it.

---

## 🌟 Key Highlights

### 📑 Instant Extractive Summaries & Smart AI Brief (Zero LLM Latency)
- **Offline & Instant**: No external API keys, no network latency, zero token costs. Evaluated in milliseconds during ingestion using TF-IDF salience scoring and Maximal Marginal Relevance (MMR).
- **One-Line Session List Summaries**: Immediately displays `«request → result · turns · ✎files · ⚠unfixed errors»` directly in the session list, persisted in SQLite (`kind = 'auto'`).
- **Comprehensive Smart Brief**: Condenses multi-turn sessions into a structured markdown report:
  - **Goal & Outcome**: Extracted initial intent and verified conclusion.
  - **Turn-by-Turn Timeline**: Condensed chronological milestone progression.
  - **Key Points**: Critical insights and architectural decisions selected via MMR without redundancy.
  - **User Corrections**: Explicit user guidance and steering detected in the dialogue.
  - **Files Changed & Collapsed Tools**: Repeated commands collapsed by command name with their final execution status; errors grouped by signature and flagged as fixed or unfixed.
  - **Verbatim Final Exchange**: Preserves the exact latest dialogue state so no context is lost.
  - Available via the desktop **"AI Brief"** button, CLI (`wikiagents brief <id>`), and MCP (`get_session_brief`).

### 📊 Activity Dashboard 2.0 & Developer Analytics
- **Local Timezone Awareness**: Server returns UTC timestamps, and the client dynamically groups them by your local timezone (UTC+N). Evening and midnight coding sessions are mapped to the correct calendar day.
- **Main Session Isolation**: Subagent auxiliary runs are filtered out, reflecting true human-agent workflows without artificial metric inflation.
- **Realistic Time Accounting**: Normalizes multi-day resumed sessions (> 6 hours) to prevent idle time from distorting work metrics.
- **Flexible Periods**: Seamlessly toggle between `7 days`, `30 days`, `90 days`, `1 year`, and `All time` (persisted in `localStorage`).
- **6 KPI Cards with Sparklines**: Track *Sessions*, *Active Time*, *Active Days*, *Messages*, *File Edits*, and *Projects*, each with delta comparison against the previous period (`+18% vs prev period`), interactive sparklines, and tooltips.
- **Streaks & Peak Days**: Current active streak, all-time record streak with exact dates, and your most productive single day.
- **Circadian Heatmap (Weekday × Hour)**: Visual 7×24 map of session starts showing your peak coding hours in local time.
- **Sessions by Agent over Time**: Stacked bar charts with persistent agent color palettes, dynamically binned by days, weeks, or months.
- **Touched Languages & Hot Files**: Breakdown of modified files by programming language/extension, plus a Hot Files ranking with its own period filter.
- **Database Status Card**: Real-time view of SQLite file size on disk, free pages, auto-vacuum status, and last compaction date.
- **Every Chart Element is a Filter**: Clicking a bar, calendar day, project, agent, or file instantly filters the session list (including the new `project:...` search chip).

### ⚡ Autonomous Self-Compacting Database & Zero Contention
- **Self-Managing SQLite Engine**: Background worker thread checks every 10 minutes (after 3 minutes from launch) whether the database needs compaction &mdash; no manual `vacuum` commands required.
- **Safety Heuristics**: Compaction runs only when the user is idle for 2 minutes, no active sync is running, the database exceeds 16 MB, free pages exceed 10%, and disk space has at least $2x$ headroom.
- **Windows SQLite mmap Release**: Unlocks memory-mapped I/O handles on Windows, allowing SQLite and the NTFS filesystem to physically truncate and shrink database files on disk from hundreds of megabytes.
- **Lock Contention Cut to 3ms**: Migrated write transactions to `BEGIN IMMEDIATE`, completely preventing lock-wait storms between the background sync daemon, MCP server, and HTTP handlers.

### 🎨 Rich Markdown-it 15.0.3 & Formatting Engine
- **GitHub Alerts / Admonitions**: Styled colored cards for `> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]`, and `> [!CAUTION]` with localized titles and distinct SVG icons.
- **Collapsible Long Code Blocks**: Code blocks exceeding 40 lines automatically collapse with an elegant bottom gradient fade and a `▾ N lines` toggle button.
- **Emoji Shortcodes**: Real-time replacement of GitHub-style emoji codes (`:rocket:` ➔ 🚀, `:fire:` ➔ 🔥, `:white_check_mark:` ➔ ✅, etc.).
- **Autolink Punctuation Fix (`WWW_TAIL`)**: Prevents documentation files like `README.md` from accidentally converting into broken links while preserving standard `www.` web URLs.
- **Lazy Image Loading**: Images render with `loading="lazy"` and `decoding="async"` for silky-smooth scrolling.

### 🚀 100% Rust & Single Tiny Executable (~3.9 MB)
- **Zero runtime dependencies**: No Node.js, no Python, no Electron.
- **Instant startup** (< 20ms) and minimal RAM usage (< 35 MB).
- Compile once, run anywhere: Windows (`wikiagents-app.exe`), macOS, and Linux.

### 🔄 One-Click Sync & Background Scheduled Auto-Sync
- **Manual Sync (`🔄 Sync`)**: Real-time button right above the session list with animated spinner and `Last synced HH:MM` timestamp.
- **Automated Periodic Sync (`Auto-sync (10m)`)**: Runs silently in the background every 10 minutes, indexing newly created agent turns without interrupting your workflow.
- **Persistent State**: Auto-sync preferences and timestamps are saved in `localStorage`.

### ⭐ Starred Sessions & Favorites Management
- **One-Click Star / Unstar (`⭐`)**: Bookmark important debugging sessions, complex refactors, and architectural discussions.
- **Instant Filter Chip**: Dedicated `⭐ Favorites (N)` quick-filter chip in the sidebar to isolate starred work with zero typing.
- **Context Menu & CLI / MCP**: Right-click context menu (`Add to favorites`), CLI command (`wikiagents favorites`), and MCP tool (`list_favorites`).

### 🤖 18+ Agent Adapters Out of the Box
Automatically locates and indexes sessions from all major coding agents:
- **Anthropic Claude Code** (`~/.claude/projects`)
- **OpenAI Codex** (`~/.codex/sessions` & `thread_history_1.sqlite`)
- **OpenCode** (both **v1.x** SQLite/JSON and **v2.x** `session_v2` schemas)
- **Google Antigravity IDE & CLI (`agy`)** (`~/.gemini/antigravity` & `~/.gemini/antigravity-cli`)
- **ZCode** (`~/.zcode/cli`)
- **Qoder** (`AppData/Roaming/com.qoder.app.stable`)
- **DeepSeek Harness** (`~/.dsh/sessions`)
- **Kimi Code** (`~/.kimi-code/sessions`)
- **Hermes Agent** (`~/.hermes/state.db`)
- **Freebuff Desktop** (`~/.config/freebuff-desktop/projects`)
- **free-pi-cli & Pi** (`~/.free-pi/agent/sessions` & `~/.pi/agent/sessions`)
- **Cline, Roo Code, Kilo Code, Gajae Code** (`globalStorage`)
- **Aider, Continue.dev, Gemini CLI, Prodex, gptme**, and more (21 adapter configurations in total).

### 🧠 Model Context Protocol (MCP) Durable Memory
Empower your agents to remember solutions between independent tasks:
- **`save_memory`**: Coding agents store architectural invariants, debugging conclusions, and configurations directly into your local database.
- **`list_snippets`**: Agents recall past solutions filtered by programming language, tags, or role (`assistant_memory`).
- **`list_favorites`**: Agents query only your starred, highest-priority sessions for authoritative context.
- **`get_session_brief`**: Agents retrieve compact, structured extractive briefings of past sessions without blowing up their context window.

### 📌 Bookmarks & Solution Snippets
- Pin valuable code blocks, configurations, and prompts into a durable local knowledge base.
- Filter by agent tool, message role (`assistant_memory`, `user`, `assistant`, `tool`), or date.
- Sort by created time, original message date, relevance, title, or length.
- Double-click to rename; one-click jump back to the exact turn in the original transcript.

### ↩️ Rewound Turns & Compaction History
- AI sessions frequently rewind or compact context when token limits are reached.
- WikiAgents preserves superseded turns with an interactive **"↩ Rewound"** toggle in the viewer.

---

## 🚀 Quick Start

### 1. Download Prebuilt Executable
Grab the latest release from the [Releases](https://github.com/AlexZander85/WikiAgents/releases) page:
- **Windows**: `wikiagents-app.exe` (or `wikiagents.exe` for CLI-only)
- **macOS / Linux**: `wikiagents-app` / `wikiagents`

*No installer or prerequisites needed &mdash; just double-click and run!*

### 2. Launch Native Desktop App
```bash
./wikiagents-app
```
WikiAgents automatically scans your local disks, discovers existing AI coding sessions, indexes them in parallel, and opens the native dashboard.

### 3. Connect via MCP (Claude Desktop, Cursor, Antigravity)
Add WikiAgents to your MCP config (`claude_desktop_config.json` or `cursor.json`):
```json
{
  "mcpServers": {
    "wikiagents": {
      "command": "wikiagents",
      "args": ["mcp"]
    }
  }
}
```

Now your agents have direct, read-and-write access to your local knowledge base using:
- `search_sessions(query, project)` &mdash; search past solutions across all tools and specific projects.
- `get_session_brief(id)` &mdash; retrieve bounded, structured briefings of specific sessions.
- `save_memory(title, content, tags)` &mdash; save architectural decisions.
- `list_snippets(role="assistant_memory")` &mdash; retrieve durable memories.
- `list_favorites()` &mdash; retrieve starred reference sessions.

---

## 💻 CLI Commands

WikiAgents includes a comprehensive CLI alongside the desktop GUI:

```bash
# Scan and report disk space used by AI coding agents
wikiagents scan

# Manually trigger index sync across all agent stores
wikiagents sync

# Generate an instant extractive smart brief for a session
wikiagents brief <session-id>

# List starred / favorite sessions
wikiagents favorites --json

# Full-text search across all agent conversations
wikiagents search "JWT authentication with RS256"

# Trace which AI session created or modified a specific file
wikiagents trace src/auth/jwt.rs

# Git blame with session attribution
wikiagents blame src/auth/jwt.rs

# Start the local browser server (optional)
wikiagents web --port 7575

# Optimize FTS5 search index and reclaim disk space
wikiagents vacuum

# Self-check health and discoverable agent stores
wikiagents doctor
```

---

## 🎯 AI Git Blame: точный режим

`wikiagents commit <sha>` answers from the commit itself when the commit says
which session made it. Without that mark, it can only guess from the transcript.

```bash
# Once per repository: install the prepare-commit-msg hook
wikiagents hook install-git
wikiagents hook install-git --repo D:/projects/api
```

The hook appends one trailer line to every commit message:

```
fix: refresh the expired token

AI-Session: 019a2b7c3d
```

- The session id comes from `WIKIAGENTS_SESSION_ID` (set it for any agent, or in
  a wrapper script); with no such variable, the hook reads
  `<.git>/wikiagents-session`, which the SessionStart hook writes for Claude
  Code. Older than 12 hours is ignored, and anything ambiguous is left alone.
- `wikiagents hook install-git` never overwrites a hook that is not ours - it
  prints the one line to add instead.
- `wikiagents commit <sha>` then reports `AI-Session trailer (exact)`. Sessions
  that merely *quoted* a SHA (`git log`, a `/commit/<sha>` link) are no longer
  credited with it at all; if several still claim it, they are all listed as
  ambiguous instead of one being picked silently.
- A merge or squash message keeps no trailer: those commits were not made here.

---

## 🏛️ High-Performance Architecture: Why Native OS WebView?

WikiAgents delivers a fluid, instant UI while maintaining a microscopic ~3.9 MB binary footprint and ~35 MB RAM footprint by using an optimal division of labor between Rust and the OS:

### 1. Zero-Electron Native OS WebView (`wry` + `tao`)
- **No Bundled Chromium / Node.js**: Unlike Electron apps (which weigh 120–180 MB and consume 200+ MB RAM), WikiAgents leverages the operating system's native hardware-accelerated web engine:
  - **Windows**: Microsoft Edge WebView2 (preinstalled in Windows 10/11, GPU DirectComposition).
  - **macOS**: Apple WebKit (`WKWebView` with Metal acceleration on Apple Silicon & Intel).
  - **Linux**: WebKitGTK (`libwebkit2gtk-4.1`).
- **Native OS Font Smoothing & Anti-Aliasing**: Sub-pixel text rendering, crisp font ligatures (`Fira Code`, `JetBrains Mono`), emoji fallback, and seamless multi-monitor DPI scaling.
- **Pure Native CLI/MCP Mode**: When invoked via CLI (`wikiagents search`) or MCP by AI agents, the GUI framework is not even loaded, executing in pure Rust with ~10 MB RAM.

### 2. Client-Side Markdown with Lazy Rendering vs. Server-Side Pre-Rendering
Why parse Markdown via `markdown-it` in the WebView rather than pre-generating HTML in Rust?
- **3x–5x Smaller Payloads**: Rust streams compact raw Markdown JSON directly from memory-mapped SQLite. Sending pre-rendered HTML would bloat payloads from 150 KB to 2.5 MB due to thousands of `<pre>`, `<code>`, and syntax `<span>` tags, degrading serialization and IPC throughput.
- **Zero-Lag Lazy Loading (`IntersectionObserver`)**: Opening a session with 500+ messages renders only the first 30 visible turns eagerly (< 15 ms). Off-screen messages are parsed on-demand as you scroll, completely eliminating DOM layout thrashing.
- **Safe Dynamic Highlighting & Interaction**: Search keyword highlighting (`<mark>`), one-click code copy badges, foldable tool executions, and loss-free "Copy as Markdown" are executed natively within the DOM without risk of corrupting server-side HTML entity trees.

---

## 🔒 Privacy & Local Security

- **100% Local**: No telemetry, no tracking, no cloud dependencies. Your data never leaves your workstation.
- **Zero External Network Calls**: All briefings, indexing, search, and analytics run entirely offline on local CPU.

---

## 🙏 Acknowledgments & Credits

WikiAgents stands on the shoulders of open-source excellence:
- **[youdie006/sessionwiki](https://github.com/youdie006/sessionwiki)** &mdash; Sincere gratitude to the author of the original project for the rock-solid Rust foundation, the elegant SQLite FTS5 architecture, and the extensible adapter design for integrating diverse AI coding tools.
- **[markdown-it](https://github.com/markdown-it/markdown-it)** &mdash; Huge thanks to the authors and maintainers of markdown-it for the lightning-fast, highly extensible, and rock-solid Markdown parser and rendering engine.

---

## 📄 License

Licensed under the [MIT License](LICENSE).
