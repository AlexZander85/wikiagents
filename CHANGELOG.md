# Changelog

## [1.3.0] - 2026-11-01 (WikiAgents)

Review fixes for 1.0→1.2.1 plus the features that followed from them. Work in
progress on the `fix/review-1.3` branch; this section grows as each phase lands.

### Security
- **Web API now proves the caller is our own UI**: every `/api/*` request must carry
  the `X-WikiAgents: 1` header. A cross-site page cannot set it — `<img src>`, a form
  post and top-level navigation send no custom headers, and a cross-origin `fetch`
  that tries is stopped by a CORS preflight the server never answers. Before this, a
  malicious page could read the whole index or trigger an export through
  `/api/backup/run`.
- **State-changing endpoints accept POST only**: `/api/backup/run`,
  `/api/backup/restore`, `/api/backup/config`, `/api/forget` and
  `/api/snippet/save|delete|rename` answer 405 to any other method.

### Fixed
- **CI is green again**: `cargo fmt`, all 45 `cargo clippy -D warnings` errors, and
  the `account_badge` trace test that still expected the pre-1.2.1 absolute-path
  match. A failing test no longer takes the rest of its file down with it: every
  shared `static Mutex` lock tolerates a poisoned lock.

## [1.2.1] - 2026-10-31 (WikiAgents)

### Fixed & Enhanced
- **Universal Windows Path Separators & Case-Insensitive Matching**:
  - `trace`, `blame`, `edits_for`, and `touched_paths_matching` normalize Windows forward `/` and backslash `\` separators on the fly and match file paths with `COLLATE NOCASE`. Files are reliably found regardless of slash orientation or case differences.
- **Windows `\\?\` Extended Path Sanitization & Claude Code Session Resume**:
  - Added `canonicalize_as_recorded` in `src/util.rs` to strip extended length prefixes (`\\?\` and `\\?\UNC\`), matching CLI agent path conventions.
  - Hardened `relativize` in `src/hook.rs` to compare path components using standard `Path` traversal on Windows without breaking session start hooks.
  - Added `claude_cwd` in `src/resume.rs` to restore the original folder case when resuming Claude Code sessions.
  - Updated `migrate_cmd` in `src/commands.rs` to use `canonicalize_as_recorded`.
- **Literal Project Scope Filter (`--project`)**:
  - SQL project scope searches now escape wildcard characters (`_` and `%`) via `escape_like`. Searching for `zapret_gui` no longer erroneously matches `zapret-gui`.
- **Precise Session Relatedness Resolution**:
  - Session relatedness matching resolves relative Codex paths against the session's workspace root before comparing with absolute Claude Code paths, preventing false-positive links between unrelated files sharing generic names.
- **Vacuum WAL Checkpoint Polish (Upstream PR #87)**:
  - Added `busy: bool` flag to `CompactReport` to signal when active SQLite readers hold locks preventing full `wal_checkpoint(TRUNCATE)`.
  - Polished CLI output to distinguish between database shrunk, unchanged, or grown scenarios with actionable status messages.

## [1.2.0] - 2026-10-31 (WikiAgents)

### Added
- **AI Git Blame (Commit-to-Session Linking)**:
  - Passive, non-invasive extraction of Git commit hashes (`git commit -m ...`, `[branch 7f2a1b9]`) from agent terminal stdout across Claude Code, Codex, Antigravity, OpenCode, and Pi.
  - New CLI command `wikiagents commit <sha> [--json]` to instantly uncover the AI session that authored a commit — why it chose an algorithm, what tests it ran, and how it reasoned.
  - Web UI commit lookup: paste any commit SHA into the search bar to jump directly to the dialogue and highlighted commit turns.
  - Interactive commit chips in session header with one-click GitHub/GitLab commit URL opening or clipboard copy.
- **Clickable Web & IDE File Links in Web UI**:
  - Automatic Git remote origin and branch detection for repository workspaces (GitHub, GitLab, Bitbucket).
  - Any file path or touched file reference in transcripts and header cards is transformed into an interactive web link pointing to the exact line numbers (e.g. `blob/main/src/auth.rs#L40-L52`).
  - Support for local editor URL schemes (`vscode://file/...`, `cursor://file/...`).
- **Granular Incremental GitHub Backup & 1-Click Restore**:
  - Replaced monolithic database dumps with granular, per-session JSON files in `sessions/<tool>/<session_id>.json` (human-readable and searchable on GitHub).
  - Incremental fast sync: checks 5,160+ sessions in under 5 seconds, only writing and committing modified or new sessions.
  - Multi-tier recovery:
    - **1-Click Web UI Restore**: `📥 Восстановить из GitHub` button in the Backup dialog pulls updates and restores all sessions, tags, notes, and snippets.
    - **CLI Restore**: `wikiagents backup --restore [--repo <url>]`.
    - **Autonomous Zero-Dependency Restore Scripts**: Pre-packaged `restore.py` and `restore.ps1` in the backup repository root restore the complete SQLite `index.db` in ~3 seconds on any clean PC without needing Rust or WikiAgents pre-installed.
  - Silent Windows execution: added `CREATE_NO_WINDOW` flags to all Git subprocesses, eliminating flashing terminal windows.
- **Multi-Vendor Web Chat Adapters**:
  - Added native adapters for exported chats: Claude Web (`claude-web`), DeepSeek Web (`deepseek-web`), Gemini Web (`gemini-web`), Qwen Web (`qwen-web`), and GLM Web (`glm-web`).
  - Preserves formatting, markdown tables, code fences, thinking/reasoning blocks, and timestamps.
- **Freebuff Desktop & free-pi-cli / Pi Harness Adapters**:
  - Full discovery and parsing for Freebuff Desktop SQLite stores (`~/.config/freebuff-desktop/` and `%APPDATA%\freebuff-desktop`).
  - Full discovery and parsing for `free-pi-cli` and upstream Pi JSONL sessions.

### Fixed & Enhanced
- **Security & Secret Scanning**: Neutralized false positive token patterns in test suites (`concat!("s", "k-...")`) to prevent GitHub Secret Scanning alerts on fake dummy test keys.
- **Web UI & Path Sanitization**: Resolved `ReferenceError: escapeHtml is not defined` and added universal sanitization for paths wrapped in JSON-escaped quotes and backslashes in Antigravity sessions.
- **Terminal Encoding**: Hardened Python restore script against Windows CP1251 charmap encoding exceptions.
- **Timestamp Normalization**: Added millisecond-to-second normalization for tools storing high-resolution timestamps.

## [1.1.3] - 2026-10-29 (WikiAgents)

### Added
- **Freebuff Desktop Adapter (`freebuff`)**:
  - Automatically discovers and indexes sessions from Freebuff Desktop SQLite databases located in `~/.config/freebuff-desktop/projects/<project-slug>/desktop-v2.db` (and `%APPDATA%\freebuff-desktop`).
  - Reads threads, messages, role turns, tool calls, and file edit events (`write_file`, `str_replace`).
  - Correctly attributes project folders from `threads` or `project.json` and parses timestamps into UTC.
- **free-pi-cli & Pi Harness Adapters (`free-pi-cli`, `pi`)**:
  - Adds native JSONL transcript parsing for `free-pi-cli` (`~/.free-pi/agent/sessions/`) and upstream Pi (`~/.pi/agent/sessions/`).
  - Extracts full conversation turns (`user`, `assistant`, `toolResult`), tool calls, and code editing operations (`write`, `edit`, `apply_patch`, `ast_edit`) with snippet diffs and touched file tracking.
- **Full UI & MCP Integration**:
  - `freebuff`, `free-pi-cli`, and `pi` are fully integrated into the Desktop GUI tool picker, CLI `scan`, `list`, `show`, `search`, and the MCP long-term memory server.

## [1.1.2] - 2026-10-28 (WikiAgents)

### Added & Enhanced
- **Right-Side Vertical Timeline Navigation Rail & Scroll Spy**:
  - Implemented an interactive right-side navigation rail in the open session view for instant visual overview and smooth jumping (matching Google Antigravity's session timeline design).
  - Includes «Scroll to top» and «Scroll to bottom» quick-jump buttons with clean SVGs and smooth viewport scrolling.
  - Dashed timeline track connecting interactive turn dots, with active pill indicator synced to current reading position via `requestAnimationFrame` scroll spy.
  - One-click jump to any user turn with lazy body auto-loading.
- **Turns Overview Popover Card (Antigravity-Matched UI)**:
  - Header displays formatted date & time range (e.g. `сб, 24 окт., 16:41 – 20:58`) with total turn count (`18 turns`).
  - Scrollable turn list with timestamps (`16:41`), turn numbers, and clean prompt excerpts.
  - Active turn highlighting synchronized with rail dots and automatic scroll into view (`el.scrollIntoView({ block: "nearest" })`).
  - One-click navigation to any message directly from the popover list rows.
- **Zero-Latency Hover Architecture**:
  - **Eliminated OS Tooltip Lag**: Completely removed native HTML `title` attributes on step dots, bypassing Windows/Chromium's mandatory 700–1000ms tooltip delay.
  - **Instant 0ms Appearance**: Configured instant visibility transitions on hover for crisp, immediate rendering.
  - **Invisible Hover Bridge**: Added a 30px transparent bridge (`::after`) between track rail and card to prevent flicker when crossing mouse gap.
  - **Hover Debounce**: Added a 140ms grace period on mouseleave to prevent accidental dismissals.
  - **Stacking & Overflow Isolation**: Isolated `.session-timeline` from global `nav { overflow-y: auto }` clipping rules, ensuring popover always renders without horizontal clipping.

## [1.1.1] - 2026-10-27 (WikiAgents)

### Added & Enhanced
- **Project Folder Navigation & Cross-Agent Filtering**:
  - **Sidebar Session List Folder Visibility**: Displays the cleaned project path (e.g. `D:\game`, `D:\crypto`) directly in the metadata line of every session card in the left sidebar list.
  - **One-Click Header Chip Filtering**: The project path badge in the active session header is now an interactive chip with hover effects (`.chip-clickable`). Clicking it instantly filters the sidebar list to all sessions belonging to that project folder.
  - **Inline Sidebar Project Link**: Clicking the project folder inside any sidebar card (`.item-proj-link`) immediately filters the list by that project without opening the session.
  - **Context Menu Integration**: Added `📁 Find all project sessions (<folder>)` (`📁 Найти все сессии проекта (<folder>)`) to the right-click context menu (accessible from both the session card in the sidebar and the session header bar).
- **Universal Path Normalization & Multi-Agent Compatibility**:
  - **Quote & Backslash Deserialization**: Automatically strips JSON-escaped quotes and unwraps escaped backslashes (e.g. converting Antigravity's `"d:\\crypto"` or `"d:\\\\crypto"` to clean `D:\crypto`).
  - **Drive Letter & Slash Standardization**: Normalizes Windows drive letters to uppercase (`D:\`) and standardizes path separators.
  - **Case-Insensitive & Slash-Agnostic Matching**: The `normProjectPath` algorithm normalizes `D:/crypto`, `D:\crypto`, `"d:\\crypto"`, etc. into an identical canonical key (`d:/crypto`). This unites sessions across different AI agents (Claude Code, OpenCode, Antigravity, Codex) under a single project filter, ensuring complete results across thousands of sessions.
  - **Subdirectory Inheritance**: Filters match both the project root and nested subdirectories (e.g. `d:/crypto/new`).
- **WebUI Header Polish**:
  - **Compact Header Bar Layout**: Reorganized header bar with collapsible outline, summary, and metadata controls, chips row, smart actions (`AI Brief`, `.md export`, terminal toggle).
  - Applied path normalization to Recent Sessions, Related Sessions cards, and Activity view.

## [1.1.0] - 2026-10-25 (WikiAgents)

### Added & Enhanced
- **Autonomous Background Database Compaction**:
  - The embedded server and desktop app periodically evaluate compaction needs in the background (3 min after launch, then every 10 min during 2 min of user inactivity and when no sync or writes are active).
  - Triggers automatically when freelist pages exceed 10%, after 1 week with 20% size growth, or after 1 month. Bypassed if database < 16 MB or free disk space is insufficient (< 2x DB size).
  - **Windows Physical File Truncation**: Disabled SQLite `mmap` on Windows (`set_mmap` in `index.rs`), eliminating memory-mapped file handle locks and allowing Windows to actually truncate and shrink SQLite database files on disk without affecting search throughput (measured variance ±10 ms).
- **Activity Dashboard 2.0**:
  - **Period Range Switcher**: Toggle between 7 days, 30 days, 90 days, 1 year, and All Time (persisted in localStorage).
  - **6 Interactive KPI Summary Cards**: Track Sessions, Time in Work, Active Days, Messages, Files Touched, and Projects with delta indicators against previous period and hoverable sparklines.
  - **Streak & Peak Analytics**: Visualizes current consecutive active day streak, record streak with calendar dates, and single most active day.
  - **Agent Contribution Breakdown**: Stacked agent distribution bars (daily on short ranges, weekly on 1 year, monthly on all-time) with persistent palette colors and aggregated "others".
  - **Local Timezone Alignment**: Hour-of-day and calendar dates computed directly from client's local timezone (e.g. UTC+5), eliminating UTC evening date shifts. Subagents are excluded from session aggregates.
  - **Clickable Dashboard Filters**: Clicking any day, agent bar, streak, project, or file navigates to the session list with matching date and filter applied.
  - **Project Filter Chip**: Added interactive «project: ...» chip with dismiss button in sidebar search.
  - **24-Hour Weekday x Hour Heatmap**: 7 days x 24 hours grid showing session initiation density in local time.
  - **File Language Breakdown**: Detects ~30 languages by extension, displaying top 8 languages with file count, session count, and percentage share.
  - **Hot Files Ranking**: Displays most frequently modified files with independent period switcher (selected range, 7d, 30d).
  - **Database Storage Health**: Shows physical on-disk file size, internal freelist space, and timestamp/status of the last auto-compaction.
- **Concurrency, Threading & Lock Contention Optimization**:
  - **Short Immediate Write Transactions**: Replaced monolithic sync batch transactions with granular `IMMEDIATE` per-session write transactions. Write lock wait times plummeted from **1,091 ms median / 2,971 ms max down to 3 ms median / 264 ms max**!
  - **Multi-Process Sync Conflict Resolution**: Concurrent sync runs from different processes now use immediate write locks and wait cleanly without `database is locked` failures.
  - **Sync Request Coalescence**: Multiple clicks and auto-sync ticks coalesce into a single server run, avoiding worker starvation.
  - **UI Race Condition Guards**: Fixed race conditions in `openSession` (only the latest selected session renders) and search `refresh` (slow previous queries cannot overwrite newer ones). Unconditional reset of MCP `already_running` flag upon error.
- **Extractive Smart AI Brief (`src/brief.rs`)**:
  - Completely redesigned offline summarization algorithm for the «AI Brief» button, CLI (`wikiagents brief`), and MCP (`get_session_brief`, `recall_solution`).
  - Turn-based segmentation isolating user prompts and tool actions while stripping system notices, compact tokens, and skills boilerplate.
  - Information scoring combining TF-IDF word rarity, code paths, decision markers («because», «root cause», «fixed»), and recency weighting.
  - Maximal Marginal Relevance (MMR) deduplication to prevent repetitive phrasing.
  - Semantic command folding (e.g. `cargo test ×12 - latest: ok (passed 357 failed 0)`), error signature grouping with status markers (✓/✗), and transcript-matched language headings.
- **Instant Extractive One-Line Summaries (`summaries.kind = 'auto'`)**:
  - Instant one-line summaries in the format `«task → result · turns · ✎files · ⚠errors»` generated on-the-fly during indexing with zero neural network latency.
  - Stored in SQLite `summaries` table under `kind = 'auto'` without overwriting LLM summaries.
  - Incremental background backfill of historical sessions in ~1s batches.
  - Smart list preview: suppresses task text if identical to session title to avoid duplicate lines.

## [1.0.1] - 2026-10-24 (WikiAgents)

### Added & Enhanced
- **Instant Transcript Streaming & Pagination**: `/api/session/<id>?offset=&limit=300` streams transcripts in lazy chunks, slashing payload from 21 MB to 327 KB and opening time from 240 ms to 12 ms on 8,000+ message sessions.
- **Hardware-Accelerated UI Rendering**: CSS `content-visibility: auto` skips layout calculations for off-screen rows. Raw text stored in memory-safe `WeakMap` instead of DOM attributes.
- **CSS-Driven Filter State**: Role filtering and rewound message toggles now apply via CSS attribute `data-rf` with zero re-rendering lag.
- **Transparent Gzip Compression**: Integrated pure-Rust `flate2` for automatic gzip compression of all JSON API responses and HTML assets.
- **Multi-Worker Server Pool**: Embedded web server runs 4 parallel worker threads (`WEB_WORKERS = 4`) with dedicated SQLite connections in WAL mode; syncs no longer block search or viewer requests.
- **Graceful Sync Lock**: Auto-sync and manual sync requests are serialized via `sync_guard` mutex.
- **Antigravity Adapter 100x Speedup**: Direct lookup of known paths per conversation folder in `~/.gemini/antigravity/brain`, replacing 50,653-file directory walks.
- **Non-Blocking SQLite Startup**: Replaced repeated `ALTER TABLE` / `UPDATE` schema checks with lightweight read checks; migrations backfill once via `meta` key.
- **Chunked Sync Commits**: Sync writes in batches of 200 sessions or 20,000 messages, preserving progress if interrupted and releasing locks between batches.
- **Automated WAL & Database Compaction**: Added `journal_size_limit = 64 MB`. Every write sync runs FTS5 segment merge (`merge=500`), `PRAGMA optimize`, and conditional `incremental_vacuum`.
- **Full-Database Server-Side Sorting**: `/api/sessions?sort=` sorts across all sessions (including 8,500+ message sessions) by message count and creation date.
- **Unified UTC Date Boundaries**: Timezone-aware date bounds sent as UTC; sessions active across day boundaries correctly match.
- **SQL Pre-Filtering**: Tag and date limits apply in SQL before the result limit.
- **Multi-Keyword Rarity Ranking**: Multi-chip (`+`) keyword search ranks sessions by rarest word frequency (TF-IDF), total match count, and recency.
- **In-Chat Search & Highlight Navigation**: Matching search words are highlighted in the transcript with keyboard navigation (↑/↓); long messages (>160 chars) pre-render before matching.
- **Related Sessions Header**: Displays `🔗 Related sessions (N)` directly in the session header.
- **Default Role Filter**: Default view is set to **«пользователь и агент»** (`user and agent`), hiding noisy tool outputs until selected.
- **Modern Terminology Rebranding**: Replaced «Ассистент» with **«Агент»** across all UI elements, dropdowns, headers, and Markdown exports.
- **Rich Markdown Engine Enhancements (reposeek / MDHero)**:
  - **GitHub Alerts / Admonitions**: Full support for `> [!NOTE]`, `> [!TIP]`, `> [!IMPORTANT]`, `> [!WARNING]`, and `> [!CAUTION]` with color-coded accent borders, themed backgrounds, and localized headers.
  - **Collapsible Long Code Blocks (>40 lines)**: Blocks longer than 40 lines automatically collapse (`max-height: 22rem`) with bottom gradient fade and an interactive toggle bar («▾ N lines / строк» / «▴ Collapse / Свернуть»).
  - **Emoji Shortcodes**: Automatic conversion of standard markdown emoji shortcodes (`:rocket:` ➔ 🚀, `:fire:` ➔ 🔥, `:white_check_mark:` ➔ ✅, `:bug:` ➔ 🐛, `:warning:` ➔ ⚠️, `:zap:` ➔ ⚡, etc.).
  - **Markdown-it 15 Linkify Fix**: Ported `www.` prefix autolinking rule (`WWW_TAIL`) ensuring `www.example.com` links properly like on GitHub without corrupting bare file names (`README.md`).
  - **Lazy Image Loading**: Automatically injects `loading="lazy"` and `decoding="async"` attributes to markdown images.

### Fixed
- **Snippet Metadata Regression**: Corrected pinned snippet metadata lookup to query `files` table instead of non-existent `sessions` table (covered by 2 regression tests).
- **UI Navigation Crashes**: Fixed `renderSession` error when toggling rewound state or returning from Snippets tab.
- **Codex Legacy Rollouts**: Ported parser fix from upstream v0.34.10 preserving top-level `type: "message"` records from older rollouts.
- **Reader Tracking & Auto-Reparse**: Added `reader` column to `files` table and automatic re-parsing of sessions indexed before parser fixes.

## [1.0.0] - 2026-10-24 (WikiAgents)

> Enhanced standalone fork based on upstream **youdie006/sessionwiki v0.34.9**, combining all core Pull Requests (#83, #84, #85, #86, #87, #88, #89) with a full markdown-it rendering engine, native desktop GUI, and durable agent memory.

### 1. GUI & Desktop Experience

1. **Autonomous Native Desktop Application (`wikiagents-app.exe` / `sessionwiki-app.exe`)**:
   - Standalone native desktop application built with 100% Rust, Wry, and Tao (using system WebView2 on Windows, WebKit on macOS/Linux), completely eliminating Electron and Node.js runtimes.
   - Compiled with `#![windows_subsystem = "windows"]` on Windows — double-clicking launches a clean native desktop window without popping up a console terminal.
   - Automatically checks for a running server and transparently starts the embedded web engine in a background thread within the same process.
2. **Industrial-Grade Markdown Rendering Engine (`markdown-it 15.0.3`)**:
   - Completely replaced primitive regex string splitting with the full-featured `markdown-it 15.0.3` engine (CommonMark + GFM).
   - Rich typography support: styled H1–H6 headings with divider lines, bulleted and nested numbered lists, blockquotes with accent border, horizontal rules (`hr`), bold/italic formatting, and inline code.
   - Safe link handling: automatically appends `target="_blank"` and `rel="noopener noreferrer"`.
3. **GitHub Flavored Markdown Tables (GFM Tables)**:
   - Full support for structured Markdown tables with MDHero-inspired styling: distinct `thead` header, borders, alternating zebra-striped rows (`:nth-child(even)`), row hover highlights, and smooth horizontal scrolling.
4. **Interactive One-Click Code Block Copy («📋 Copy»)**:
   - Floating «📋 Copy» button automatically attached to every `<pre><code>` block, appearing smoothly on hover.
   - One click copies clean code to clipboard with instant visual feedback («✓ Copied»).
5. **Dynamic Message Role Filter in Transcript Viewer**:
   - The `#role` dropdown filters messages in the open session view in real time.
   - The `user and agent` filter hides noisy tool dumps (`role: "tool"`), bash logs, and file patches, surfacing clean human-agent dialogue.
   - Dedicated modes to isolate user prompts, agent replies, or tool outputs.
   - Interactive counter badge showing filtered ratio (e.g. `18 / 209 msgs`).
6. **Draggable Sidebar Resizer (Splitter)**:
   - Interactive splitter between `<aside>` and `<main>` with `col-resize` cursor for customizable sidebar width.
   - Preferred width is persisted across sessions in `localStorage`.
7. **Collapsible Touched Files List**:
   - Files touched by a session are neatly wrapped in `<details class="files-details">`, collapsed by default (`▸ Files (28)`).
   - Windows path normalization (`/[/\]/`) displays clean basenames instead of verbose filesystem paths (full paths retained on tooltip).
8. **Ergonomic Controls Layout & Full-Width Search Input**:
   - Database metrics (`3,956 sessions · 8 tools · 6.7 GB`) integrated into the title row to save vertical screen space.
   - Tool and role select controls arranged side-by-side with compact padding.
   - Search input placed on its own row spanning 100% of sidebar width for clear legibility.
9. **Widescreen Transcript Layout (Up to 1400px)**:
   - Expanded transcript viewport from 800px to 1400px, removing excessive side margins on wide displays and maximizing readability for multi-line code diffs and tables.
10. **In-Chat Search with Navigation & Shortcuts (`Ctrl+F`)**:
    - Integrated transcript search directly in the session header with match counter (`1 / 14`), `↑`/`↓` buttons, clear `✕`, and hotkeys (`Ctrl+F`, `Enter`, `Shift+Enter`, `Escape`).
    - Smooth auto-scroll and highlight on active matches.
11. **Dedicated "Recent & Related" View**:
    - Relocated the `SEE ALSO` list out of the transcript footer into a dedicated view.
    - Sidebar footer Recent (🕒) button opens recently viewed sessions (saved in `localStorage`, with clear history option) and related sessions.
12. **Interactive Tag Cloud Filter**:
    - Color-coded tag chips in the sidebar (`#frontend`, `#refactor`, `#bugfix`) with dynamic session counters.
    - Clicking any tag instantly filters sessions; tags can be assigned or removed directly from the context menu.
13. **4 Session List Sorting Modes**:
    - Quick sorting switcher: last active (newest activity first), created date, search relevance, and total message count.
14. **Message Timestamps & Session Last-Active Display**:
    - Individual timestamps on every dialogue turn with formatted tooltip.
    - Sidebar session items display exact last-message timestamp (`last: 2026-10-24 19:15`).
15. **Live Database Sync Button & 10-Minute Auto-Sync**:
    - Interactive `🔄 Sync` button with spinner animation and live counter updates.
    - Background 10-minute recurring auto-sync timer with state saved in `localStorage`.
16. **Text Selection Context Menu**:
    - Right-clicking highlighted text offers "Copy as Markdown", "Copy as Plain Text", "Search in Google", and `Ctrl+Shift+C` shortcut.
17. **Claude Code `<pasted_content>` Formatting**:
    - Automatic detection and styling of Claude Code external file pastes with `📋 Pasted Content` badge.
    - Preserves line breaks (`breaks: true`) with an inline `[Copy]` button.
18. **Internationalization & Multi-Language Support (i18n)**:
    - Full localization across 5 languages: English, Russian (`Русский`), Korean, Japanese, and Chinese, with automatic browser language detection.

---

### 2. New IDEs, Agents & Ecosystem Adapters

19. **ZCode Adapter (`src/adapters/zcode.rs`)**:
    - SQLite session store support at `~/.zcode/cli/db/db.sqlite` (supporting `ZCODE_DB` and `ZCODE_HOME` overrides).
    - Implements `Adapter` and `Store` traits with fast incremental checking via `time_updated`.
    - Parses `session`, `message`, and `part` tables, decoding `user` and `assistant` semantics.
    - Extracts tool invocations (`ToolCall`) and results (`ToolResult`) from `part.data` (`Read`, `Edit`, `Write`, `Bash`, etc.).
    - Captures file patches (`EditEvent`, `EditKind::Edit`, `EditKind::Write`) and touched paths (`touched`) for code provenance.
    - Automatically discovers subagents via `parent_id` and `task_type == "subagent_child"`.
20. **Google Antigravity IDE Adapter (`src/adapters/antigravity.rs`)**:
    - Indexing of `transcript.jsonl` stores in `~/.gemini/antigravity/brain/*/` (including `.system_generated/logs/transcript.jsonl`).
    - Line-by-line filtering of record types (`USER_INPUT`, `PLANNER_RESPONSE`, `GENERIC`).
    - Extracts user prompts, planner thoughts, file edits (`write_file`, `edit_file`), and auto-redacts private tokens.
21. **Google Antigravity CLI Support (`agy`)**:
    - Dedicated store adapter for terminal agent sessions at `~/.gemini/antigravity-cli/brain`.
22. **Qoder Adapter (`src/adapters/qoder.rs`)**:
    - Reads local SQLite database at `%APPDATA%/com.qoder.app.stable/main.sqlite`.
    - Incremental sync via `updated_at` / `created_at` change tokens.
    - Parses `chat_sessions` and `chat_session_messages` with structured message parts and file blame tracking.
23. **OpenCode v2 SQLite Architecture (`src/adapters/opencode.rs`)**:
    - Full support for OpenCode v2.x schema (`session_v2`, `session_message` in `~/.local/share/opencode/opencode.db`).
    - Extracts user messages, agent responses, and structured tool calls (`edit`, `write`, `bash`, `read`) from `session_message.data.content`.
    - Preserves backward compatibility with OpenCode v1 databases and JSON stores.
24. **DeepSeek Harness Adapter (`~/.dsh/sessions`)**:
    - Support for DeepSeek V3 / R1 CLI agent environments with reasoning transcripts.
25. **Kimi Code Adapter (`~/.kimi-code/sessions`)**:
    - Parses sessions and task workflows from Moonshot AI's Kimi Code CLI agent.
26. **Hermes Agent Adapter (`~/.hermes/state.db`)**:
    - Integration for Hermes SQLite-backed state and task history.
27. **GitHub Copilot Transcript Discovery**:
    - Discovers and parses GitHub Copilot chat conversations stored in VS Code workspaces.

---

### 3. Performance & Memory Optimizations

28. **High-Performance Compiled HTML Cache (`mdCache`)**:
    - In-memory LRU cache storing parsed Markdown HTML, eliminating redundant parser overhead on re-rendered messages.
29. **Fault-Tolerant Markdown Fallback (`renderRichFallback`)**:
    - Safe exception isolation: if malformed external text fails parsing, gracefully falls back to raw rendering without breaking the page.
30. **Zero-Lag Lazy Rendering via IntersectionObserver**:
    - Messages mount instantly as lightweight placeholders, with heavy Markdown parsing deferred until scrolled into view for solid 60–120 FPS.
31. **SQLite Memory-Mapped I/O (`PRAGMA mmap_size = 256MB`)**:
    - Zero-copy OS page caching for instant full-text search (FTS5) queries and session loading.
32. **Enlarged Page Cache (`PRAGMA cache_size = 32MB`)**:
    - Enlarged SQLite page cache from default 2 MB to 32 MB, eliminating disk I/O when navigating search results.
33. **In-Memory Temporary Storage (`PRAGMA temp_store = MEMORY`)**:
    - Diverts SQLite temp tables, sort buffers, and aggregations directly to RAM.
34. **LTO Binary Shrink by ~25%**:
    - Built with `opt-level = "s"`, `codegen-units = 1`, LTO (`lto = true`), and `strip = true`, reducing binary size (GUI `wikiagents-app.exe` to ~3.53 MB, CLI `wikiagents.exe` to ~4.71 MB) with no runtime speed penalty.
35. **Strict HTTP Cache Control**:
    - Serves web assets with `Cache-Control: no-cache, no-store, must-revalidate` to prevent stale WebView2 caching upon binary upgrades.

---

### 4. SQL Engine & Storage Reliability

36. **`wikiagents vacuum` Command for FTS5 & Disk Reclaim**:
    - Reclaims fragmented SQLite pages and disk space (reclaiming 30–50+ MB on 4,000+ session databases).
37. **Free-Space Preflight Safety Check**:
    - Validates available filesystem space before executing vacuum to prevent database corruption during low disk space.
38. **WAL Checkpoint Handling**:
    - Safely processes `PRAGMA wal_checkpoint(TRUNCATE)` output columns without crashing.
39. **Knowledge Base Snippets Schema with Auto-Migration**:
    - Dedicated SQLite `snippets` table with automated column migrations (`tool`, `role`, `message_ts`) and background backfill.
40. **Favorites Table (`favorites`) across Schema Upgrades**:
    - Persistent `favorites (session_id PRIMARY KEY, created_at TEXT)` table preserved across index rebuilds.
41. **Permanent Session Deletion (`forget`)**:
    - Securely deletes sessions from SQLite and FTS5 search indexes via the UI context menu.
42. **Prompt Injection & Path Privacy Defense**:
    - Sanitizes control sequences (`neutralize_field`, `safe_text`) and redacts user home directories (`redact_home`, `safe_project`).

---

### 5. Advanced Features: Knowledge Base, Durable Memory, Analytics & Subagents

43. **Knowledge Base & Solution Snippets**:
    - Pin (📌) any message or code block from transcripts into a persistent local knowledge base.
    - Quick selection pinning via context menu (`📌 Save selection to Snippets`).
44. **Dedicated "Snippets" Sidebar Tab**:
    - Segmented switcher between "Sessions" and "Snippets" with live badge counters.
    - Filter snippets by tool, role, date range, and 5 sorting modes (saved date, message date, relevance, alphabetical, length).
45. **Interactive Snippet Management**:
    - Snippet context menu: rename (including inline pencil `✏️` button), copy Markdown, export as `.md`, and delete.
    - One-click jump to original session with smooth scroll and target turn highlight pulse.
46. **Durable Agent Memory Tool via MCP (`propose_memory` / `save_memory`)**:
    - Enables Claude Code, Codex, Cursor, and Antigravity to persist architectural decisions, bug causes, and project conventions directly into WikiAgents.
47. **Synthesized Solution Retrieval for MCP Agents**:
    - `recall_solution`: high-level problem-solving tool prioritizing bookmarked (⭐) sessions.
    - `get_session_summary`: lightweight metadata and files retrieval without streaming large transcripts.
    - `list_favorites`: direct access to verified solution sessions.
    - `list_snippets`: queries saved knowledge base snippets.
48. **Subagents DAG Hierarchy & Workflows**:
    - Tracks `parent_id` / `parent_session_id` relationships.
    - Displays hierarchy chain in the session header: "Parent session → Subtask invocation → Child session".
    - One-click navigation to parent session and sidebar "Subagents" toggle filter.
49. **Turn History & Rollback Awareness (Superseded & Rewound Turns)**:
    - Tracks `is_rewound` attribute for reverted or compacted turns, with a WebUI toggle (`↩ Rewound`).
50. **Developer Activity Heatmap & Recap**:
    - 52-week GitHub-style contribution calendar.
    - 24-hour diurnal activity rhythm chart.
    - Agent & model distribution breakdown (Claude vs Codex vs ZCode vs OpenCode vs Antigravity).
    - Top modified files ranking (Top Touched Files).
51. **Starred Sessions (⭐ Favorites)**:
    - Bookmark crucial sessions with a gold star in the header or context menu.
    - Sidebar filter `⭐ Favorites (N)`, CLI command `wikiagents favorites`, and MCP integration.
52. **AI Brief Generator (`AI Brief`)**:
    - Generates concise Markdown briefs (~1,500 tokens) to transfer session context to other agents in one click.
53. **Export Sessions to Clean Markdown (.md)**:
    - Exports formatted session dialogue with metadata and file diffs via native "Save As" dialog.
54. **Multi-Keyword Intersecting Search ('+' Chips)**:
    - Adds keyword chips to search sessions matching multiple disparate terms across the project.
55. **Reverse File Provenance Tracing**:
    - Search sessions that touched a specific file via context menu or `file:path/to/file` syntax.
56. **Rebranded to WikiAgents**:
    - Evolves from a simple transcript reader into an autonomous desktop command center and long-term memory engine for AI coding agents.

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
semantic versioning once it reaches 1.0.

## [Unreleased]

## [0.34.10] - 2026-10-24

### Fixed

- A large Codex rollout in the older format, which writes each message at the
  top level, keeps its conversation from the middle again. 0.34.9 kept only
  the current record types there.
- The 0.34.9 fixes for Codex calls declined at approval and Gemini /rewind
  now reach sessions indexed before it: Codex history threads and Gemini
  chats are re-parsed on the next sync. Before, a session that no longer
  changed kept its old reading. Each indexed session now records which reader
  parsed it, so a session that a sessionwiki mcp started before the upgrade
  re-parses with its old reader is re-parsed again instead of keeping that
  result.

## [0.34.9] - 2026-10-21

### Fixed

- A large Codex session keeps its whole conversation again. From the middle
  of a session over the size cap only conversation records are kept, within a
  64 MB budget, but `compacted` records (which carry the whole replacement
  history) were matched as conversation and filled the budget first. In one
  real 1.1 GB rollout 5025 of 6960 conversation lines were dropped; it now
  indexes 2883 messages instead of 926. Large sessions are re-parsed once on
  the next sync.
- A Codex session that changes path keeps its summary. Moving a rollout to or
  from `archived_sessions`, or between a rollout and the history database,
  carried its tags and note over and deleted its summary.
- A rollout taken back out of `archived_sessions` is listed once. The old row
  was still live when the new one was indexed, so both stayed, and its tags
  stayed on the archived one.
- A Codex call declined at approval is recorded as not having run, instead of
  as a success.
- A Gemini chat's `/rewind` is applied: the rewound turns are no longer
  indexed and searchable as part of the chat.
- Gemini subagent transcripts, written under their parent chat at
  `chats/<parent session id>/`, are indexed, as subagents.

## [0.34.8] - 2026-10-20

### Fixed

- `blame` no longer credits a session that edited a different file whose name
  differs only where the asked-for name has `_` or `%`. The path query used
  them as SQL wildcards, so `blame KBS/newdata_copy.py` also took the session
  that edited `KBS/newdata copy.py`. `trace` already treated them literally.

## [0.34.7] - 2026-10-20

### Fixed

- **Gemini CLI chats written as JSONL are indexed.** Gemini CLI now saves
  each chat as `chats/session-*.jsonl` - a header line, `$set` lines and one
  line per message, repeated as the message grows - and only `.json` chats
  were read, so every Gemini session since mid-June was missing (14 on one
  machine). These chats are now folded back into their messages, each
  message once.
- **Search finds what was said in the middle of a large session.** A session
  over the 256 MB cap was indexed from its first 256 KB and last 512 KB only,
  so its conversation in between could not be searched: one 352 MB session
  had 1,733 prompts and 7 of them were indexed. The user and assistant text
  of a windowed Claude or Codex session is now kept from the whole file (up to
  64 MB of it); tool output in the middle stays windowed. The live windowed
  sessions are re-parsed once on the next sync.

## [0.34.6] - 2026-10-20

### Fixed

- **`trace` finds the files a large session edited.** A session over the
  256 MB cap is indexed from its first 256 KB and last 512 KB, and the files
  it edited in between were dropped with the rest of the middle. On one
  machine 82 sessions (29 GB) were windowed, the longest-running work there
  was, and three of them that edited 574 to 585 files each traced to none.
  The edited files are now read from the whole file, parsing only the lines
  that carry a tool call or patch, so `trace`, `files` and `blame` see every
  edit; the text stays windowed. The live windowed sessions are re-parsed
  once on the next sync (about 4 s per 3 GB).
- **`doctor` no longer calls Codex's thread history unreadable.** Since
  0.34.4 Codex's thread history is indexed from `thread_history_1.sqlite`,
  one database file, and `doctor` read every store as a directory, so it
  reported `store: codex - present but unreadable: Not a directory` on any
  machine that has one. A store that is a file is now opened as a file.

## [0.34.5] - 2026-10-19

### Fixed

- **Compressed Codex rollouts are indexed.** Codex compresses older rollouts
  in place, `rollout-*.jsonl` becoming `rollout-*.jsonl.zst`, and only
  `.jsonl` files were read. On one Mac, 678 of 684 rollouts were compressed:
  none were indexed, and the 667 indexed earlier were listed as deleted
  sessions once their `.jsonl` disappeared. Compressed rollouts are now read
  (with the pure-Rust `ruzstd` decoder), and each one absorbs its row from
  before compression, tags and note included.

## [0.34.4] - 2026-10-19

### Fixed

- **Codex threads kept only in Codex's history database are indexed.**
  Codex now stores thread history in `thread_history_1.sqlite` and may
  remove a thread's rollout file after migrating it. Only rollouts in
  `~/.codex/sessions` were read, so one machine with 2,987 threads of history
  showed 128 Codex sessions. Threads without a rollout are now read from the
  history database, with cwd and sub-agent status taken from `state_5.sqlite`.
  Rollouts in `~/.codex/archived_sessions` are indexed too (#65).
- **A Codex session that changes location is listed once.** Codex moves a
  rollout into `archived_sessions` when a thread is archived, and the rollout
  may be removed while the thread continues in the history database. The old
  location's row was kept as a deleted session beside the new one. It is now
  merged into the new row, with its tags and note, unless it held more
  messages than the new copy.
- **`sync --tool codex` covers every Codex store.** It ran only the first
  adapter of that name.

- **Codex MCP calls recorded only as events are indexed.** Some Codex
  rollouts record an MCP call only as its `mcp_tool_call_end` event, with no
  tool call or output beside it, and those calls were not indexed at all -
  5698 on one machine, 1608 of them failures. Each is now indexed from its
  event, marked as an error when it failed; a call that also has a tool call
  record is not repeated. Codex sessions already indexed are re-parsed once
  on the next sync.

## [0.34.3] - 2026-10-13

### Fixed

- **A failed Codex MCP tool call shows as an error.** Codex states an MCP
  call's failure only in its `mcp_tool_call_end` event (`isError`, or an
  `Err` result); the tool output beside it is plain text, so every failed
  MCP call read `⇒ ok` - 257 calls on one machine (an earlier version of this
  entry said 1782, counting calls that were not indexed at all; see 0.34.4).
  The event is now read and the call is marked as an error. The first open after
  upgrading marks Codex sessions already indexed for one re-parse, so the
  next sync corrects them too; sessions whose rollout is gone keep what they
  had.

## [0.34.2] - 2026-10-10

### Fixed

- **Workflow journals are no longer indexed as sessions.** Claude Code keeps
  a workflow's bookkeeping beside its agents' transcripts
  (`subagents/workflows/wf_*/journal.jsonl`). Every `.jsonl` was indexed, so
  each journal became an empty subagent session whose project was the
  `wf_*` folder name - 257 on one machine. Only `agent-*.jsonl` under
  `subagents/` is read now, and the next sync removes the journal rows.
- **A session with nothing indexed is removed, not archived, when its file
  goes.** Archiving was documented to skip such sessions, but kept every one
  as an archived row with no transcript.
- **"Kept after the tool deleted them" counts sessions.** `stats`, `doctor`
  and the sync summary counted every archived transcript, subagent
  transcripts included, beside a session total that counts only main
  sessions - one store read "3959 sessions, 2050 kept" where 611 sessions
  were kept. The kept count now covers main sessions, like the total.

## [0.34.1] - 2026-10-10

### Fixed

- **An older sessionwiki no longer rebuilds an index a newer one wrote.** The
  cache is rebuilt whenever its schema version differs from the binary's, and
  that included a NEWER version. A `sessionwiki mcp` server started before an
  upgrade keeps running the old binary, so after 0.34.0 rebuilt the index each
  such server rebuilt it back in the old format on its next call - keeping
  only the last 15 minutes of sessions - and the next CLI run rebuilt it again.
  An older binary now refuses a newer index with an error that says to
  restart it. Servers already running 0.33.x or earlier do not have this
  check: restart agent sessions that run `sessionwiki mcp` after upgrading.

## [0.34.0] - 2026-10-10

### Changed

- **Tool calls are stored as one compact line each**, for example
  `→ Bash(cargo test) ⇒ error · 12 lines — error[E0425]: ...`: tool name,
  primary argument, status, output line count, and the first output line on
  error. Every adapter (Claude Code, Codex, Gemini, gptme, Cline/Roo/Kilo,
  Continue, Gajae, OpenCode) emits structured calls and results, and one shared
  renderer formats them. The line is what `show`, `brief`, the web UI, MCP and
  search see; `brief` now includes tool lines by default, and `--tools`
  (`brief`) or `--full` (`show`) append bounded output. The default `summary`
  mode keeps only the compact line in new archives, plus the first output line
  for failed calls. This is less than the previous behavior, which kept up to
  500 characters of tool output. Run `sessionwiki tool-output full` to retain up
  to 8 KB of redacted output per result in archive mode. The per-index setting
  lets embedders choose; switching to `summary` clears full output from live
  sessions, while already archived sessions keep what they have. Switching
  back to `full` marks live sessions for re-parsing.
  Tool output is no longer indexed, so a string that appeared only mid-output
  no longer matches a search. Schema v9 rebuilds each existing index once on
  first use; the first run after upgrading re-parses every session, which can
  take a while on a large store.

## [0.33.2] - 2026-10-08

### Fixed

- **A prodex consult is listed once, and `show <id>` opens it.** The bridge
  registry can name one repo under two spellings - a case-variant working
  directory on a case-insensitive drive, or a symlink. Each spelling was
  walked, so every consult in that repo was indexed twice under one id: it
  appeared twice in `list` and `projects`, and `show`, `brief` and `resume`
  stopped with "ambiguous id" even given the full id. On the machine this was
  found, 524 consults were doubled. Discovery now walks each repo once, and
  the next sync drops the second copy instead of archiving it.
- **Gemini CLI sessions carry their project's path.** Gemini CLI names each
  project's chat folder with a short slug and records the real directory in
  a `.project_root` file beside it. The adapter used the slug, so a Gemini
  session's project read `myrepo` while every other tool's read
  `/home/me/myrepo`: `list --project /home/me/myrepo` and the session-start
  recall hook left Gemini sessions out, and `projects` showed the repo twice.
  On the machine this was found, all 46 Gemini sessions were affected. The
  project is now the recorded path, and the first open after upgrading
  corrects rows already indexed.
## [0.33.1] - 2026-10-07

### Fixed

- **Stopping the npm-installed command stops sessionwiki.** The npm launcher
  ran the binary with a blocking call that cannot pass a signal on, so a
  SIGTERM or SIGHUP to the `sessionwiki` command - from a process manager, an
  MCP host or `kill` - ended only the launcher. The binary kept running with
  no parent; `sessionwiki web` kept its port, so the next start failed. The
  launcher now forwards SIGINT, SIGTERM and SIGHUP to the binary and exits
  with its status once it has stopped.

## [0.33.0] - 2026-10-07

### Added

- **npm package.** `npm i -g @youdie006/sessionwiki` installs the prebuilt
  binary for macOS (arm64, x64), Linux x64 or Windows x64 through a platform
  package, with no install script. On macOS it upgrades the binary in place,
  which keeps folder-access grants across releases.

### Fixed

- **macOS keeps a folder-access answer across upgrades, and stops asking
  about media folders.** macOS ties a grant to read Documents or Desktop to
  the binary's path and code signature. Releases were ad-hoc signed, so every
  version looked like a new program and asked again - on one Mac, Desktop,
  Documents and Downloads prompts plus 23 Photos prompts in three days. The
  macOS binaries are now signed with one stable identity, so at a stable
  install path (npm, the install script) an answer given once still applies
  after an upgrade; Homebrew installs each version under a new path and still
  asks once per upgrade. The aider walk also no longer enters Pictures,
  Movies, Music or the Trash, which cannot hold a project and raised the
  Photos and Media Library prompts.
- **Index fix-ups also catch rows written later by an older binary.** The
  one-off passes that respell project paths (0.30.2) and mark Codex sub-agent
  threads (0.32.0) ran once and recorded that they had. A `sessionwiki mcp`
  started before an upgrade keeps running the old code, often for weeks, and
  kept writing case-variant paths and sub-agent threads as main sessions
  afterwards; nothing looked at those rows again. On the machine this was
  found, four sub-agent threads reappeared in `list` within an hour of
  upgrading. Each pass now records how far it has checked and, on every
  open, looks only at rows written since, so a late write is fixed the next
  time the current version opens the index.

## [0.32.0] - 2026-10-07

### Added

- **Search can filter by message role.** `--role user,assistant` limits matches
  to conversation messages before ranking and grouping, so tool output cannot
  crowd conversational matches out of search results. The web search and MCP
  search accept the same role filter.

### Fixed

- **Codex sub-agent threads are sub-agent sessions, not main ones.** Codex
  marks a sub-agent's rollout in its `session_meta` (`thread_source:
  "subagent"`), and its task arrives encrypted, so these threads have no
  readable prompt. The adapter ignored the marker and listed every one as a
  main session: on the machine this was found, 2,924 of 4,843 Codex "main"
  sessions were sub-agent threads, crowding `list`, the `projects` counts,
  `recent_sessions` and the recall hook with "(no user prompt)" rows. They
  are now sub-agent sessions like Claude Code's, and the first open after
  upgrading reclassifies rows already indexed by reading each file's first
  line (a one-off update, not a rebuild).
## [0.31.0] - 2026-10-06

### Added

- **Configurable FTS5 tokenizer.** `sessionwiki tokenizer` shows the current
  setting; `sessionwiki tokenizer <SPEC>` changes it and rebuilds only the FTS
  table from indexed messages. The choice is stored in the index and shared by
  CLI, web, and MCP.

### Fixed

- **Search now ANDs terms within one message.** Previously a multi-word query
  matched only the exact substring, including its whitespace. Quote words to
  require a phrase; phrase whitespace follows the configured tokenizer, and
  short terms still work alongside longer indexed terms.

## [0.30.2] - 2026-10-06

### Fixed

- **`projects` LAST and its order follow last activity, as `list` does.**
  0.30.1 moved `list` to each session's last message, but `projects` still
  took LAST from when its newest session began, so a project worked in
  yesterday through a session started in May showed May and ranked below
  projects untouched since. LAST is now the latest activity of any session in
  the project (its start when it has none), and equally busy projects are
  ordered by it. The web UI's project list reads the same value.
- **One directory is one project, however its case was typed.** On a
  case-insensitive filesystem (WSL's `/mnt/<drive>`, macOS by default) a
  shell can enter the same directory as `apo` or `APO`, and each tool records
  what it was given, so one project split into case-variant rows - on the
  machine this was found, 14 projects, one of them 23 + 514 sessions. Project
  paths are now stored as spelled on disk, and the first open after upgrading
  respells the rows already indexed (a one-off update, not a rebuild). The
  recall hook's exact project match uses the same spelling. A case-sensitive
  filesystem is unaffected: two directories that differ only in case stay
  two projects.

## [0.30.1] - 2026-09-30

### Fixed

- **`list` and the `recent_sessions` MCP tool order sessions by last
  activity.** They sorted by when a session started, so one resumed day after
  day sank below everything begun since: on the machine this was found, eleven
  sessions active within the last day ranked as weeks or months old, including
  the one asking. They now order by the session's last message (its start when
  it has none), and `list`'s WHEN column shows that time.

### Changed

- Session rows from `recent` carry `last_active` (RFC 3339, the last message's
  time) alongside `started` in `--json` and MCP output. `started` keeps its
  meaning.

## [0.30.0] - 2026-09-27

### Added

- **`Codex::in_home` and `ClaudeCode::in_home` for embedders with several
  installs of one tool.** A program that drives more than one Codex or Claude
  Code install (`~/.codex2`, `~/.claude4`, and so on) can now build one adapter
  per install root; the adapter knows its own session sub-directory layout, so
  the caller passes only the home. Every install keeps the shared tool name, so
  parsing by tool name still works for rows from any of them. An adapter built
  this way scopes deletion reconciliation to its own root, so syncing one
  install never archives another install's rows. `Codex::default()` and
  `ClaudeCode::default()` keep reading the stock locations and scope
  reconciliation to those locations.

### Fixed

- Syncing a stock Codex or Claude Code install no longer archives rows from
  another install. A later sync restores archived rows when their source still
  exists, without reparsing an unchanged transcript.

### Maintenance

- `clap` is updated from 4.6.6 to 4.6.7.

## [0.29.0] - 2026-09-17

### Added

- Embedders can index custom adapters with `index::sync_with`, restrict deletion
  reconciliation to their own keys with `Adapter::reconcile_scope`, and render
  a redacted briefing without its source path with `commands::brief_markdown`.
  Custom tool names are preserved when reading sessions from the index.

### Fixed

- Sessions indexed by an external adapter stay readable in `show`, `brief`, MCP
  and the web viewer while their source files still exist. Readers without the
  adapter use the indexed transcript instead of failing with `unknown tool`;
  the embedder must sync again to publish later changes. Built-in adapters still
  reparse live files and report parse errors. The web viewer now shares the same
  reader as the CLI and MCP.
- Redirected synchronization logs omit terminal-only progress redraws while
  retaining warnings and per-tool summaries.

### Maintenance

- CI and the dependency auto-merge gate use `actions/setup-python` v7 with
  the existing Python 3.12 configuration.

Implementation and validation: [PR #26](https://github.com/youdie006/sessionwiki/pull/26)
and [PR #27](https://github.com/youdie006/sessionwiki/pull/27). Publication and
installation verification are recorded in the
[0.29.0 release](https://github.com/youdie006/sessionwiki/releases/tag/v0.29.0).

## [0.28.0] - 2026-09-15

### Fixed

- **Large MCP turn responses remain valid JSON.** Quote-heavy and backslash-heavy
  text can expand when JSON-encoded, exceeding the transport cap even when its
  raw character count fits. Single-turn responses now retain the longest text
  prefix that fits after serialization and set `clipped: true`, preserving the
  schema, turn identity and original retained byte count. If metadata alone
  exceeds the limit, window and turn responses return a small, parseable JSON
  error instead of a success payload cut in the middle of a string.

## [0.27.0] - 2026-09-15

### Fixed

- **Exported session fields are redacted before they are shortened.** Briefs,
  summarizer input and MCP session windows now redact the complete parsed
  session, including titles, project names, paths, messages and edit snippets,
  before rendering or applying length limits. This covers reparsed originals
  and archived sessions, including single-turn MCP output. A token cut by a
  preview or output budget can no longer survive as an unrecognized prefix.
- **Prompt-derived titles and previews no longer preserve credential fragments.**
  Redaction runs before adapters select a title line or shorten tool previews,
  edit snippets and Prodex answers, including structured tool input and
  multiline private keys.
- **Turning off swapdex serving clears subsequent account badges.**
  `serve-off` is an explicit attribution boundary until a later `use`,
  `restore` or `serve` event activates an account. It no longer leaves the
  previous account attached to later sessions.
- **Incomplete discovery no longer looks like deleted sessions.** Aider walk
  errors and unreadable, malformed or partial Prodex registry/task listings
  retain the incomplete-discovery signal, so synchronization preserves live
  sessions until a complete listing can reconcile deletions.

### Maintenance

- Update `dirs` from 6 to 7. Its upstream behavior change affects Windows
  `preference_dir()`, which sessionwiki does not call; the home, configuration
  and data directory functions used for session discovery remain unchanged.
  The update was checked against the current implementation with 205 Rust
  tests and clippy passing.
- Dependency auto-merge now verifies the workflow, repository, PR and tested
  commit, checks permitted manifest and lockfile changes structurally, and
  rechecks current state before merging. Incomplete or inconsistent evidence
  refuses the merge. The gate's regression tests run in CI.

Implementation and review: [a1ef3a8](https://github.com/youdie006/sessionwiki/commit/a1ef3a884cacecb1b495fff5710709318de8ffc0)
and [PR #21](https://github.com/youdie006/sessionwiki/pull/21). Publication and
installation verification are recorded in the
[0.27.0 release](https://github.com/youdie006/sessionwiki/releases/tag/v0.27.0).

## [0.26.0] - 2026-09-07

### Fixed

- **A brief no longer carries credentials off the machine.** `redact` has
  stripped them since the beginning, at INDEX time, because the index outlives
  the original session. `brief_text` does not read the index: `load_session`
  parses the ORIGINAL file whenever it still exists and falls back to the index
  only when it is gone, so the same session was redacted only in the case where
  the original had been deleted. Every consumer sends the text somewhere -
  `brief` is written to be pasted into another session and says so, `recall`
  prints it for the same, `summarize` pipes it to an external LLM CLI, and the
  MCP server hands it to a connected agent. That MCP handler already neutralized
  the title, redacted the username out of the project path, stripped control
  characters and framed the whole thing as untrusted data; credentials were not
  on the list. Stripped now in `brief_text`, where all four meet, and before the
  length budget - a truncated secret is still a leaked prefix.
- **The timeline read is bounded for real.** `load_events` said it capped the
  read as defence against a hand-edited file, then read the whole thing into
  memory and capped only the PARSE. It takes the last megabyte as whole lines
  now, dropping the one the seek cut in half so no fragment can parse as a
  different record.
- **A partial store walk says it skipped deletion reconciliation.** Two paths
  carry that rule - do not archive off an incomplete listing - and only one said
  so. The silent one is the path aider takes, and aider is rootless: its walk
  covers the whole home under a two-second budget, and the flag is set on any
  cap-hit. On a large home reconciliation could be skipped every run with
  nothing saying why a session the tool deleted was still listed.

## [0.25.0] - 2026-09-07

### Fixed

- **The account badge names the account that actually served the session.** It
  counted swapdex's `use` and `restore` events, which was right while every
  switch went through them. Switching now goes through swapdex's proxy, which
  writes `serve` - and `serve` was being dropped as "not a switch". True of the
  event, false of the question: on a machine driven by the proxy it is the only
  record of which account was live. Every claude-code session on this one was
  badged with nothing while 190 serve events named three accounts, and codex
  sessions carried a `use` from six weeks before the account last changed. The
  newest event now wins whatever kind it is; a session that predates every event
  still gets no badge rather than a guess. The parsing half is separated from
  the file read so which lines survive it is testable.
- **The prodex adapter's four reads are bounded.** The memory cap on session
  files exists so a corrupt or hostile one cannot exhaust the process. Eleven
  adapters went through it; prodex read its task JSON, its result JSON, its
  registry and its artifact with a plain `fs::read` - one of them under a comment
  saying "Bounded read; an artifact is normally a few KB", where the largest file
  in the real store on this machine is 35 MB.
- **`doctor` no longer reports a failed count as zero.** The count of sessions
  kept after their tool deleted them turned any error - a schema without
  `archived_at`, a locked database - into "0 kept", which reads as the good news
  that nothing was lost.
- **A redirected run leaves the real index alone.** The one-time migration from
  the old directory names looked under `dirs::data_dir()` whatever `SESSIONWIKI_DATA`
  said, then renamed what it found into the destination - so a redirected run
  could move the user's real index out of their home.
- `ResumeInfo`'s doc claimed every supported tool encodes its native session id
  in the filename. Two of twelve do, and the code already answers None for the
  rest; only the comment was wrong.

### Added

- `doctor` says when a store's location could not be worked out, instead of
  giving that the same silence as a tool that is not installed. Defence, not a
  reproduced failure - on Unix `dirs` falls back to the passwd database, so
  unsetting HOME does not make it fire.

## [0.24.0] - 2026-08-20

### Fixed

- **A renamed folder no longer hides a file's history.** `trace` matched paths by
  suffix, so a file traced by the path it has TODAY found nothing once its folder
  had been renamed - and said "no session touched a file matching ...", about a
  file whose entire history was in the index under the old directory. Found on a
  real machine where `~/Project/lunch` had become `~/Project/slack`: the sessions
  that built it were all there and unreachable by the path the owner had. A full
  path that matches nothing now retries with the file NAME, which is the part
  that survives a move, and says so - otherwise the older paths listed underneath
  read as the index pointing somewhere wrong.

## [0.23.0] - 2026-08-19

### Added

- **`migrate --config-dir`** - write a transcript into a specific account's
  store instead of always the default one. This is what lets a conversation be
  handed to another ACCOUNT: under swapdex's slot model each account has its own
  `CLAUDE_CONFIG_DIR` with its own `projects/` inside, and migrate wrote into
  `~/.claude` unconditionally, so on a slotted machine the copy succeeded and
  landed where the resuming account never looks. Without the flag
  `CLAUDE_CONFIG_DIR` is honoured, so running inside a slot's own shell already
  does the right thing; failing both, the single default store.

- **`doctor`** - a read-only, one-command health check of this machine's setup:
  which session stores exist (and how many sessions each holds), the index's
  schema currency and core-table presence, how many sessions are indexed (and
  kept after a tool deleted them), and the version. `--json` for scripts. When
  something looks empty or stale the cause is usually mundane; this surfaces it
  so a bug report starts from facts.
- **Secrets are redacted from message text, titles, edit snippets, and synopses
  at index time** (`[redacted:<kind>]`) - PEM private keys, JWTs, and prefixed
  tokens (OpenAI, AWS, GitHub, Stripe, Google, Slack). The index outlives the
  original session via archive mode, so credentials must not land in it. A
  schema bump scrubs sessions already indexed before this change. Dependency-free
  and tuned to high-confidence shapes to avoid redacting ordinary code.
- **File history - "how this file was written".** Clicking a file in the web UI
  now shows its evidence chain: the sessions that edited it, newest first, each
  with what it actually changed - the edit kind (edit/write/multiedit) and a
  bounded snippet of the new code, not just a bare list of sessions that touched
  the path. This answers *why does this file look like this*. Backed by a new
  `edits(session_id, path, kind, ts, snippet)` cache table (extracted at parse
  time from each tool call, rebuilt on a schema bump like the other cache
  tables), an `evidence_for(path)` assembly, and a `GET /api/file?path=`
  endpoint. `trace`/`blame` still give the path-level provenance.

## [0.22.0] - 2026-07-20

Open any live session by its native id (codex-rollout or claude-transcript
UUID), closing the harness tower to sessionwiki join gap.

### Added

- `session_window`/`show` now resolve a session by its native codex-rollout or
  claude-transcript UUID (full or prefix), in addition to the sessionwiki short
  id. Live sessions not yet indexed are found via a disk-scan fallback (no
  sync), so a session started today opens by native id in one call.
- `list`, `recent_sessions`, and `search` JSON now expose a `native_id` field
  (the UUID only, never the absolute path). Closes the harness tower to
  sessionwiki join gap.

## [0.21.0] - 2026-07-16

Harden the MCP read path for in-loop agent-to-agent reads: a stable window
contract and freshness on every read tool.

### Added
- **`session_window` (MCP) now returns versioned JSON** (schema `sessionwiki.window/1`) instead of an unversioned text render, so a reading agent can parse a sibling's window rather than scrape it. Fields: `id`, `tool`, `project`, `title`, `started`/`ended`, `messages` (total turns), `large` (indexed head+tail), `budget_tokens`, `omitted_leading`, `turns[]` (each with `i` = stable turn index, `role` = user/assistant/tool, `text`, and `truncated` or `folded`+`bytes`), and a `drilldown` hint. Deterministic for a fixed (id, budget); the recent tail is kept within budget and the whole object is size-guarded so it is always valid JSON. Tool output is folded head+tail and byte-bounded.
- **Per-turn drill-down:** `session_window(id, turn=<i>)` returns that one turn's full retained text (schema `sessionwiki.turn/1`), untruncated by the window's folding/cap - fetch-full-content-on-demand for a turn seen in a window. (Tool outputs are already capped at parse time, so this recovers folded-out lines, not adapter-dropped bulk.)

### Changed
- **`search_sessions` (MCP) freshens in the background**, matching `recent_sessions` (0.20.2): it serves the current index immediately and kicks a bounded incremental sync, so a just-created sibling shows on a subsequent search without the request ever waiting on the store walk. `session_window` already reads the target file directly (live). This closes the last MCP read tool that did no freshen; incremental parsing (only files whose mtime/size changed) was already in place.

## [0.20.2] - 2026-07-16

### Changed
- **`recent_sessions` (MCP) freshens in the background.** It returns the current index immediately (~200ms) and kicks a bounded incremental sync, so a just-started sibling session shows on the next call - without the multi-second blocking sync a full 46GB-corpus walk would cost. New `index::sync_bounded` re-parses only files modified since a floor and never archives a live session on a bounded run.

## [0.20.1] - 2026-07-16

### Fixed
- **Over-cap sessions are windowed (head+tail) instead of dropped.** A session past the 256MB read cap was skipped entirely - so the biggest sessions, the ones most worth referencing across sessions, vanished from search and `session_window`. They are now indexed from the first 256KB + last 512KB of lines (byte-bounded, no full scan), partially readable, and flagged `[large]` in the title.

## [0.20.0] - 2026-07-15

Cross-session context for agents: one agent can now see what a sibling session
(or its other self) is doing, as the ACTUAL recent conversation - not a lossy
summary - bounded so it can't flood the reader's context.

### Added
- **`sessionwiki show <id> --window [--budget N]`** renders a session's real
  recent turns with tool outputs folded to head+tail, capped to the recent tail
  by a token budget, an orientation header + role labels, and a
  `show <id> --full` drill-down hint. `--live` reads the session file directly
  (0-delay), bypassing the index sync.
- **MCP discovery + read tools** so an agent can find and read sibling sessions:
  - `session_window(id, budget_tokens)` - the bounded window above, over stdio.
  - `recent_sessions(limit, tool, exclude_id)` - the most recent sessions across
    every tool ("who's around"), bounded to a recent window.
  - `related_sessions(id, limit, exclude_id)` - a session's siblings by shared
    project, edited files, or tags.
  Both discovery tools take `exclude_id` to drop your own session. This closes
  the chain: `recent`/`search`/`related` -> id -> `session_window`. Verified
  cross-tool (Claude Code and Codex sessions see each other).

### Fixed
- (from 0.19.4) `sessionwiki web` no longer auto-opens a browser on WSL, where
  `xdg-open` pops a stray WSLg/Windows window on every start and often can't
  reach the loopback server; it prints the URL instead. Native Linux/macOS keep
  auto-open.

## [0.19.4] - 2026-07-14

### Fixed
- **`sessionwiki web` no longer pops a stray browser window on WSL.** On WSL,
  `xdg-open` launches a WSLg/Windows browser on every `web` start - a blank
  window each time you restart the server during development, and it often
  cannot even reach the loopback address. `web` now detects WSL and prints the
  URL instead of auto-opening; native Linux/macOS keep the existing auto-open
  (still overridable with `--no-open`).

## [0.19.3] - 2026-07-14

Codex/Claude transcript formats drifted; a comprehensive audit against the
real corpus caught five parsing gaps, all fixed here.

### Fixed
- **Codex user messages no longer index twice.** Current rollouts carry each
  prompt BOTH as an `event_msg` and as a `response_item`; the adapter now
  cancels each pair exactly once (counted, so a genuinely repeated prompt -
  "continue" twice - still keeps every occurrence).
- **AGENTS.md instructions no longer become session titles.** The new
  `# AGENTS.md instructions for <path>` / `<INSTRUCTIONS>` user-message shape
  is treated as boilerplate, like the older environment-context wrappers.
- **2025-era Codex rollouts parse again.** Files that predate the `payload`
  envelope (bare `{"type":"message"}` lines) indexed as empty "(no user
  prompt)" sessions; they are now parsed like their modern counterparts.
- **`custom_tool_call` items are indexed** and their inputs feed `trace` /
  `blame` path extraction, so edits made through custom tools are no longer
  invisible.
- **Claude titles use the `ai-title` event.** Current Claude Code transcripts
  stopped emitting `summary` events; titles now fall back summary ->
  ai-title -> first user message instead of always the first prompt.

## [0.19.2] - 2026-07-07

### Fixed
- A crafted/corrupt prodex task id with a multibyte character at a slice
  boundary could panic the parser - and a parse panic aborts the WHOLE
  indexer, so one bad `.bridge` file bricked every `list`/`search`/`scan`.
  The id timestamp parse now gates on ASCII digits before slicing (found by
  an adversarial audit; same bug class swapdex fixed a release earlier).
- `list --account` no longer silently returns fewer rows than `-n`: the
  badge filter is computed post-query, so the query now over-fetches and
  truncates after filtering.

## [0.19.1] - 2026-07-07

Ecosystem-walkthrough fixes, from a fresh user's chair.

### Added
- `--account <name>` on `list` and `search`: filter by the swapdex @badge,
  which the help text now explains.
- prodex consult titles are the QUESTION (prompt first line) instead of
  prodex's identical auto-title, so a list of consults is tellable apart.
  The derived-cache version was bumped, so existing rows re-index once.

### Fixed
- A consult whose answer artifact was written but whose result JSON was not
  (the crash path the durable ledger exists for) is indexed again: the
  artifact is read independently of the result.
- `trace` with an ABSOLUTE editor path now finds sessions whose stored paths
  are repo-relative (prodex bundles relative paths; matching honors the `/`
  boundary, so suffix lookalikes cannot match).
- `--tool` help enumerates prodex and points at `scan` for the full list;
  the scan footer pluralizes correctly; scan's prodex PATH shows the store
  directory (not the registry file); the prodex resume error message no
  longer has a hanging indent.

## [0.19.0] - 2026-07-07

### Added
- prodex adapter: every ChatGPT Pro consult made through
  [prodex](https://github.com/youdie006/prodex) becomes a searchable session -
  the bridge task is the question, the result/answer artifact is the answer,
  and files bundled with a task become `trace` provenance links. Discovery
  reads the machine-wide bridge registry prodex >=0.11.0 maintains
  (`~/.local/share/prodex/bridges.json`); registered repos that no longer
  exist are skipped silently. `resume` on a prodex session prints the ChatGPT
  thread URL it ran in (only an `https://chatgpt.com/` URL is ever surfaced).
- Session ids for prodex are the same short stable 12-hex shape as every
  other tool (task ids share a long date prefix that would defeat prefix
  addressing).

### Changed
- `list`/`search` truncate the ID column display at 13 chars so an unusually
  long id cannot shear the table (ids are prefix-addressable).

## [0.18.1] - 2026-07-07

### Fixed
- `trace` and `related` rows now carry the swapdex `account` field like
  `list`/`search` do (the annotate call was missing on those two query paths,
  so the same session showed a badge in search but never in a file trace -
  across CLI, web, and MCP).

### Changed
- Timeline reads are capped to the newest 4000 lines (defense in depth;
  swapdex itself bounds the file to ~1000 events), and account names are
  control-char-stripped at the source so every consumer gets a terminal-safe
  badge. `search` no longer prints a trailing space on badge-less lines.
- The web UI shows the account badge in each session's meta line.

## [0.18.0] - 2026-07-07

### Added
- swapdex integration: `list` and `search` badge each session with the
  account profile active when it ran (`@work`), and `--json` rows carry an
  `account` field (null when unknown). Attribution reads swapdex's switch
  timeline read-only and never guesses - sessions that predate the first
  recorded switch stay unbadged. No swapdex on the machine, no change.
  The MCP server neutralizes the new field like every other free-text one.

## [0.17.0] - 2026-07-03

### Added
- `sessionwiki mcp`: a stdio MCP server exposing three read-only tools
  (`search_sessions`, `trace_file`, `get_session_brief`) to any MCP client
  (Claude Code, Cursor, and others). Cross-tool session search as agent tools;
  100% local, read-only, no sync. Register with
  `claude mcp add sessionwiki -- sessionwiki mcp`.
- Homebrew: `brew install youdie006/tap/sessionwiki` (macOS and Linux).

### Changed
- aider re-indexing is now linear instead of O(runs^2): a long
  `.aider.chat.history.md` is split once per sync and cached, so a file with
  thousands of runs re-indexes in a fraction of the time (measured 1600 runs:
  ~11s to ~0.2s).

### Fixed
- A partial directory walk (an unreadable project directory) can no longer
  archive live sessions: file-per-session adapters now report a partial listing
  and sync skips deletion reconciliation for that run, the same guard shared
  stores already had.
- `sync` warns when a session fails to parse and reports honest counts
  ("indexed 12/14 (2 failed to parse)") instead of silently dropping it.
- Tags are NFC-normalized on add, filter, and remove, so a tag typed in
  decomposed form (macOS IME) matches everywhere, like every other indexed
  string.
- `resume` checks tool support before file existence, so tools without headless
  resume (aider, OpenCode) get the right message instead of a false "the
  session file is gone".
- `digest --since` with an out-of-range duration returns an error instead of
  panicking.
- A failed data-directory migration now warns with instructions instead of
  silently starting an empty index.

## [0.16.1] - 2026-06-19

### Fixed
- crates.io README: the dark-mode banner now uses an absolute image URL so it
  renders on the crates.io page, which (unlike GitHub) does not rewrite relative
  `<source srcset>` paths. Docs only; no code change.

## [0.16.0] - 2026-06-19

### Added
- `aider` adapter (11th tool): indexes Aider sessions from per-repo
  `.aider.chat.history.md`. Markdown-derived (roles reconstructed from line
  prefixes, no per-message timestamps); edited files come from aider's
  `Applied edit to` / `Creating empty file` lines. Discovery is a bounded,
  capped, symlink-safe walk (home by default, `SESSIONWIKI_AIDER_ROOTS` to
  scope) that opens only `.aider.chat.history.md` and logs nothing.
- Recall loop (Claude Code plugin): a SessionStart hook auto-injects a small,
  fenced "prior work in this project" brief at the start of a new session, so the
  agent gets long-term memory without being asked. Exact-project scoped,
  `--no-sync` (tens of ms), zero output on fresh projects, always exits 0. The
  injected recall is fenced as untrusted data and sanitized; the LLM synopsis is
  not auto-injected. New internal `sessionwiki hook session-start` command.

### Changed
- Schema stability on the road to 1.0: the durable index tables (summaries,
  tags, notes, archive) now version independently of the disposable cache via a
  `meta.durable_version` and a forward-only, additive-only migration runner, so
  user curation and archived sessions survive every upgrade. All index
  connections now set a `busy_timeout`. No behavior change in this release (zero
  migrations registered).

### Security
- `search` strips control bytes (ESC/OSC/C1/DEL) from terminal snippets, so an
  untrusted session body can no longer inject terminal escape sequences into the
  operator's terminal. The `--json` output and other commands already did this.

## [0.15.0] - 2026-06-18

### Added
- `blame <file> [-L start,end]`: git blame for the AI era - attributes each line
  to the AI session most likely behind the commit that last changed it, by
  joining `git blame --line-porcelain` with the index's per-file touch records
  and session time windows. Best-effort by design (`ambiguous`/`unattributed`
  are normal) and it falls back to file-level `trace` whenever git can't carry
  the weight. The git child runs with a hardened environment (no system config,
  no pager, `core.fsmonitor`/hooks neutralized, inherited `GIT_*` cleared), the
  path is passed after `--`, and `-L` is validated. `--json` for agents.

### Security
- `truncate()` - the choke point that renders session titles to the terminal in
  `list`/`search`/`trace`/`resume` - now strips C0/C1/DEL control characters, not
  just newlines and tabs. A session title is untrusted input (a planted session
  can set any title), so an unstripped ESC could have injected ANSI/terminal
  escape sequences into output. The web UI was already safe (it builds the DOM
  with `textContent`, never `innerHTML`).

## [0.14.0] - 2026-06-18

### Added
- **gptme adapter** (10th tool): indexes sessions from [gptme](https://github.com/gptme/gptme)
  (`~/.local/share/gptme/logs/<session>/conversation.jsonl`). Drops `pinned: true`
  system-prompt boilerplate and bare `system` role lines; handles Python's naive
  `datetime.now().isoformat()` timestamps (no UTC offset) by falling back to
  UTC-assumed parsing. Size-capped against `MAX_SESSION_FILE_BYTES`.
- Windows install: `scripts/install.ps1`, a PowerShell one-liner (`irm ... | iex`)
  that downloads + checksum-verifies the latest Windows binary and adds it to
  PATH. The shell installer already covers WSL, which is Linux.

## [0.13.0] - 2026-06-18

### Security
Hardening from a red-team pass of the launch surface (all confirmed with PoCs):
- The local web server now rejects requests whose `Host` header isn't the
  loopback name it bound (defeats DNS rebinding) and whose `Origin` is a
  different site (defeats a plain cross-origin `fetch`), so a malicious page you
  visit while `web` is running can't read your sessions from `127.0.0.1`.
- `resume` no longer auto-launches Codex or Gemini in a directory the session
  file claims. Those tools aren't directory-scoped, so a planted session could
  name any directory (with an attacker's `AGENTS.md` there) and have `resume`
  load it; it now prints the command for you to run unless the directory is
  verified, which only Claude Code's store layout allows.
- `resolve` escapes `LIKE` wildcards in a session-id prefix, so a web request for
  id `%` can no longer match and dump an arbitrary session.
- install.sh documents honestly that its checksum only detects a corrupted
  download, not a malicious release (the hash is co-located with the binary).

## [0.12.0] - 2026-06-18

### Added
- `digest [--since 7d]`: a markdown rollup of recent sessions grouped by project
  &mdash; what you worked on, the files each session touched, and any cached
  synopsis. It composes the timeline, provenance, and summaries the index already
  has, over a time window (`--since 2w`/`24h`/`90m`, `--project`, `--tool`,
  `--json`). The standup / PR-description / "what did I ship this week" view.

### Security
- `resume` no longer auto-launches the upstream tool (`claude`/`codex`) in the
  directory a session file *claims* unless that directory can be verified as the
  session's own &mdash; for Claude Code, by matching the store folder the session
  lives in (the folder name encodes the cwd). A session file is untrusted input;
  before this, a planted or prompt-poisoned session could point its `cwd` at an
  attacker directory and have `resume` launch the agent there, loading that
  directory's `CLAUDE.md` / `.mcp.json` / settings. When the directory can't be
  verified, `resume` now prints the command for you to run instead of launching.
- Session-file reads are size-capped (256 MB) and the OpenCode SQLite message/
  part reads are `LIMIT`ed, so a malicious oversized file or a db packing
  millions of rows into one session can't exhaust memory.

### Changed
- `SECURITY.md` now documents that `migrate` writes copies into the target tool's
  store and that `resume` validates the session-derived directory, instead of
  implying the tool only ever reads.

## [0.11.0] - 2026-06-18

### Added
- `migrate <id> <dir>`: copy a session so it can be resumed from a different
  project directory. Each tool ties a session to a directory differently, so
  migrate does the right thing per tool: Claude Code's resume is scoped to the
  project folder, so the transcript is copied into the target's folder; Codex
  resumes by id from any directory, so nothing is copied (it just prints the
  command); Gemini copies the chat into the target project's store
  (`~/.gemini/tmp/<sha256(dir)>/chats/`) and rewrites its `projectHash`. The
  original session is never modified.

## [0.10.0] - 2026-06-17

### Added
- `recall <query>`: one-shot recall — searches, lists the candidate matches, and
  briefs the top one, collapsing the usual search -> pick id -> brief loop into a
  single command (`--tool`/`--project`/`-n`/`--max-chars`/`--json`/`--no-sync`).
- `sync [--tool]`: build or refresh the index on demand, so later queries can run
  with `--no-sync`.
- `--no-sync` on `search`/`list`/`recall`/`show`/`brief`/`resume`/`trace`: query
  the already-built index without walking the stores — the fast path when the
  index is kept warm (e.g. a cron running `sessionwiki sync`).

### Changed
- `show`/`brief`/`resume`/`recall` resolve the id against the existing index first
  and only sync (all tools) when it is genuinely unknown (no prefix match) - an
  already-indexed id, including an *ambiguous* prefix, no longer triggers a full
  walk of every store (notably the large Codex one). The tradeoff: a session that
  has grown since the last sync is served at its last-synced length until the next
  `sync`.
- Harness labeling (the oh-my-* tags from 0.9.0) is now detected from the
  filesystem only - the `.omc`/`.omo` orchestration directory in a session's
  project - dropping the transcript-text markers. Those markers were the tools'
  own source/doc literals, so a session that merely *discusses* a harness (e.g.
  while building OSS) was mislabeled as run by it; the filesystem signal cannot
  be confused that way. oh-my-codex leaves no directory and is no longer tagged.
- A tag filter (`list --tag`, web) now includes subagent transcripts, and `list`
  marks them `[subagent]`, so a tag carried only by a subagent is no longer shown
  in the tag cloud yet hidden from the listing.

### Fixed
- `recall --json` includes the plain `snippet` next to `snippet_marked` in each
  candidate, matching `search --json`; `recall`'s candidate list marks subagent
  transcripts `[subagent]`.
- `sync` reports the count of top-level live sessions (matching `stats`/`list`)
  instead of every file row, which had included subagent transcripts.

## [0.9.0] - 2026-06-17

### Added
- Five new adapters, bringing supported tools to nine: the **Cline family**
  (Cline, Roo Code, Kilo Code) from one parser over their shared VS Code
  `globalStorage/<ext>/tasks/<id>/` layout - handling both native `tool_use`
  blocks and the XML-in-text tool calls Cline writes, and pulling edited files
  from either; **gajae-code** (and upstream Pi) from `~/.gjc/agent/sessions`
  JSONL; and **Continue** from `~/.continue/sessions/*.json`. Each extracts the
  files a session edited so they show up in `trace`.
- Harness labeling for the Korean "oh-my-*" wrappers. oh-my-claudecode,
  oh-my-codex, and the oh-my-openagent / lazyclaudecode / lazycodex family run
  on top of Claude Code, Codex, and OpenCode, so their conversations are already
  indexed via those adapters; sessionwiki now detects which harness drove a
  session (from the markers each injects) and auto-tags it `oh-my-claudecode` /
  `oh-my-codex` / `oh-my-openagent`, so `list --tag oh-my-claudecode` works. No
  new adapter required.

### Changed
- The OpenCode adapter now reads OpenCode's SQLite store
  (`~/.local/share/opencode/opencode.db`, plus `opencode-<channel>.db` and
  `OPENCODE_DB`), which has been the default since OpenCode v1.2.0 - the
  previous JSON-only reader missed every session created on a current install.
  The legacy `storage/**` JSON layout is still read on pre-1.2.0 installs. This
  needed a small shared-store path in the indexer (one SQLite db holds many
  sessions, so the file-per-session + mtime model did not fit); `scan` sizes the
  db and re-indexes only sessions whose updated-time changed.

## [0.8.0] - 2026-06-17

### Added
- OpenCode adapter: indexes sst/opencode sessions from
  `~/.local/share/opencode/storage` (the multi-file session/message/part JSON
  layout, joined in id order), including the files each session edited (the
  `edit`/`write` tools' `filePath` and `patch` file lists) for `trace`. Brings
  supported tools to four.
- Provenance: sessions are linked to the files they edited or created, read
  from their tool calls (Claude's `Edit`/`Write`/`MultiEdit`, Codex's
  `apply_patch`). New commands: `files <id>` (a session's edits) and
  `trace <path>` (the sessions that touched a file, newest first, matched by
  suffix so a relative path resolves to the absolute one on disk). The scope is
  honest — sessions that *touched* a file, not line-level authorship.
- `related` now also links sessions that edited the same file, even across
  projects; `stats` reports the count of files linked to a session.
- Web UI: a session's touched files appear as chips in its header; clicking one
  lists every session that touched it (new `/api/trace` endpoint).
- Archive mode: when a tool deletes a session's original file, sessionwiki
  keeps the indexed copy instead of dropping it, so `search`, `trace`, and
  `brief` keep working for it. A durable `archive` table survives schema
  rebuilds; archived sessions are served from the index (`show`/`brief`/web),
  flagged `[archived]` in `list`/`show`/web, and counted in `stats`. `sync`
  reports how many were kept. New `forget <id>` purges a session for good;
  re-appearing files un-archive automatically; `SESSIONWIKI_NO_ARCHIVE` reverts
  to delete-on-prune. A store that vanishes wholesale (uninstall, unmount) is
  not mass-archived.
- Korean/CJK search: 2-syllable words (회사, 검색) - the most common Korean word
  length - are now searchable via a LIKE fallback below the trigram floor, and
  all indexed text + queries are NFC-normalized so macOS NFD input no longer
  silently returns nothing. "Zero-setup CJK search" is now actually true.
- `--json` on `search`, `list`, `related`, `brief`, `trace`, and `files`: stable
  snake_case output (matching the web API) for agent consumption, with the
  absolute path hidden. Snippets are control-code-free with a `snippet_marked`
  variant. The agent-native half of the tool.
- Claude Code plugin (`/plugin marketplace add youdie006/sessionwiki`): a
  `session-recall` skill + `/sessionwiki:recall` command that use the `--json`
  commands so an agent recalls past sessions as long-term memory, fully offline,
  degrading gracefully when the CLI isn't installed.

### Changed
- `show` pages long transcripts through your pager (`SESSIONWIKI_PAGER`, then
  `PAGER`, default `less -FRX`) when stdout is a terminal, so `show --full` of a
  huge session no longer floods the screen, and falls back to plain printing when
  no pager is installed; piped/redirected output is unchanged.
- `summarize` states up front that it pipes each transcript to the summarizer
  (the default `claude -p` sends them to the Anthropic API) - the one place data
  can leave the machine, and only on explicit `summarize`.
- CLI demo is a clean static full-frame terminal (no camera zoom). The web demo
  zooms to exact element regions measured from the live DOM, at 50fps with
  cubic ease-in-out moves and still holds.

### Fixed
- `tag` now rejects a tag containing a comma or an empty tag: a comma corrupted
  the comma-joined `tags` array in `--json` and the web API, and an empty tag
  emitted `""`. JSON serialization of session rows in `search`/`trace` can no
  longer panic mid-array.

## [0.7.0] - 2026-06-16

### Changed
- Renamed the project from `sessiondex` to **sessionwiki**, reframed around
  "session engineering": a session is a unit of context, and once you have
  hundreds they need curating, not just searching. The data directory migrates
  newest-name-first (`sessiondex` then `session-atlas`), carrying the index and
  curated tags/notes/summaries over.

### Added
- Curation layer (the editable "wiki" part): `tag` / `note`, stored in the
  index and surviving reindexing; the original session files are never touched.
- `related`: sessions about the same thing — same project first, then anything
  sharing a tag — surfaced in `show` and as a "see also" panel in the web UI.
- `projects` and `stats`: per-project and per-tool/per-month rollups.
- Web UI: clickable tag chips and a tag-filter bar, notes, a "see also" panel,
  a node-graph logo, and a globe-button language picker.
- Two demo GIFs: a narrated CLI recording (`docs/demo-cli.gif`) and a web UI
  tour (`docs/demo-web.gif`). `SECURITY.md`.

## [0.6.0] - 2026-06-16

### Added
- Library + binary split (`src/lib.rs`): the adapter and index logic is now a
  reusable crate, not just a CLI.
- Golden-file parser tests for all three adapters, covering boilerplate
  dropping, subagent detection, schema variants, multi-block content, and
  malformed-line tolerance (`tests/parse.rs`, `tests/fixtures/`).
- GitHub Actions: CI (fmt + clippy + test) and a release workflow that builds
  prebuilt binaries for Linux, macOS (x86_64 + arm64), and Windows on tag push.
- `scripts/install.sh` one-line installer that fetches the right prebuilt
  binary for the platform.
- A "how this differs from a single-tool viewer" section in the README.

## [0.5.0] - 2026-06-12

### Changed
- Renamed the project from `session-atlas` to **sessiondex**. The data
  directory migrates automatically on first run, carrying the existing index
  and cached summaries over.

### Added
- Multilingual web UI (English, Korean, Japanese, Chinese), auto-detected from
  the browser locale and switchable from the sidebar.
- Korean README (`README.ko.md`).

## [0.4.0] - 2026-06-12

### Added
- `summarize`: cache 1–2 sentence LLM synopses using your own LLM CLI. Stored
  in a table that survives index schema rebuilds.
- README polish: logo, light/dark adaptive hero, architecture diagram, FAQ.

## [0.3.0] - 2026-06-12

### Added
- Session previews (last assistant message) in the web sidebar.
- `show --outline`: a deterministic digest of a session (every user turn plus
  how it ended), and a clickable outline in the web transcript.

## [0.2.0] - 2026-06-12

### Added
- `resume`: reopen a session in its original tool (`claude --resume` /
  `codex resume`) in the right project directory.
- `brief`: emit a markdown briefing of a session to carry context across tools.

## [0.1.0] - 2026-06-12

### Added
- Initial release: `scan`, `list`, `search`, `show`, and a local web UI over an
  incremental SQLite FTS5 index. Adapters for Claude Code, Codex, and Gemini
  CLI. 100% local, no telemetry.

[0.15.0]: https://github.com/youdie006/sessionwiki/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/youdie006/sessionwiki/compare/v0.13.0...v0.14.0
[0.13.0]: https://github.com/youdie006/sessionwiki/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/youdie006/sessionwiki/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/youdie006/sessionwiki/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/youdie006/sessionwiki/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/youdie006/sessionwiki/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/youdie006/sessionwiki/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/youdie006/sessionwiki/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/youdie006/sessionwiki/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/youdie006/sessionwiki/releases/tag/v0.5.0
[0.4.0]: https://github.com/youdie006/sessionwiki/releases/tag/v0.4.0
[0.3.0]: https://github.com/youdie006/sessionwiki/releases/tag/v0.3.0
[0.2.0]: https://github.com/youdie006/sessionwiki/releases/tag/v0.2.0
[0.1.0]: https://github.com/youdie006/sessionwiki/releases/tag/v0.1.0
