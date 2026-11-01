use crate::adapters::{self, Adapter};
use crate::model::Role;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Where a legacy index would sit, given where this one is going.
///
/// Beside the destination, not under `dirs::data_dir()`. The migration used to
/// ask the ambient data dir wherever the index was actually headed, and then
/// RENAME what it found into that destination - so a run with
/// `SESSIONWIKI_DATA` pointed at a temp dir would move the real
/// `~/.local/share/sessiondex` into it, and the tags, notes and summaries in
/// there are not rebuildable. Eight test files set that variable.
fn legacy_candidates(dir: &std::path::Path) -> Vec<PathBuf> {
    let Some(parent) = dir.parent() else {
        return Vec::new();
    };
    ["sessiondex", "session-atlas"]
        .iter()
        .map(|n| parent.join(n))
        .collect()
}

/// Resolve the data directory where `index.db` lives.
/// 1. `SESSIONWIKI_DATA` environment variable override (if set).
/// 2. Local `data` directory next to the executable (portable mode, if exists).
/// 3. Platform default directory (`~/.local/share/sessionwiki` or `%APPDATA%\sessionwiki`).
fn resolve_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SESSIONWIKI_DATA") {
        return Some(PathBuf::from(dir));
    }
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let local_data = exe_dir.join("data");
            if local_data.exists() {
                return Some(local_data);
            }
        }
    }
    dirs::data_dir().map(|d| d.join("sessionwiki"))
}

/// The index lives outside the session stores and never touches them.
/// Default: ~/.local/share/sessionwiki/index.db (platform equivalent), or local `data/` if present.
pub fn db_path() -> Result<PathBuf> {
    let dir = resolve_data_dir().context("cannot determine a data directory")?;
    // One-time migration from earlier names, newest first. This carries over
    // the existing index AND the curated tags/notes/summaries, which are not
    // rebuildable. The project was session-atlas, then sessiondex.
    if !dir.exists() {
        {
            for old in legacy_candidates(&dir) {
                if old.exists() {
                    // A failed rename must not pass silently: the user would
                    // get a fresh empty index while their curation sits
                    // stranded in the old directory. Re-check after failure,
                    // though - a concurrent first run (hook + CLI) may have
                    // migrated it already, which is success, not failure.
                    if let Err(e) = std::fs::rename(&old, &dir) {
                        if !dir.exists() && old.exists() {
                            eprintln!(
                                "warning: could not migrate {} -> {} ({e}); \
                                 starting a fresh index. Move it manually to \
                                 keep your tags, notes, and archive.",
                                old.display(),
                                dir.display()
                            );
                        }
                    }
                    break;
                }
            }
        }
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("index.db"))
}

/// The index path IF it already exists - with none of the directory creation or
/// legacy migration `db_path` performs. For strictly read-only callers (`doctor`)
/// that must not mutate the filesystem just to check for the index.
pub fn existing_db_path() -> Option<PathBuf> {
    let dir = resolve_data_dir()?;
    let db = dir.join("index.db");
    db.exists().then_some(db)
}

/// `user_version` versions the disposable cache: a mismatch drops and rebuilds
/// the derived tables (files/messages/msgs/touched) instead of migrating. The
/// durable tables (summaries, tags, notes, archive) hold what cannot be
/// re-derived - LLM output, user curation, and sessions whose originals the tool
/// deleted - and are versioned separately by `meta.durable_version` via forward,
/// additive-only migrations that never drop, so they survive every upgrade. The
/// two counters are independent and must never gate each other.
pub const SCHEMA_VERSION: i64 = 9; // 9: compact tool lines + optional full outputs

const DEFAULT_FTS_TOKENIZER: &str = "trigram";
const FTS_TOKENIZER_META_KEY: &str = "fts_tokenizer";
const DEFAULT_TOOL_OUTPUT_MODE: ToolOutputMode = ToolOutputMode::Summary;
const TOOL_OUTPUT_MODE_META_KEY: &str = "tool_output_mode";

/// Whether full tool results are retained in the index and archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutputMode {
    /// Retain bounded, redacted tool output separately from searchable text.
    Full,
    /// Keep compact tool summary lines in the index and archive. Failed calls
    /// include the first output line in their summary.
    Summary,
}

impl ToolOutputMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Summary => "summary",
        }
    }
}

impl std::fmt::Display for ToolOutputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ToolOutputMode {
    type Err = &'static str;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "full" => Ok(Self::Full),
            "summary" => Ok(Self::Summary),
            _ => Err("tool output mode must be full or summary"),
        }
    }
}

/// Version of the durable schema this binary ships. The durable CREATE
/// statements are frozen at this shape; every later durable change is a
/// migration in DURABLE_MIGRATIONS. Independent of SCHEMA_VERSION (the cache).
const BASELINE_DURABLE_VERSION: i64 = 1;

/// Read meta.durable_version, seeding the baseline when absent. Absence covers
/// both a fresh DB (durables just created at baseline) and an existing
/// pre-feature index (durables already at baseline) - both correctly start at
/// BASELINE. The common path is read-only; INSERT OR IGNORE handles a concurrent
/// first-open after the initial read.
fn read_or_init_durable_version(conn: &Connection) -> Result<i64> {
    let read = || -> Result<Option<String>> {
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'durable_version'",
                [],
                |r| r.get(0),
            )
            .optional()?)
    };
    if let Some(v) = read()? {
        return Ok(v.parse().unwrap_or(BASELINE_DURABLE_VERSION));
    }
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('durable_version', ?1)",
        params![BASELINE_DURABLE_VERSION.to_string()],
    )?;
    let v = read()?.context("durable_version missing after initialization")?;
    Ok(v.parse().unwrap_or(BASELINE_DURABLE_VERSION))
}

/// One migration step: plain DDL, or Rust code for transformations SQL cannot
/// express (e.g. unicode normalization). Both run inside the same gated
/// transaction and must stay additive-and-repair-only - never drop durable data.
enum MigrationStep {
    // The natural shape for most future migrations (ALTER/CREATE); only data
    // repairs need `Fix`. Allowed while no registered migration uses it.
    #[allow(dead_code)]
    Sql(&'static str),
    Fix(fn(&Connection) -> Result<()>),
}

struct Migration {
    version: i64,
    step: MigrationStep,
}

/// Forward-only, additive-only durable migrations applied in order. ALTER ADD
/// COLUMN / CREATE / data repair only - never DROP/RENAME a durable column or
/// table.
const DURABLE_MIGRATIONS: &[Migration] = &[
    // v2: tags written by pre-0.17 binaries were lowercased but not
    // NFC-normalized, so a decomposed-form tag (macOS IME) is unreachable by
    // the normalized lookups. Re-normalize stored rows once.
    Migration {
        version: 2,
        step: MigrationStep::Fix(normalize_stored_tags),
    },
];

/// Durable migration v2: converge every stored tag on `norm_tag` form. An NFC
/// twin that already exists absorbs the row (INSERT OR IGNORE + DELETE).
fn normalize_stored_tags(conn: &Connection) -> Result<()> {
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT session_id, tag FROM tags")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (sid, tag) in rows {
        let norm = norm_tag(&tag);
        if norm != tag {
            conn.execute(
                "INSERT OR IGNORE INTO tags(session_id, tag) VALUES (?1, ?2)",
                params![sid, norm],
            )?;
            conn.execute(
                "DELETE FROM tags WHERE session_id = ?1 AND tag = ?2",
                params![sid, tag],
            )?;
        }
    }
    Ok(())
}

/// Apply migrations whose version exceeds the stored durable_version, in one
/// IMMEDIATE transaction (re-reading the version inside it so a concurrent
/// process that already migrated makes this a no-op). DDL + version bump are
/// atomic. Version-gating is the only thing that makes re-running safe, since
/// SQLite ALTER ADD COLUMN is not idempotent.
fn run_durable_migrations(conn: &Connection, migrations: &[Migration]) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<()> {
        let current: i64 = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'durable_version'",
                [],
                |r| r.get::<_, String>(0),
            )?
            .parse()
            .unwrap_or(BASELINE_DURABLE_VERSION);
        for m in migrations.iter().filter(|m| m.version > current) {
            match m.step {
                MigrationStep::Sql(sql) => conn.execute_batch(sql)?,
                MigrationStep::Fix(f) => f(conn)?,
            }
            conn.execute(
                "UPDATE meta SET value = ?1 WHERE key = 'durable_version'",
                params![m.version.to_string()],
            )?;
        }
        Ok(())
    })();
    match outcome {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(())
}

/// Ensure durable settings and metadata tables exist before the cache schema reads them.
fn create_meta_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;
    ensure_favorites_table(conn)?;
    ensure_snippets_table(conn)?;
    ensure_memory_proposals_table(conn)?;
    Ok(())
}

pub fn ensure_favorites_table(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS favorites(
            session_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );",
    );
    Ok(())
}

pub fn ensure_snippets_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS snippets(
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            message_idx INTEGER NOT NULL,
            title       TEXT NOT NULL,
            lang        TEXT,
            content     TEXT NOT NULL,
            tags        TEXT,
            created_at  TEXT NOT NULL,
            tool        TEXT,
            role        TEXT,
            message_ts  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_snippets_session ON snippets(session_id);",
    )?;
    // Columns added after these tables first shipped. Checked with a read so a
    // normal open writes nothing: every ALTER/UPDATE here used to run on each
    // open, and each one waits on the write lock while a sync holds it. A
    // read-only handle (the MCP server) leaves repairs to the next writer.
    if conn.is_readonly(rusqlite::MAIN_DB).unwrap_or(true) {
        return Ok(());
    }
    for (table, column, decl) in [
        ("snippets", "tool", "TEXT"),
        ("snippets", "role", "TEXT"),
        ("snippets", "message_ts", "TEXT"),
        ("files", "parent_id", "TEXT"),
        ("files", "project_key", "TEXT NOT NULL DEFAULT ''"),
        ("messages", "is_rewound", "INTEGER DEFAULT 0"),
        ("summaries", "kind", "TEXT NOT NULL DEFAULT 'llm'"),
    ] {
        if let Err(e) = add_missing_column(conn, table, column, decl) {
            eprintln!("warning: could not add {table}.{column}: {e:#}");
        }
    }
    if let Err(e) = backfill_snippet_metadata(conn) {
        eprintln!("warning: could not backfill snippet metadata: {e:#}");
    }
    // A column added after a schema version shipped needs its index created the
    // same way: the cache batch below only runs when the schema is behind. On a
    // brand-new index `files` does not exist yet, and its own CREATE carries the
    // index, so failing here is expected rather than worth a warning.
    let _ = conn
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_files_project_key ON files(project_key)");
    Ok(())
}

/// `ALTER TABLE .. ADD COLUMN` unless the table lacks nothing - or does not
/// exist yet, in which case its CREATE already carries the column.
fn add_missing_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("SELECT name FROM pragma_table_info(?1)")?;
    let cols: Vec<String> = stmt
        .query_map([table], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !cols.is_empty() && !cols.iter().any(|c| c == column) {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    }
    Ok(())
}

/// Fill tool/role/timestamp on snippets saved before those columns existed.
/// Runs once per index (gated in `meta`); new snippets get them on insert.
fn backfill_snippet_metadata(conn: &Connection) -> Result<()> {
    const KEY: &str = "snippet_meta_backfilled";
    let done = conn
        .query_row("SELECT 1 FROM meta WHERE key = ?1", [KEY], |_| Ok(()))
        .optional()?
        .is_some();
    if done {
        return Ok(());
    }
    let files_exist: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'files')",
        [],
        |r| r.get(0),
    )?;
    if !files_exist {
        // Fresh index: the cache tables come next, and nothing needs filling.
        return Ok(());
    }
    conn.execute_batch(
        "UPDATE snippets SET tool = coalesce(
            (SELECT tool FROM files WHERE files.session_id = snippets.session_id LIMIT 1),
            (SELECT tool FROM archive WHERE archive.session_id = snippets.session_id)
         ) WHERE tool IS NULL OR tool = '';
         UPDATE snippets SET role = 'assistant' WHERE role IS NULL OR role = '';
         UPDATE snippets SET message_ts = coalesce(
            (SELECT started FROM files WHERE files.session_id = snippets.session_id LIMIT 1),
            (SELECT started FROM archive WHERE archive.session_id = snippets.session_id),
            created_at
         ) WHERE message_ts IS NULL OR message_ts = '';",
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, '1')",
        [KEY],
    )?;
    Ok(())
}

/// The chosen FTS tokenizer is durable configuration. Indexes predating this
/// setting use the historical trigram tokenizer until a user changes it.
pub fn tokenizer_spec(conn: &Connection) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [FTS_TOKENIZER_META_KEY],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| DEFAULT_FTS_TOKENIZER.to_string()))
}

fn sql_string_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn validate_tokenizer_spec(conn: &Connection, spec: &str) -> Result<()> {
    let sql = format!(
        "CREATE VIRTUAL TABLE temp.sessionwiki_tokenizer_validation USING fts5(\
             text, tokenize='{}');\
         DROP TABLE temp.sessionwiki_tokenizer_validation;",
        sql_string_literal(spec)
    );
    conn.execute_batch(&sql)
        .map_err(anyhow::Error::from)
        .context("invalid FTS5 tokenizer specification")
}

fn create_msgs_table(conn: &Connection, spec: &str) -> Result<()> {
    let sql = format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(\
             text, content='messages', content_rowid='id', tokenize='{}');",
        sql_string_literal(spec)
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Store a tokenizer choice and rebuild only the FTS index. Session files are
/// not parsed; the external-content table rebuilds from `messages` in SQLite.
/// Returns whether the setting changed.
pub fn set_tokenizer_spec(conn: &Connection, spec: &str) -> Result<bool> {
    validate_tokenizer_spec(conn, spec)?;
    let current = tokenizer_spec(conn)?;
    if current == spec {
        return Ok(false);
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<bool> {
        let current = tokenizer_spec(conn)?;
        if current == spec {
            return Ok(false);
        }
        conn.execute_batch("DROP TABLE msgs;")?;
        create_msgs_table(conn, spec)?;
        conn.execute_batch("INSERT INTO msgs(msgs) VALUES('rebuild');")?;
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![FTS_TOKENIZER_META_KEY, spec],
        )?;
        Ok(true)
    })();
    match outcome {
        Ok(changed) => {
            conn.execute_batch("COMMIT")?;
            Ok(changed)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

/// Read whether full tool results are retained. Indexes without an explicit
/// choice use `summary`, keeping compact tool lines in the archive.
pub fn tool_output_mode(conn: &Connection) -> Result<ToolOutputMode> {
    let value = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [TOOL_OUTPUT_MODE_META_KEY],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match value {
        None => Ok(DEFAULT_TOOL_OUTPUT_MODE),
        Some(value) => value.parse().map_err(anyhow::Error::msg),
    }
}

/// Change tool-output retention for this index. Switching to `summary` clears
/// full output from live sessions only; archived sessions keep the contents
/// already stored in both `messages` and `archive`. Switching back to `full`
/// marks live sessions stale so the next sync re-parses their source files.
pub fn set_tool_output_mode(conn: &Connection, mode: ToolOutputMode) -> Result<bool> {
    if tool_output_mode(conn)? == mode {
        return Ok(false);
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<bool> {
        if tool_output_mode(conn)? == mode {
            return Ok(false);
        }
        match mode {
            ToolOutputMode::Full => {
                // Negative change tokens cannot match filesystem metadata or
                // shared-store tokens, and sync treats them as forced work
                // even for a bounded recent-session refresh.
                conn.execute(
                    "UPDATE files SET mtime = -1, size = -1 WHERE archived_at IS NULL",
                    [],
                )?;
            }
            ToolOutputMode::Summary => {
                // Archived sessions have no source file to recover output from,
                // so preserve their durable copy in both messages and archive.
                conn.execute(
                    "UPDATE messages SET full_text = NULL
                     WHERE session_id IN (
                         SELECT session_id FROM files WHERE archived_at IS NULL
                     )",
                    [],
                )?;
            }
        }
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![TOOL_OUTPUT_MODE_META_KEY, mode.as_str()],
        )?;
        Ok(true)
    })();
    match outcome {
        Ok(changed) => {
            conn.execute_batch("COMMIT")?;
            Ok(changed)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

struct ArchiveMessage {
    role: String,
    text: String,
    full_text: Option<String>,
    is_rewound: bool,
}

/// Read both the historical `[role, text]` archive rows and the newer
/// `[role, text, full_text]` rows. Legacy sessions naturally have no output.
fn decode_archive_transcript(transcript: &str) -> Result<Vec<ArchiveMessage>> {
    let rows: Vec<serde_json::Value> = serde_json::from_str(transcript)?;
    rows.into_iter()
        .map(|row| {
            let fields = row
                .as_array()
                .context("archive transcript message must be an array")?;
            anyhow::ensure!(
                fields.len() >= 2 && fields.len() <= 4,
                "archive transcript message must have two to four fields"
            );
            let role = fields[0]
                .as_str()
                .context("archive transcript role must be a string")?
                .to_owned();
            let text = fields[1]
                .as_str()
                .context("archive transcript text must be a string")?
                .to_owned();
            let full_text = match fields.get(2) {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(text)) => Some(text.clone()),
                Some(serde_json::Value::Bool(_)) => None,
                Some(_) => anyhow::bail!("archive full tool output must be a string or null"),
            };
            let is_rewound = match fields.get(3) {
                Some(serde_json::Value::Bool(b)) => *b,
                _ => match fields.get(2) {
                    Some(serde_json::Value::Bool(b)) => *b,
                    _ => false,
                },
            };
            Ok(ArchiveMessage {
                role,
                text,
                full_text,
                is_rewound,
            })
        })
        .collect()
}

#[cfg(test)]
fn create_cache_schema(conn: &Connection) -> Result<()> {
    create_meta_schema(conn)?;
    let spec = tokenizer_spec(conn)?;
    create_cache_schema_with_tokenizer(conn, &spec)
}

fn create_cache_schema_with_tokenizer(conn: &Connection, tokenizer: &str) -> Result<()> {
    let msgs_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'msgs')",
        [],
        |row| row.get(0),
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS files(
            path       TEXT PRIMARY KEY,
            mtime      INTEGER NOT NULL,
            size       INTEGER NOT NULL,
            session_id TEXT NOT NULL,
            tool       TEXT NOT NULL,
            project    TEXT NOT NULL DEFAULT '',
            title      TEXT NOT NULL DEFAULT '',
            started    TEXT,
            ended      TEXT,
            msg_count  INTEGER NOT NULL DEFAULT 0,
            kind       TEXT NOT NULL DEFAULT 'main',
            parent_id  TEXT,
            -- Set when the tool deleted the original session file but we kept
            -- the indexed copy (archive mode). NULL for live sessions.
            archived_at TEXT,
            -- The `READER` that parsed the row; 0 from a binary without it.
            reader     INTEGER NOT NULL DEFAULT 0,
            -- The comparable form of `project` (see util::project_key), folded up
            -- to the repository root. `project` keeps the tool's own spelling for
            -- display; every filter and grouping uses this.
            project_key TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_files_session ON files(session_id);
        CREATE INDEX IF NOT EXISTS idx_files_project_key ON files(project_key);
        -- Plain transcript rows + external-content FTS. Deleting a session is
        -- an indexed lookup here; with session_id stored UNINDEXED inside the
        -- FTS table it was a full scan per file, which made re-index runs
        -- quadratic in practice.
        CREATE TABLE IF NOT EXISTS messages(
            id         INTEGER PRIMARY KEY,
            session_id TEXT NOT NULL,
            role       TEXT NOT NULL,
            text       TEXT NOT NULL,
            full_text  TEXT,
            is_rewound INTEGER DEFAULT 0
        );
        CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id);
        CREATE TABLE IF NOT EXISTS summaries(
            session_id TEXT PRIMARY KEY,
            summary    TEXT NOT NULL,
            created    TEXT NOT NULL,
            -- 'llm': written by `summarize` (kept); 'auto': the instant
            -- extractive line, rewritten whenever the session is reindexed.
            kind       TEXT NOT NULL DEFAULT 'llm'
        );
        -- Curation layer (the editable 'wiki' part). Like summaries, these
        -- are user-authored and survive index rebuilds: only files/messages/
        -- msgs are dropped on a schema bump, never these.
        CREATE TABLE IF NOT EXISTS tags(
            session_id TEXT NOT NULL,
            tag        TEXT NOT NULL,
            PRIMARY KEY (session_id, tag)
        );
        CREATE INDEX IF NOT EXISTS idx_tags_tag ON tags(tag);
        CREATE TABLE IF NOT EXISTS notes(
            session_id TEXT PRIMARY KEY,
            note       TEXT NOT NULL,
            updated    TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS favorites(
            session_id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS snippets(
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            message_idx INTEGER NOT NULL,
            title       TEXT NOT NULL,
            lang        TEXT,
            content     TEXT NOT NULL,
            tags        TEXT,
            created_at  TEXT NOT NULL,
            tool        TEXT,
            role        TEXT,
            message_ts  TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_snippets_session ON snippets(session_id);
        -- Archive (durable, never dropped on a schema bump). When the tool
        -- deletes a session's original file, we keep a self-contained copy
        -- here - the distilled transcript and provenance plus the metadata
        -- needed to reconstruct the files row. This is the only table that is
        -- not re-derivable from disk, so on a schema bump the cache tables are
        -- rehydrated from it. Live sessions are NOT stored here.
        CREATE TABLE IF NOT EXISTS archive(
            session_id  TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            tool        TEXT NOT NULL,
            project     TEXT NOT NULL DEFAULT '',
            title       TEXT NOT NULL DEFAULT '',
            started     TEXT,
            ended       TEXT,
            msg_count   INTEGER NOT NULL DEFAULT 0,
            kind        TEXT NOT NULL DEFAULT 'main',
            transcript  TEXT NOT NULL,  -- JSON [[role,text,(full_text)],...] in order
            touched     TEXT NOT NULL,  -- JSON [path,...]
            archived_at TEXT NOT NULL
        );
        -- Provenance: which files each session edited or created, from its
        -- tool calls. Rebuilt from the sessions on sync, so it is dropped on a
        -- schema bump like messages - not curated. The path index powers
        -- `trace` (sessions for a file) and shared-file relatedness.
        CREATE TABLE IF NOT EXISTS touched(
            session_id TEXT NOT NULL,
            path       TEXT NOT NULL,
            PRIMARY KEY (session_id, path)
        );
        CREATE INDEX IF NOT EXISTS idx_touched_path ON touched(path);
        -- Evidence layer over `touched`: the concrete edits (kind + a bounded
        -- snippet of the change) behind each touched path. A log - multiple rows
        -- per (session, path) - so the whole change history of a file survives.
        -- Derived from the sessions on sync, dropped on a schema bump like
        -- touched. Powers `edits_for` and the file-history page.
        CREATE TABLE IF NOT EXISTS edits(
            session_id TEXT NOT NULL,
            path       TEXT NOT NULL,
            kind       TEXT NOT NULL,
            ts         TEXT,
            snippet    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_edits_path ON edits(path);
        CREATE INDEX IF NOT EXISTS idx_edits_session ON edits(session_id);
        -- Commits: passively extracted Git commits executed or referenced inside
        -- AI sessions. Enables instant commit SHA -> session lookup without altering
        -- git commit messages or hooks (non-invasive).
        CREATE TABLE IF NOT EXISTS session_commits(
            session_id TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            message    TEXT,
            timestamp  INTEGER,
            PRIMARY KEY (session_id, commit_sha)
        );
        CREATE INDEX IF NOT EXISTS idx_session_commits_sha ON session_commits(commit_sha);
        CREATE INDEX IF NOT EXISTS idx_session_commits_session ON session_commits(session_id);
        ",
    )?;
    if !files_has_reader(conn)? {
        conn.execute_batch("ALTER TABLE files ADD COLUMN reader INTEGER NOT NULL DEFAULT 0")?;
    }
    create_msgs_table(conn, tokenizer)?;
    if !msgs_exists {
        conn.execute_batch("INSERT INTO msgs(msgs) VALUES('rebuild');")?;
    }
    Ok(())
}

fn cache_schema_needs_work(conn: &Connection) -> Result<bool> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Ok(true);
    }
    for table in [
        "files",
        "messages",
        "summaries",
        "tags",
        "notes",
        "archive",
        "touched",
        "edits",
        "session_commits",
        "msgs",
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(true);
        }
    }
    Ok(!files_has_reader(conn)?)
}

fn files_has_reader(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('files') WHERE name = 'reader')",
        [],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE notes(session_id TEXT PRIMARY KEY, note TEXT NOT NULL, updated TEXT NOT NULL);
             INSERT INTO meta VALUES('durable_version','1');
             INSERT INTO notes VALUES('s1','keep me','t');",
        )
        .unwrap();
        c
    }

    #[test]
    fn runner_applies_gated_and_is_idempotent() {
        let c = mem();
        let migs = [Migration {
            version: 2,
            step: MigrationStep::Sql("ALTER TABLE notes ADD COLUMN pinned INTEGER"),
        }];
        run_durable_migrations(&c, &migs).unwrap();
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('notes')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(cols.contains(&"pinned".to_string()), "column added");
        let v: String = c
            .query_row(
                "SELECT value FROM meta WHERE key='durable_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "2", "version advanced");
        let note: String = c
            .query_row("SELECT note FROM notes WHERE session_id='s1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(note, "keep me", "existing durable row preserved");
        // re-run is a clean no-op (ADD COLUMN would otherwise error duplicate column)
        run_durable_migrations(&c, &migs).unwrap();
    }
}

/// Memory-mapped reads, except on Windows: it cannot shrink a file another
/// connection has mapped, so a VACUUM or incremental_vacuum in the app (whose
/// handlers keep their connections open) or next to a running `mcp` never
/// gave the space back. Reads measured the same without it.
fn set_mmap(conn: &Connection) {
    if !cfg!(windows) {
        let _ = conn.pragma_update(None, "mmap_size", 268435456i64);
    }
}

/// A read-only handle for serving processes (the MCP server): it can never
/// create the schema, migrate, VACUUM, or write durable data. Fails if no
/// index exists yet (unlike `open`, which creates one).
pub fn open_readonly() -> Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        db_path()?,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    set_mmap(&conn);
    let _ = conn.pragma_update(None, "cache_size", -32000i64);
    Ok(conn)
}

pub fn open() -> Result<Connection> {
    let conn = Connection::open(db_path()?)?;
    // A brand-new file can take incremental auto-vacuum for free; an existing
    // one switches on its first automatic compaction.
    let fresh: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    if fresh == 0 {
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    // A full sync grows the WAL by the size of everything it wrote; without a
    // limit the file keeps that size on disk after the checkpoint.
    let _ = conn.pragma_update(None, "journal_size_limit", 64i64 * 1024 * 1024);
    set_mmap(&conn);
    let _ = conn.pragma_update(None, "cache_size", -32000i64);
    let _ = conn.pragma_update(None, "temp_store", "MEMORY");
    create_meta_schema(&conn)?;
    if cache_schema_needs_work(&conn)? {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let cache_setup = (|| -> Result<()> {
            // Another opener may have repaired the schema while this one waited
            // for the lock, so avoid DDL if the cache is current now.
            if cache_schema_needs_work(&conn)? {
                let tokenizer = tokenizer_spec(&conn)?;
                let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
                // A newer binary built this cache. Rebuilding it in this
                // binary's format only makes that one rebuild it back - a
                // `sessionwiki mcp` left running across an upgrade did exactly
                // that on every call.
                if version > SCHEMA_VERSION {
                    anyhow::bail!(
                        "the index was written by a newer sessionwiki (schema {version}, this \
                         binary reads {SCHEMA_VERSION}); restart this process with the \
                         installed version"
                    );
                }
                if version != SCHEMA_VERSION {
                    // Drop only the derived cache. The durable tables (summaries, tags,
                    // notes, archive) are never dropped: rebuilding the index is cheap,
                    // re-running an LLM or recovering a session the tool already deleted is
                    // not. Archived sessions are rehydrated into the cache below.
                    conn.execute_batch(
                        "DROP TABLE IF EXISTS msgs;
                         DROP TABLE IF EXISTS messages;
                         DROP TABLE IF EXISTS touched;
                         DROP TABLE IF EXISTS files;
                         DROP TABLE IF EXISTS edits;
                         DROP TABLE IF EXISTS session_commits;",
                    )?;
                    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                    // Rebuilt rows restart at rowid 1; the fix-ups start over.
                    for key in FIXUP_MARKS {
                        conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
                    }
                }
                create_cache_schema_with_tokenizer(&conn, &tokenizer)?;
            }
            Ok(())
        })();
        match cache_setup {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        }
    }
    // Durable-table versioning, independent of user_version (the cache). `meta`
    // was opened before any cache schema changes above.
    let durable = read_or_init_durable_version(&conn)?;
    let latest = DURABLE_MIGRATIONS
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap_or(BASELINE_DURABLE_VERSION);
    if durable < latest {
        // Back up the irreplaceable durable data before the first migration runs.
        let bak = db_path()?.with_extension(format!("db.bak-v{durable}"));
        let _ = std::fs::remove_file(&bak);
        conn.execute("VACUUM INTO ?1", params![bak.to_string_lossy()])?;
        run_durable_migrations(&conn, DURABLE_MIGRATIONS)?;
    }
    // Replay archived sessions into the cache whenever any are missing from it
    // - after a schema bump (which dropped the cache) or if the cache was
    // cleared some other way. Gated on a count so a normal open does nothing.
    let arch_total: i64 = conn.query_row("SELECT count(*) FROM archive", [], |r| r.get(0))?;
    let arch_live: i64 = conn.query_row(
        "SELECT count(*) FROM files WHERE archived_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    if arch_total > arch_live {
        rehydrate_archive(&conn)?;
    }
    respell_projects(&conn)?;
    fill_project_keys(&conn)?;
    reclassify_codex_subagents(&conn)?;
    resolve_gemini_projects(&conn)?;
    reparse_codex_for_mcp_errors(&conn)?;
    reparse_large_for_provenance(&conn)?;
    reparse_after_reader_fixes(&conn)?;
    reparse_stale_reader_versions(&conn)?;
    Ok(conn)
}

/// Meta keys holding how far each index fix-up has checked `files`, by rowid.
/// A rebuild of the cache restarts rowids, so `open` clears these with it.
const FIXUP_MARKS: [&str; 4] = [
    "codex_subagent_upto",
    "project_case_upto",
    "gemini_project_upto",
    "project_key_upto",
];

/// The `files` rowids a fix-up has not yet checked: above its mark, up to the
/// current top. Not a once-only flag: a `sessionwiki mcp` started before an
/// upgrade keeps writing rows with the old code for weeks, and each such
/// write (INSERT OR REPLACE) lands above the mark, so the next open by this
/// version fixes it.
fn unchecked_rowids(conn: &Connection, key: &str) -> Result<(i64, i64)> {
    let mark: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let top: i64 = conn.query_row("SELECT coalesce(max(rowid), 0) FROM files", [], |r| {
        r.get(0)
    })?;
    Ok((mark, top))
}

/// Apply a fix-up's writes and advance its mark, but only if the write lock is
/// free right now. Opening the index must never wait on a writer (a sync can
/// hold it for minutes); a busy index is fixed by a later open instead.
/// `value` is what the mark key stores - a rowid, a version, or JSON.
fn apply_without_waiting(
    conn: &Connection,
    key: &str,
    value: &str,
    updates: &[(&str, Vec<String>)],
) -> Result<()> {
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let applied = (|| -> rusqlite::Result<()> {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        for (sql, args) in updates {
            tx.execute(sql, rusqlite::params_from_iter(args.iter()))?;
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta(key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        tx.commit()
    })();
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    match applied {
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            Ok(())
        }
        other => Ok(other?),
    }
}

/// Mark Codex sub-agent rollouts indexed as main - by a version that did not
/// read the marker - as `sub`, as a fresh parse now does. Only each file's
/// first line is read; a schema bump would re-parse every session on disk.
fn reclassify_codex_subagents(conn: &Connection) -> Result<()> {
    const KEY: &str = "codex_subagent_upto";
    let (mark, top) = unchecked_rowids(conn, KEY)?;
    if top <= mark {
        return Ok(());
    }
    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT session_id, path FROM files
             WHERE tool = 'codex' AND kind = 'main' AND rowid > ?1 AND rowid <= ?2",
        )?
        .query_map(params![mark, top], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut updates = Vec::new();
    for (session_id, path) in rows {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut first = String::new();
        if std::io::BufRead::read_line(&mut std::io::BufReader::new(file), &mut first).is_err() {
            continue;
        }
        let is_sub = serde_json::from_str::<serde_json::Value>(&first).is_ok_and(|v| {
            v.get("type").and_then(serde_json::Value::as_str) == Some("session_meta")
                && crate::adapters::codex_is_subagent_meta(&v)
        });
        if is_sub {
            updates.push((
                "UPDATE files SET kind = 'sub' WHERE session_id = ?1 AND tool = 'codex'",
                vec![session_id],
            ));
        }
    }
    apply_without_waiting(conn, KEY, &top.to_string(), &updates)
}

/// Replace the store-dir slug that Gemini rows indexed by an older version
/// carry as their project with the directory Gemini recorded for it, as a
/// fresh parse now does.
fn resolve_gemini_projects(conn: &Connection) -> Result<()> {
    const KEY: &str = "gemini_project_upto";
    let (mark, top) = unchecked_rowids(conn, KEY)?;
    if top <= mark {
        return Ok(());
    }
    let rows: Vec<(String, String)> = conn
        .prepare(
            "SELECT path, project FROM files
             WHERE tool = 'gemini' AND rowid > ?1 AND rowid <= ?2",
        )?
        .query_map(params![mark, top], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut updates = Vec::new();
    for (path, project) in rows {
        let Some(root) = crate::adapters::gemini_project_root(std::path::Path::new(&path)) else {
            continue;
        };
        let root = crate::util::project_key_as_on_disk(&root);
        if root != project {
            updates.push((
                "UPDATE files SET project = ?1 WHERE path = ?2",
                vec![root, path],
            ));
        }
    }
    apply_without_waiting(conn, KEY, &top.to_string(), &updates)
}

/// Codex rows parsed by an older Codex reader stay as it left them, since a
/// rollout that never changes is never parsed again. Mark the live ones for
/// one re-parse whenever the reader has changed since; the key is not a
/// fix-up mark, so a cache rebuild - which re-parses everything anyway - does
/// not repeat it. 1: failed MCP calls (0.34.3). 2: MCP calls recorded only as
/// events.
fn reparse_codex_for_mcp_errors(conn: &Connection) -> Result<()> {
    const KEY: &str = "codex_mcp_reparsed";
    const READER: i64 = 2;
    let done: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if done >= READER {
        return Ok(());
    }
    apply_without_waiting(
        conn,
        KEY,
        &READER.to_string(),
        &[(
            "UPDATE files SET mtime = -1, size = -1 WHERE tool = 'codex' AND archived_at IS NULL",
            vec![],
        )],
    )
}

/// Mark every live row a stale reader produced for a re-parse, tool by tool.
/// `reader_version` is per tool, so a fix to one parser re-reads only that
/// tool's sessions. The meta value records the versions already swept, so a
/// normal open runs one query instead of one UPDATE per tool, and the next
/// raise of a version sweeps again. Rows a pre-1.3 `sessionwiki mcp` still
/// running rewrites after the sweep keep an old `reader`; the next bump marks
/// them, since the sweep's stored version is then behind again.
///
/// Rows that never got a `reader` (0) are swept too: they predate the column,
/// which is exactly when they need today's parser.
fn reparse_stale_reader_versions(conn: &Connection) -> Result<()> {
    const KEY: &str = "reader_versions_applied";
    let stored: String = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .unwrap_or_default();
    // A value this build cannot read (or an empty one) means "never swept".
    let mut applied: std::collections::BTreeMap<String, i64> =
        serde_json::from_str(&stored).unwrap_or_default();
    let mut updates = Vec::new();
    for tool in crate::adapters::tool_names() {
        let want = crate::adapters::reader_version(tool);
        if applied.get(tool).copied().unwrap_or(0) >= want {
            continue;
        }
        applied.insert(tool.to_string(), want);
        updates.push((
            "UPDATE files SET mtime = -1, size = -1
             WHERE archived_at IS NULL AND tool = ?1 AND reader < CAST(?2 AS INTEGER)",
            vec![tool.to_string(), want.to_string()],
        ));
    }
    if updates.is_empty() {
        return Ok(());
    }
    let value = serde_json::to_string(&applied)?;
    apply_without_waiting(conn, KEY, &value, &updates)
}

/// Sessions over the byte cap were indexed without the files they edited
/// between their head and tail; an ended session never changes, so it is never
/// parsed again. Mark the live windowed rows for one re-parse whenever the
/// windowed reader has changed since. 1: provenance read from the whole file.
/// 2: the conversation between head and tail kept too. 3: Codex `compacted`
/// records no longer fill the middle's budget ahead of the conversation.
/// Stamped on every `files` row this version parses. Raise it with each reader
/// fix listed in `reparse_after_reader_fixes`.
///
/// 1.3: commit extraction (shipped in 1.2.0) reaches sessions indexed before
/// it, and web-chat transcripts keep thinking blocks. `adapters::reader_version`
/// reads this as the default, so raising it re-parses every tool at once; add an
/// override there to re-parse one tool alone.
pub const READER: i64 = 2;

/// Mark the live rows a fixed reader produces for a re-parse, so the fix
/// reaches sessions that never change again. Each fix names its rows and the
/// `READER` that introduced it; a row parsed by an older reader - including one
/// an older `sessionwiki mcp` rewrites after this ran - is marked again.
fn reparse_after_reader_fixes(conn: &Connection) -> Result<()> {
    // 1: a Codex history call (keys carry the store's \x1f separator) declined
    // at approval is a failure; Gemini `/rewind` is applied.
    const STALE: &str = "SELECT EXISTS(SELECT 1 FROM files
         WHERE archived_at IS NULL AND mtime != -1 AND reader < 1
           AND (tool = 'gemini' OR (tool = 'codex' AND instr(path, char(31)) > 0)))";
    if !conn.query_row(STALE, [], |r| r.get::<_, bool>(0))? {
        return Ok(());
    }
    apply_without_waiting(
        conn,
        "reader_fixes_marked",
        &READER.to_string(),
        &[(
            "UPDATE files SET mtime = -1, size = -1
             WHERE archived_at IS NULL AND mtime != -1 AND reader < 1
               AND (tool = 'gemini' OR (tool = 'codex' AND instr(path, char(31)) > 0))",
            vec![],
        )],
    )
}

fn reparse_large_for_provenance(conn: &Connection) -> Result<()> {
    const KEY: &str = "large_provenance_reparsed";
    const READER: i64 = 3;
    let done: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [KEY], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if done >= READER {
        return Ok(());
    }
    apply_without_waiting(
        conn,
        KEY,
        &READER.to_string(),
        &[(
            "UPDATE files SET mtime = -1, size = -1
             WHERE tool IN ('claude-code', 'codex') AND title LIKE '[large]%'
               AND archived_at IS NULL",
            vec![],
        )],
    )
}

/// Respell stored project paths as on disk, so case variants written by a
/// version without `project_key_as_on_disk` merge into one project. Only the
/// paths in rows not yet checked are looked at; a schema bump would re-parse
/// every session on disk to fix a handful of strings.
fn respell_projects(conn: &Connection) -> Result<()> {
    const KEY: &str = "project_case_upto";
    let (mark, top) = unchecked_rowids(conn, KEY)?;
    if top <= mark {
        return Ok(());
    }
    let projects: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT project FROM files
             WHERE project != '' AND rowid > ?1 AND rowid <= ?2",
        )?
        .query_map(params![mark, top], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut updates = Vec::new();
    for project in projects {
        let key = crate::util::project_key_as_on_disk(&project);
        if key != project {
            updates.push((
                "UPDATE files SET project = ?1 WHERE project = ?2",
                vec![key, project],
            ));
        }
    }
    apply_without_waiting(conn, KEY, &top.to_string(), &updates)
}

/// Give `project_key` to rows written before the column existed. One UPDATE per
/// distinct project string - thousands of sessions share a handful of paths -
/// in the same single transaction, and a busy index defers it to the next open.
fn fill_project_keys(conn: &Connection) -> Result<()> {
    const KEY: &str = "project_key_upto";
    let (mark, top) = unchecked_rowids(conn, KEY)?;
    if top <= mark {
        return Ok(());
    }
    let projects: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT project FROM files
             WHERE project != '' AND rowid > ?1 AND rowid <= ?2",
        )?
        .query_map(params![mark, top], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut updates = Vec::new();
    for project in projects {
        updates.push((
            "UPDATE files SET project_key = ?1 WHERE project = ?2 AND project_key != ?1",
            vec![crate::util::project_root_key(&project), project],
        ));
    }
    apply_without_waiting(conn, KEY, &top.to_string(), &updates)
}

/// After a schema bump drops the cache tables, replay archived sessions back
/// into them from the durable `archive` table, so search, `trace`, and reading
/// keep working for sessions whose originals the tool deleted. This is what
/// makes archive survive a rebuild; without it a version bump would silently
/// lose exactly the data that cannot be re-derived from disk.
fn rehydrate_archive(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT session_id, path, tool, project, title, started, ended,
                kind, transcript, touched, archived_at FROM archive",
    )?;
    let rows: Vec<ArchiveRow> = stmt
        .query_map([], |r| {
            Ok(ArchiveRow {
                session_id: r.get(0)?,
                path: r.get(1)?,
                tool: r.get(2)?,
                project: r.get(3)?,
                title: r.get(4)?,
                started: r.get(5)?,
                ended: r.get(6)?,
                kind: r.get(7)?,
                transcript: r.get(8)?,
                touched: r.get(9)?,
                archived_at: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    for a in rows {
        // The transcript is the durable backup; if it will not deserialize,
        // skip the session rather than rehydrate an empty shell that claims to
        // have content - that would be silent data loss disguised as success.
        let msgs = match decode_archive_transcript(&a.transcript) {
            Ok(m) => m,
            Err(e) => {
                eprintln!(
                    "archive: skipping {} - unreadable transcript ({e})",
                    a.session_id
                );
                continue;
            }
        };
        let paths: Vec<String> = serde_json::from_str(&a.touched).unwrap_or_else(|e| {
            eprintln!("archive: {} has unreadable provenance ({e})", a.session_id);
            Vec::new()
        });

        // Idempotent: clear any existing cache rows for this session first, so
        // re-running rehydrate never duplicates messages/FTS rows.
        delete_session_msgs(conn, &a.session_id)?;
        delete_session_provenance(conn, &a.session_id)?;
        conn.execute(
            "DELETE FROM files WHERE session_id = ?1",
            params![a.session_id],
        )?;

        // mtime/size are forced to 0 so that if this file ever reappears on
        // disk, the next sync always sees a mismatch and re-parses it, clearing
        // archived_at. msg_count comes from the actual transcript, never the
        // stored count, so the displayed count can never outrun the content.
        conn.execute(
            "INSERT INTO files
             (path, mtime, size, session_id, tool, project, title, started, ended,
              msg_count, kind, archived_at, project_key)
             VALUES (?1,0,0,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                a.path,
                a.session_id,
                a.tool,
                crate::util::project_key_as_on_disk(&a.project),
                a.title,
                a.started,
                a.ended,
                msgs.len() as i64,
                a.kind,
                a.archived_at,
                crate::util::project_root_key(&a.project),
            ],
        )?;
        {
            let mut ins_row = conn.prepare_cached(
                "INSERT INTO messages(session_id, role, text, full_text, is_rewound)
                     VALUES (?1,?2,?3,?4,?5)",
            )?;
            let mut ins_fts =
                conn.prepare_cached("INSERT INTO msgs(rowid, text) VALUES (?1,?2)")?;
            for message in &msgs {
                // Re-normalize on rehydrate: pre-fix archives hold raw/NFD JSON,
                // so this is where archived Korean sessions become NFC again.
                // Also redact - a pre-redaction archive holds raw secrets.
                let text = crate::redact::redact(&crate::util::nfc(&message.text)).into_owned();
                let full_text = message
                    .full_text
                    .as_deref()
                    .map(|text| crate::redact::redact(&crate::util::nfc(text)).into_owned());
                // External-content FTS requires byte-identical message text.
                ins_row.execute(params![
                    a.session_id,
                    message.role,
                    text,
                    full_text,
                    message.is_rewound as i32
                ])?;
                ins_fts.execute(params![conn.last_insert_rowid(), text])?;
            }
        }
        let mut ins_touched =
            conn.prepare_cached("INSERT OR IGNORE INTO touched(session_id, path) VALUES (?1,?2)")?;
        for p in &paths {
            ins_touched.execute(params![a.session_id, crate::util::nfc(p)])?;
        }
        let mut ins_commit = conn.prepare_cached(
            "INSERT OR IGNORE INTO session_commits(session_id, commit_sha, message, timestamp) VALUES (?1,?2,?3,?4)",
        )?;
        for message in &msgs {
            // Only output the agent produced can hold its own commit result; a
            // user paste of someone else's `git commit` is not authorship.
            if !matches!(message.role.as_str(), "assistant" | "tool") {
                continue;
            }
            let mut commits = extract_commits_from_text(&message.text);
            // The `git commit` output a tool row really carries is in the raw
            // result, not in the display text.
            if let Some(raw) = &message.full_text {
                commits.extend(extract_commits_from_text(raw));
            }
            for (sha, msg) in commits {
                ins_commit.execute(params![a.session_id, sha, msg, None::<i64>])?;
            }
        }
    }
    Ok(())
}

struct ArchiveRow {
    session_id: String,
    path: String,
    tool: String,
    project: String,
    title: String,
    started: Option<String>,
    ended: Option<String>,
    kind: String,
    transcript: String,
    touched: String,
    archived_at: String,
}

/// Move what the user added to a session onto the id it answers to now.
///
/// A parser fix can respell an id - the web-chat adapters used to hash the
/// export file into it - and the same key then re-indexes under a new one. The
/// curation rows (marks, notes, snippets) belong to the conversation, so they
/// follow it; the transcript under the old id is the same conversation read by
/// an older parser, so it goes, unless another file still lists it.
fn carry_identity_over(tx: &rusqlite::Transaction, key: &str, new_id: &str) -> Result<()> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT session_id FROM files WHERE path = ?1 AND session_id <> ?2",
            params![key, new_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(old_id) = previous else {
        return Ok(());
    };
    for table in ["tags", "notes", "summaries", "favorites", "snippets"] {
        tx.execute(
            &format!("UPDATE OR IGNORE {table} SET session_id = ?2 WHERE session_id = ?1"),
            params![old_id, new_id],
        )?;
    }
    // The row being replaced is the one that named the old id; drop it first so
    // the reference check below sees only the other spellings of this session.
    tx.execute(
        "DELETE FROM files WHERE path = ?1 AND session_id = ?2",
        params![key, old_id],
    )?;
    let still_listed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE session_id = ?1)",
        params![old_id],
        |r| r.get(0),
    )?;
    if !still_listed {
        delete_session_msgs(tx, &old_id)?;
        delete_session_provenance(tx, &old_id)?;
        tx.execute("DELETE FROM archive WHERE session_id = ?1", params![old_id])?;
    }
    Ok(())
}

/// Bring the index up to date with what is on disk. Files whose (mtime, size)
/// changed since the last run are re-parsed; archived rows whose source has
/// returned unchanged are made live again without re-reading the transcript.
/// Insert one parsed session into the cache tables (files, messages, msgs,
/// touched) and tag it if an oh-my-* harness drove it. `key` is the stored
/// path/identity, `mtime` the change-token, `size` the byte size (0 for
/// shared-store sessions). Shared by the file-per-session and shared-store paths.
fn index_one(
    tx: &rusqlite::Transaction,
    session: &crate::model::Session,
    key: &str,
    mtime: i64,
    size: i64,
    output_mode: ToolOutputMode,
) -> Result<()> {
    // A reader fix can respell a session's id - a web chat used to take its id
    // from the export file it sat in - and everything the user added belongs to
    // the conversation, not to the id, so it has to follow it across.
    carry_identity_over(tx, key, &session.id)?;
    delete_session_msgs(tx, &session.id)?;
    delete_session_provenance(tx, &session.id)?;
    // A path that was archived (the tool deleted it, now it is back) is live
    // again: this INSERT clears archived_at, and the durable copy is dropped.
    tx.execute(
        "DELETE FROM archive WHERE session_id = ?1",
        params![session.id],
    )?;
    tx.execute(
        "INSERT OR REPLACE INTO files
         (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id, reader, project_key)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            key,
            mtime,
            size,
            session.id,
            session.tool,
            crate::util::project_key_as_on_disk(&session.project),
            crate::redact::redact(&session.title).as_ref(),
            session.started.map(|t| t.to_rfc3339()),
            session.ended.map(|t| t.to_rfc3339()),
            session.messages.len() as i64,
            if session.subagent { "sub" } else { "main" },
            session.parent_id.as_deref(),
            // The tool's own parser version, so one tool's fix re-parses only
            // that tool's rows (see adapters::reader_version).
            crate::adapters::reader_version(session.tool),
            crate::util::project_root_key(&session.project),
        ],
    )?;
    fold_moved_copy(tx, session, key)?;
    // Contract: messages are inserted in transcript order within one
    // transaction, so the autoincrement messages.id is a monotonic proxy for
    // order (preview + the web transcript rely on it). Keep this sequential.
    {
        let mut ins_row = tx.prepare_cached(
            "INSERT INTO messages(session_id, role, text, full_text, is_rewound)
             VALUES (?1,?2,?3,?4,?5)",
        )?;
        let mut ins_fts = tx.prepare_cached("INSERT INTO msgs(rowid, text) VALUES (?1,?2)")?;
        for m in &session.messages {
            // Normalize once and reuse for the plain row and external-content
            // FTS row: they MUST be byte-identical or FTS delete/rebuild corrupts.
            // Strip secrets before they enter the index, which outlives the
            // original session in archive mode.
            let text = crate::redact::redact(&crate::util::nfc(&m.text)).into_owned();
            let full_text = if output_mode == ToolOutputMode::Full {
                match &m.tool {
                    Some(crate::model::ToolEvent::Summary(summary)) => {
                        summary.output.as_deref().map(|output| {
                            crate::redact::redact(&crate::util::nfc(output)).into_owned()
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };
            ins_row.execute(params![
                session.id,
                m.role.label(),
                text,
                full_text,
                m.is_rewound as i32
            ])?;
            ins_fts.execute(params![tx.last_insert_rowid(), text])?;
        }
        let mut ins_touched =
            tx.prepare_cached("INSERT OR IGNORE INTO touched(session_id, path) VALUES (?1,?2)")?;
        for p in &session.touched {
            ins_touched.execute(params![session.id, crate::util::nfc(p)])?;
        }
        // Evidence layer (a log, not a set - the same file edited twice keeps
        // both rows). Prior rows were cleared with `touched` above.
        let mut ins_edit = tx.prepare_cached(
            "INSERT INTO edits(session_id, path, kind, ts, snippet) VALUES (?1,?2,?3,?4,?5)",
        )?;
        for e in &session.edits {
            ins_edit.execute(params![
                session.id,
                crate::util::nfc(&e.path),
                e.kind.as_str(),
                e.ts.map(|t| t.to_rfc3339()),
                crate::redact::redact(&e.snippet).as_ref(),
            ])?;
        }
        // Commit tracking: the commits this session created, read out of the
        // output it produced. The user's own messages are not scanned - they
        // quote other people's history all the time.
        let mut ins_commit = tx.prepare_cached(
            "INSERT OR IGNORE INTO session_commits(session_id, commit_sha, message, timestamp) VALUES (?1,?2,?3,?4)",
        )?;
        for m in &session.messages {
            if !matches!(m.role, Role::Assistant | Role::Tool) {
                continue;
            }
            let mut commits = extract_commits_from_text(&m.text);
            if let Some(crate::model::ToolEvent::Summary(summary)) = &m.tool {
                if let Some(output) = &summary.output {
                    commits.extend(extract_commits_from_text(output));
                }
            }
            let ts = m.ts.map(|t| t.timestamp());
            for (sha, msg) in commits {
                ins_commit.execute(params![session.id, sha, msg, ts,])?;
            }
        }
    }
    // Tag sessions an oh-my-* harness drove (it wraps Claude Code / Codex /
    // OpenCode) so they are filterable; recomputed on every reindex.
    if matches!(session.tool, "claude-code" | "codex" | "opencode") {
        if let Some(h) = crate::adapters::harness::detect(&session.project) {
            add_tag(tx, &session.id, h)?;
        }
    }
    Ok(())
}

/// The source exists again with the exact metadata we indexed before it was
/// mistakenly archived. Its searchable rows are still present, so restoring
/// the live marker needs no transcript parse or FTS rewrite.
fn restore_unchanged(conn: &mut Connection, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for key in keys {
        tx.execute(
            "DELETE FROM archive WHERE session_id = (SELECT session_id FROM files WHERE path = ?1)",
            params![key],
        )?;
        tx.execute(
            "UPDATE files SET archived_at = NULL WHERE path = ?1",
            params![key],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// A Codex conversation can change path: archiving a thread moves its rollout,
/// and after the history migration Codex may delete the rollout while the
/// thread lives on in its history db. The old path's row is kept as archived,
/// so once the conversation is indexed at its new path the kept row is a second
/// listing of it. Fold it in, moving its tags and note across, unless it held
/// more messages than the new copy - folding that would lose content.
fn fold_moved_copy(
    tx: &rusqlite::Transaction,
    session: &crate::model::Session,
    key: &str,
) -> Result<()> {
    if session.tool != "codex" {
        return Ok(());
    }
    let Some(native) = native_id_of(key) else {
        return Ok(());
    };
    // Not only rows already archived: the old row can still be live when the
    // new one is indexed (its adapter reconciles later). A live row whose
    // file still exists is a second copy, not a move, and is left alone.
    let kept: Vec<(String, String, i64, bool)> = tx
        .prepare(
            "SELECT path, session_id, msg_count, archived_at IS NOT NULL FROM files
             WHERE tool = 'codex' AND path <> ?1 AND instr(lower(path), ?2) > 0",
        )?
        .query_map(params![key, native], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (path, sid, count, archived) in kept {
        if sid == session.id
            || native_id_of(&path).as_deref() != Some(native.as_str())
            || count > session.messages.len() as i64
            || (!archived && std::path::Path::new(&path).exists())
        {
            continue;
        }
        for table in ["tags", "notes", "summaries"] {
            tx.execute(
                &format!("UPDATE OR IGNORE {table} SET session_id = ?2 WHERE session_id = ?1"),
                params![sid, session.id],
            )?;
        }
        delete_session_msgs(tx, &sid)?;
        delete_session_provenance(tx, &sid)?;
        for table in ["files", "archive", "summaries", "tags", "notes"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id = ?1"),
                params![sid],
            )?;
        }
    }
    Ok(())
}

/// The end-of-adapter sync line. Honest about failures: "indexed 12/14
/// (2 failed to parse)" rather than pretending everything landed.
fn report_indexed(tool: &str, total: usize, failed: usize) {
    if failed > 0 {
        eprintln!(
            "\r[{tool}] indexed {}/{total} ({failed} failed to parse)    ",
            total - failed
        );
    } else {
        eprintln!("\r[{tool}] indexed {total}/{total}    ");
    }
}

/// Write one parsed session in its own short transaction. The write lock is
/// held for the few milliseconds its rows take, never across a parse - a
/// sync of thousands of sessions used to keep it for seconds at a time, and
/// a favorite, note or tag saved meanwhile (or another process's sync) failed
/// as "database is locked" once its busy timeout ran out. IMMEDIATE takes the
/// lock up front, so a concurrent writer is waited for through the busy
/// timeout instead of failing the upgrade from a stale read snapshot. Each
/// session is still written whole, and an interrupted sync keeps what it
/// already indexed.
fn write_session(
    conn: &mut Connection,
    session: &crate::model::Session,
    key: &str,
    mtime: i64,
    size: i64,
    output_mode: ToolOutputMode,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    index_one(&tx, session, key, mtime, size, output_mode)?;
    if !session.subagent && stored_auto_summary_version(&tx)? <= AUTO_SUMMARY_VERSION {
        store_auto_summary(
            &tx,
            &session.id,
            crate::brief::short_summary(session).as_deref(),
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Keep the instant one-line summary in step with the session; a summary the
/// model wrote is never touched.
fn store_auto_summary(conn: &Connection, session_id: &str, line: Option<&str>) -> Result<()> {
    match line {
        Some(line) => conn.execute(
            "INSERT INTO summaries(session_id, summary, created, kind)
             VALUES (?1, ?2, datetime('now'), 'auto')
             ON CONFLICT(session_id) DO UPDATE SET summary = excluded.summary, created = excluded.created
             WHERE summaries.kind = 'auto'",
            params![session_id, line],
        )?,
        None => conn.execute(
            "DELETE FROM summaries WHERE session_id = ?1 AND kind = 'auto'",
            params![session_id],
        )?,
    };
    Ok(())
}

/// Bump when [`crate::brief::short_summary`] changes what it writes.
const AUTO_SUMMARY_VERSION: i64 = 1;
const AUTO_SUMMARY_KEY: &str = "auto_summary_version";

/// Give sessions indexed before instant summaries existed their line, a slice
/// per sync (newest first, about a second of work) so no sync is held up.
/// The line-maker version the index's instant lines were written by (0 when
/// none is recorded).
fn stored_auto_summary_version(conn: &Connection) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [AUTO_SUMMARY_KEY],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0))
}

/// Settle which line-maker owns the instant lines, and whether this binary
/// (at `current`) may write them. A newer one re-derives them all: the old
/// lines go and backfill fills them in again (model-written ones stay). The
/// version only ever moves up, so an older process still running beside a
/// newer one (a long-lived MCP server, say) neither wipes the newer lines nor
/// writes its own over them - two versions would otherwise take turns
/// deleting each other's work on every sync.
fn claim_auto_summaries(conn: &mut Connection, current: i64) -> Result<bool> {
    let stored = stored_auto_summary_version(conn)?;
    if stored > current {
        return Ok(false);
    }
    if stored < current {
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        // Re-read under the write lock: another process may have moved it.
        let stored = stored_auto_summary_version(&tx)?;
        if stored > current {
            return Ok(false);
        }
        if stored < current {
            tx.execute("DELETE FROM summaries WHERE kind = 'auto'", [])?;
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![AUTO_SUMMARY_KEY, current.to_string()],
            )?;
        }
        tx.commit()?;
    }
    Ok(true)
}

fn backfill_auto_summaries(conn: &mut Connection) -> Result<usize> {
    if !claim_auto_summaries(conn, AUTO_SUMMARY_VERSION)? {
        return Ok(0);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1000);
    let mut done = 0;
    loop {
        let rows = summary_candidates(conn, 50, None, false)?;
        if rows.is_empty() {
            return Ok(done);
        }
        let mut lines = Vec::with_capacity(rows.len());
        for row in &rows {
            let line = session_from_index(conn, row)
                .ok()
                .and_then(|s| crate::brief::short_summary(&s));
            // A session with nothing to say still gets a row, or it would be
            // picked again on every sync.
            lines.push((row.session_id.clone(), line.unwrap_or_default()));
        }
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for (id, line) in &lines {
            tx.execute(
                "INSERT OR IGNORE INTO summaries(session_id, summary, created, kind)
                 VALUES (?1, ?2, datetime('now'), 'auto')",
                params![id, line],
            )?;
        }
        tx.commit()?;
        done += lines.len();
        if std::time::Instant::now() >= deadline {
            return Ok(done);
        }
    }
}

pub fn sync(conn: &mut Connection, only_tool: Option<&str>) -> Result<()> {
    sync_bounded(conn, only_tool, None)
}

/// Like [`sync`], but when `since` is set, only files/sessions modified at or
/// after that epoch-second are (re)parsed - a bounded FRESHNESS top-up for the
/// MCP path, so `recent_sessions` picks up a just-started sibling without paying
/// the full-corpus re-parse (the 46GB-codex trap). Deletion reconciliation still
/// sees every file, so a bounded run never archives a live session; older
/// changed files are simply left for the next full `sync`.
pub fn sync_bounded(
    conn: &mut Connection,
    only_tool: Option<&str>,
    since: Option<i64>,
) -> Result<()> {
    let adapters: Vec<Box<dyn Adapter>> = match only_tool {
        Some(t) => adapters::named(t),
        None => adapters::all(),
    };
    sync_with(conn, &adapters, since)
}

/// Like [`sync_bounded`], but over an explicit adapter list instead of the
/// built-in registry. A program that embeds this crate as a library can index
/// its own sessions by passing its own [`Adapter`] alongside `adapters::all()`.
/// Readers without that adapter use the indexed transcript; call this again
/// to make later changes to those sessions visible to the standalone binary.
pub fn sync_with(
    conn: &mut Connection,
    adapters: &[Box<dyn Adapter>],
    since: Option<i64>,
) -> Result<()> {
    // Per-session progress redraws one line with `\r`, which only means
    // anything on a terminal. Redirected into a log it becomes a single line
    // megabytes long, so decide once per sync and skip those writes when stderr
    // is not a terminal. Warnings and the per-tool summary still print.
    let progress = std::io::stderr().is_terminal();
    let output_mode = tool_output_mode(conn)?;
    let mut known: HashMap<String, (i64, i64, bool)> = HashMap::new();
    {
        let mut stmt =
            conn.prepare("SELECT path, mtime, size, archived_at IS NOT NULL FROM files")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                (
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, bool>(3)?,
                ),
            ))
        })?;
        for row in rows {
            let (path, ms) = row?;
            known.insert(path, ms);
        }
    }

    let mut archived_total = 0usize;
    let mut wrote = false;
    for adapter in adapters {
        // `store_present` tells "the tool pruned some sessions" (root exists,
        // those gone) apart from "the whole store vanished" (uninstall,
        // unmounted) - we must not mass-archive on the latter.
        let tool = adapter.name();
        let store_present = adapter.root().is_some_and(|r| r.exists());

        // Shared store (e.g. OpenCode's SQLite db): enumerate sessions by key +
        // change-token and re-parse only the changed ones, bypassing the
        // file-per-session path. The token is stored in the `mtime` column
        // (size 0), so the same change comparison works.
        if let Some(store) = adapter.store() {
            let mut seen: Vec<String> = Vec::with_capacity(store.keys.len());
            let mut pending: Vec<String> = Vec::new();
            let mut restore: Vec<String> = Vec::new();
            for (key, token) in &store.keys {
                match known.get(key) {
                    Some(&(mtime, 0, true)) if mtime == *token => restore.push(key.clone()),
                    Some(&(mtime, 0, false)) if mtime == *token => {}
                    old if old.is_some_and(|row| row.2 || row.0 < 0 || row.1 < 0)
                        || since.is_none_or(|s| *token >= s) =>
                    {
                        pending.push(key.clone());
                    }
                    _ => {}
                }
                seen.push(key.clone());
            }
            restore_unchanged(conn, &restore)?;

            if !pending.is_empty() {
                let token_of: HashMap<&str, i64> =
                    store.keys.iter().map(|(k, t)| (k.as_str(), *t)).collect();
                let total = pending.len();
                let mut failed = 0usize;
                wrote = true;
                for (i, key) in pending.iter().enumerate() {
                    if progress {
                        eprint!("\r[{tool}] indexing {}/{total}", i + 1);
                        std::io::stderr().flush().ok();
                    }
                    // A failed parse is warned, not silently dropped: the user
                    // must know a session is missing from the corpus.
                    let session = match adapters::parse_session_key(adapter.as_ref(), key) {
                        Ok(s) => s,
                        Err(e) => {
                            failed += 1;
                            eprintln!("\r[{tool}] failed to parse {key}: {e:#}");
                            continue;
                        }
                    };
                    let token = token_of.get(key.as_str()).copied().unwrap_or(0);
                    write_session(conn, &session, key, token, 0, output_mode)?;
                }
                report_indexed(tool, total, failed);
            }

            // Reconcile deletions only when the whole store was read this run.
            // If a backing db was present but unreadable (locked, half-written),
            // `seen` is partial - pruning off it would archive the whole corpus
            // on a transient hiccup, so skip reconciliation until a clean read.
            if store.had_error {
                // The discover path says this out loud; this one did not. It is
                // the path aider takes, whose walk is capped at two seconds over
                // the whole home - so on a large home the flag can be set every
                // run, reconciliation never happens, and nothing says why a
                // session the tool deleted is still listed.
                eprintln!(
                    "[{tool}] the store could not be read in full; \
                     skipping deletion reconciliation this run"
                );
            } else {
                // After the new keys are written, so a conversation the store
                // now lists at a different key is recognised as the same session
                // rather than archived as a deleted one.
                archived_total += archive_or_prune(
                    conn,
                    tool,
                    &seen,
                    store_present,
                    adapter.reconcile_scope().as_deref(),
                )?;
            }
            continue;
        }

        let discovered = adapter.discover();
        let mut seen: Vec<String> = Vec::with_capacity(discovered.files.len());
        let mut pending: Vec<(PathBuf, i64, i64)> = Vec::new();
        let mut restore: Vec<String> = Vec::new();

        for f in discovered.files {
            let meta = match f.metadata() {
                Ok(m) => m,
                Err(e) => {
                    // The file was just discovered, so it exists: a failed
                    // stat must keep it in `seen` (dropping it would let
                    // reconciliation archive a live session) and be warned,
                    // not swallowed.
                    eprintln!("\r[{tool}] failed to stat {}: {e}", f.display());
                    seen.push(f.to_string_lossy().into_owned());
                    continue;
                }
            };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let size = meta.len() as i64;
            let key = f.to_string_lossy().into_owned();
            match known.get(&key) {
                Some(&(old_mtime, old_size, true)) if old_mtime == mtime && old_size == size => {
                    restore.push(key.clone());
                }
                Some(&(old_mtime, old_size, false)) if old_mtime == mtime && old_size == size => {}
                old if old.is_some_and(|row| row.2 || row.0 < 0 || row.1 < 0)
                    || since.is_none_or(|s| mtime >= s) =>
                {
                    pending.push((f, mtime, size));
                }
                _ => {}
            }
            seen.push(key);
        }
        restore_unchanged(conn, &restore)?;

        // Same guard the shared-store path has: a partial walk (an unreadable
        // directory) means `seen` is incomplete - reconciling deletions off it
        // would archive live sessions, so wait for a clean walk.
        if discovered.had_error {
            eprintln!(
                "[{tool}] some session directories could not be read; \
                 skipping deletion reconciliation this run"
            );
        } else {
            archived_total += archive_or_prune(
                conn,
                tool,
                &seen,
                store_present,
                adapter.reconcile_scope().as_deref(),
            )?;
        }

        if pending.is_empty() {
            continue;
        }
        wrote = true;
        let total = pending.len();
        let mut done = 0usize;
        let mut failed = 0usize;
        for (path, mtime, size) in pending {
            done += 1;
            if progress {
                eprint!("\r[{tool}] indexing {done}/{total}");
                std::io::stderr().flush().ok();
            }

            // A failed parse is warned, not silently dropped: the user must
            // know a session is missing from the corpus.
            let session = match adapters::parse_session(adapter.as_ref(), &path) {
                Ok(s) => s,
                Err(e) => {
                    failed += 1;
                    eprintln!("\r[{tool}] failed to parse {}: {e:#}", path.display());
                    continue;
                }
            };
            let key = path.to_string_lossy();
            write_session(conn, &session, &key, mtime, size, output_mode)?;
        }
        report_indexed(tool, total, failed);
    }
    let backfilled = match backfill_auto_summaries(conn) {
        Ok(n) => n,
        Err(e) => {
            eprintln!("warning: could not backfill instant summaries: {e:#}");
            0
        }
    };
    if wrote || archived_total > 0 || backfilled > 0 {
        maintain(conn);
    }

    // The one passive signal that archive is earning its keep: how many
    // sessions we kept this run that the tool deleted, and the running total.
    if archived_total > 0 {
        let kept: i64 = conn.query_row(
            "SELECT count(*) FROM files WHERE kind='main' AND archived_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        eprintln!(
            "archived {archived_total} session(s) the tool removed ({kept} kept that your tools have deleted)"
        );
    }
    Ok(())
}

/// FTS leaf pages merged per sync. Re-indexing a live session deletes and
/// re-inserts all its rows, which leaves the search index in ever more, ever
/// emptier segments; a bounded merge after each writing sync keeps that from
/// piling up (on a real index, ~6% of the file) without the multi-second full
/// `optimize` that `sessionwiki vacuum` runs.
const FTS_MERGE_PAGES: i64 = 500;

/// Free pages returned to the filesystem per sync, once incremental
/// auto-vacuum is on: 4096 pages = 16 MB with the default page size.
const VACUUM_STEP_PAGES: i64 = 4096;

/// Best-effort upkeep after a sync that wrote: bounded FTS merge, free pages
/// back to the filesystem, fresh planner statistics. Never waits on another
/// writer - a busy index is maintained by a later sync.
fn maintain(conn: &Connection) {
    let _ = conn.busy_timeout(std::time::Duration::ZERO);
    let _ = conn.execute(
        "INSERT INTO msgs(msgs, rank) VALUES('merge', ?1)",
        params![FTS_MERGE_PAGES],
    );
    let incremental = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get::<_, i64>(0))
        .is_ok_and(|mode| mode == 2);
    if incremental {
        // Only when the free list is worth a pass: over 1 MB and over 2%.
        let free: i64 = conn
            .pragma_query_value(None, "freelist_count", |r| r.get(0))
            .unwrap_or(0);
        let total: i64 = conn
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap_or(0);
        if free > 256 && free * 50 > total {
            let _ = conn.execute_batch(&format!(
                "PRAGMA incremental_vacuum({});",
                free.min(VACUUM_STEP_PAGES)
            ));
        }
    }
    // Planner statistics for the tables a sync just grew; bounded so it stays
    // cheap on a large index.
    let _ = conn.execute_batch("PRAGMA analysis_limit = 400; PRAGMA optimize;");
    let _ = conn.busy_timeout(std::time::Duration::from_millis(5000));
}

/// Sizes around one [`compact`] run: database plus WAL, in bytes.
#[derive(Debug, Clone, Copy)]
pub struct CompactReport {
    pub before: u64,
    pub after: u64,
    pub busy: bool,
}

/// Meta keys recording the last full compaction: when, and the size it left.
const COMPACT_AT_KEY: &str = "compact_at";
const COMPACT_SIZE_KEY: &str = "compact_size";

fn db_and_wal_size(path: &std::path::Path) -> u64 {
    let wal = path.with_file_name("index.db-wal");
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        + std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
}

/// Full compaction: optimize the search index into one segment, rebuild the
/// file (switching it to incremental auto-vacuum on the way - the setting only
/// takes effect through a VACUUM) and truncate the WAL. Refuses when the disk
/// lacks room for the rebuild (twice the database). Records when it ran, which
/// [`compact_due`] reads.
pub fn compact(conn: &Connection, path: &std::path::Path) -> Result<CompactReport> {
    let before = db_and_wal_size(path);
    let required = before.saturating_mul(2);
    let free = match std::env::var("SESSIONWIKI_SIMULATE_FREE_SPACE") {
        Ok(val) => val.parse::<u64>().unwrap_or(0),
        Err(_) => crate::util::available_space(path)?,
    };
    if free < required {
        anyhow::bail!(
            "insufficient disk space for vacuum: need at least {} (2x database size), but only {} available on volume",
            crate::util::human_size(required),
            crate::util::human_size(free),
        );
    }
    conn.execute_batch("INSERT INTO msgs(msgs) VALUES('optimize');")
        .context("optimize FTS5 search index")?;
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;")
        .context("vacuum database")?;
    let busy: i32 = conn
        .query_row("PRAGMA wal_checkpoint(TRUNCATE);", [], |r| r.get(0))
        .context("wal_checkpoint(TRUNCATE)")?;
    if busy != 0 {
        eprintln!("warning: WAL checkpoint was blocked by an active reader");
    }
    let after = db_and_wal_size(path);
    let set = |key: &str, value: String| {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
    };
    set(COMPACT_AT_KEY, chrono::Utc::now().timestamp().to_string())?;
    set(COMPACT_SIZE_KEY, after.to_string())?;
    Ok(CompactReport {
        before,
        after,
        busy: busy != 0,
    })
}

/// Below this a full compaction is never worth interrupting anyone for.
const COMPACT_MIN_BYTES: u64 = 16 * 1024 * 1024;

/// The index file as the Activity tab shows it: size on disk (with WAL),
/// free pages, whether syncs give space back on their own, and the last
/// full compaction (Unix seconds and the size it left), if any.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StorageStatus {
    pub bytes: u64,
    pub free_bytes: u64,
    pub incremental: bool,
    pub compacted_at: Option<i64>,
    pub compacted_size: Option<u64>,
}

pub fn storage_status(conn: &Connection, path: &std::path::Path) -> Result<StorageStatus> {
    let pragma =
        |name: &str| -> Result<i64> { Ok(conn.pragma_query_value(None, name, |r| r.get(0))?) };
    let meta = |key: &str| -> Result<Option<i64>> {
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok()))
    };
    Ok(StorageStatus {
        bytes: db_and_wal_size(path),
        free_bytes: (pragma("freelist_count")? * pragma("page_size")?).max(0) as u64,
        incremental: pragma("auto_vacuum")? == 2,
        compacted_at: meta(COMPACT_AT_KEY)?,
        compacted_size: meta(COMPACT_SIZE_KEY)?.map(|v| v.max(0) as u64),
    })
}

/// Whether a full [`compact`] is worth running now, and why. Cheap: a few
/// pragmas and two meta reads.
///
/// - The file is still in `auto_vacuum = NONE`: compact once to switch it, so
///   from then on syncs hand freed pages back on their own.
/// - Over a tenth of the file is free pages (only possible without incremental
///   auto-vacuum, or after a large forget).
/// - It is a week or more since the last compaction and the file has grown by
///   a fifth since - churn that leaves the search index fragmented beyond what
///   the per-sync merge keeps up with - or a month has passed regardless.
pub fn compact_due(conn: &Connection, path: &std::path::Path) -> Result<Option<&'static str>> {
    let size = db_and_wal_size(path);
    if size < COMPACT_MIN_BYTES {
        return Ok(None);
    }
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    if mode != 2 {
        return Ok(Some("switch to incremental auto-vacuum"));
    }
    let free: i64 = conn.pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    let pages: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    if pages > 0 && free * 10 > pages {
        return Ok(Some("over 10% of the file is free pages"));
    }
    let meta = |key: &str| -> Result<Option<i64>> {
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .and_then(|v| v.parse().ok()))
    };
    let Some(at) = meta(COMPACT_AT_KEY)? else {
        // Born incremental (or converted by an older binary): start the clock.
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES (?1, ?2), (?3, ?4)",
            params![
                COMPACT_AT_KEY,
                chrono::Utc::now().timestamp().to_string(),
                COMPACT_SIZE_KEY,
                size.to_string()
            ],
        )?;
        return Ok(None);
    };
    let days = (chrono::Utc::now().timestamp() - at) / 86_400;
    let base = meta(COMPACT_SIZE_KEY)?.unwrap_or(size as i64).max(1) as u64;
    if days >= 30 {
        return Ok(Some("monthly compaction"));
    }
    if days >= 7 && size * 5 >= base * 6 {
        return Ok(Some("the index grew by a fifth since the last compaction"));
    }
    Ok(None)
}

/// External-content FTS5 requires handing back the old rows on delete.
fn delete_session_msgs(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO msgs(msgs, rowid, text)
         SELECT 'delete', id, text FROM messages WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM messages WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(())
}

/// Clear a session's derived provenance - `touched` AND `edits` together - so a
/// delete site can never remember one and forget the other (they drifted once,
/// leaving orphaned edit rows visible through `edits_for`).
fn delete_session_provenance(conn: &Connection, session_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM touched WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM edits WHERE session_id = ?1",
        params![session_id],
    )?;
    conn.execute(
        "DELETE FROM session_commits WHERE session_id = ?1",
        params![session_id],
    )?;
    Ok(())
}

/// Reconcile the index with a tool's store after discovery. Sessions whose
/// original file disappeared are **archived** (kept in the durable `archive`
/// table and flagged in `files`, with messages/touched left in place so
/// search and trace keep working) instead of deleted - unless
/// `SESSIONWIKI_NO_ARCHIVE` is set or the session has no indexed content, in
/// which case they are pruned as before. Returns how many were newly archived.
///
/// Guard: if the store root is gone (uninstalled, unmounted), do not touch its
/// sessions - that is "the whole store vanished", not "the tool pruned some".
/// An existing-but-empty store is a legitimate prune-everything and proceeds.
fn archive_or_prune(
    conn: &Connection,
    tool: &str,
    seen: &[String],
    store_present: bool,
    scope: Option<&str>,
) -> Result<usize> {
    let no_archive = std::env::var_os("SESSIONWIKI_NO_ARCHIVE").is_some();
    // A scoped adapter speaks only for the keys under its prefix; everything
    // else under the same tool name belongs to another store and must be left
    // alone. Filtering happens in Rust, not with SQL `LIKE`: keys are paths and
    // `_` is a LIKE wildcard.
    let in_scope = |key: &str| scope.is_none_or(|p| key.starts_with(p));
    let seen_set: std::collections::HashSet<&str> = seen
        .iter()
        .map(String::as_str)
        .filter(|k| in_scope(k))
        .collect();

    let mut stmt =
        conn.prepare("SELECT path, session_id FROM files WHERE tool = ?1 AND archived_at IS NULL")?;
    let all_live: Vec<(String, String)> = stmt
        .query_map(params![tool], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    // A session still live at a path discovery did return was not deleted; the
    // gone row is a second spelling of it, and archiving it would keep both.
    let still_live: std::collections::HashSet<String> = all_live
        .iter()
        .filter(|(p, _)| seen_set.contains(p.as_str()))
        .map(|(_, sid)| sid.clone())
        .collect();
    // A web export keys its chats by file, so the store lists one conversation
    // at the newest of the files that hold it. The other rows are not sessions
    // that went away, and their marks belong to the id the chat answers to now.
    let listed_by_native: std::collections::HashMap<String, String> = if tool.ends_with("-web") {
        all_live
            .iter()
            .filter(|(p, _)| seen_set.contains(p.as_str()))
            .filter_map(|(p, sid)| {
                crate::adapters::web_chat::native_part_of_key(p)
                    .map(|native| (native.to_string(), sid.clone()))
            })
            .collect()
    } else {
        std::collections::HashMap::new()
    };
    let live: Vec<(String, String)> = all_live.into_iter().filter(|(p, _)| in_scope(p)).collect();
    let gone: Vec<(String, String)> = live
        .into_iter()
        .filter(|(p, _)| !seen_set.contains(p.as_str()))
        .collect();
    if gone.is_empty() {
        return Ok(0);
    }
    if !store_present {
        eprintln!(
            "[{tool}] store not found - skipping ({} indexed session(s) left untouched, not archived)",
            gone.len()
        );
        return Ok(0);
    }
    // The root exists but discovery returned nothing while we still had live
    // sessions: could be a legitimate prune-everything, but also a transient
    // read failure (permissions, a half-mounted network FS). Archiving keeps
    // the data (reversible on the next good sync), but say so loudly.
    if seen_set.is_empty() {
        eprintln!(
            "[{tool}] no sessions found on disk but {} were indexed - archiving them; \
             if the store is just unreadable right now, they will un-archive on the next sync",
            gone.len()
        );
    }

    let mut archived = 0usize;
    for (path, sid) in gone {
        let moved_to = crate::adapters::web_chat::native_part_of_key(&path)
            .and_then(|native| listed_by_native.get(native))
            .filter(|live_sid| **live_sid != sid)
            .cloned();
        if still_live.contains(&sid) {
            conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
        } else if let Some(current_sid) = moved_to {
            // The chat was re-exported into a newer file: drop this listing and
            // let the marks follow the conversation, rather than archiving a
            // second copy of something the store still has.
            conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
            for table in ["tags", "notes", "summaries", "favorites", "snippets"] {
                conn.execute(
                    &format!("UPDATE OR IGNORE {table} SET session_id = ?2 WHERE session_id = ?1"),
                    params![&sid, &current_sid],
                )?;
            }
            // The superseded export is usually still on disk - the store just
            // prefers the newer one - so its transcript waits with it: a newer
            // export that fails to parse must not cost the conversation. Once
            // that file is gone as well, these rows describe a session the index
            // already keeps under another id.
            let still_on_disk = crate::adapters::web_chat::file_part_of_key(&path)
                .is_some_and(|file| std::path::Path::new(file).exists());
            let listed_elsewhere: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM files WHERE session_id = ?1)",
                params![&sid],
                |r| r.get(0),
            )?;
            if !still_on_disk && !listed_elsewhere {
                delete_session_msgs(conn, &sid)?;
                delete_session_provenance(conn, &sid)?;
                conn.execute("DELETE FROM archive WHERE session_id = ?1", params![&sid])?;
            }
        } else if no_archive || !has_messages(conn, &sid)? {
            conn.execute("DELETE FROM files WHERE path = ?1", params![path])?;
            delete_session_msgs(conn, &sid)?;
            delete_session_provenance(conn, &sid)?;
        } else {
            archive_session(conn, &path, &sid)?;
            archived += 1;
        }
    }
    Ok(archived)
}

/// Nothing indexed means nothing to keep: an archived copy would hold a title
/// and no transcript.
fn has_messages(conn: &Connection, sid: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id = ?1)",
        params![sid],
        |r| r.get(0),
    )?)
}

/// Copy a session whose original file is gone into the durable `archive` table
/// and flag its `files` row. The messages/msgs/touched rows are left in place
/// so search and `trace` keep working; the archive copy is the rebuild-survival
/// backup (replayed by `rehydrate_archive` after a schema bump).
fn archive_session(conn: &Connection, path: &str, sid: &str) -> Result<()> {
    let mut s = conn
        .prepare("SELECT role, text, full_text, COALESCE(is_rewound, 0) FROM messages WHERE session_id = ?1 ORDER BY id")?;
    let rows: Vec<(String, String, Option<String>, i32)> = s
        .query_map(params![sid], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(s);
    let transcript: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|(role, text, full_text, is_rewound)| {
            if is_rewound != 0 {
                serde_json::json!([role, text, full_text, true])
            } else {
                match full_text {
                    Some(full_text) => serde_json::json!([role, text, full_text]),
                    None => serde_json::json!([role, text]),
                }
            }
        })
        .collect();
    let mut s = conn.prepare("SELECT path FROM touched WHERE session_id = ?1 ORDER BY rowid")?;
    let touched: Vec<String> = s
        .query_map(params![sid], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    drop(s);
    let transcript_json = serde_json::to_string(&transcript)?;
    let touched_json = serde_json::to_string(&touched)?;
    conn.execute(
        "INSERT OR REPLACE INTO archive
         (session_id, path, mtime, size, tool, project, title, started, ended,
          msg_count, kind, transcript, touched, archived_at)
         SELECT session_id, path, mtime, size, tool, project, title, started, ended,
                msg_count, kind, ?2, ?3, datetime('now')
         FROM files WHERE path = ?1",
        params![path, transcript_json, touched_json],
    )?;
    conn.execute(
        "UPDATE files SET archived_at = datetime('now') WHERE path = ?1",
        params![path],
    )?;
    Ok(())
}

/// Serializes to the agent-facing JSON contract: snake_case keys matching the
/// web API (`id`, `msgs`, tags as an array). The absolute `path` is never
/// serialized verbatim - only the tool's own `native_id` (the codex rollout /
/// claude transcript UUID extracted from the filename) is exposed, so an agent
/// can join a harness "tower" row (which knows only the native id) back to a
/// session without the local path ever leaking.
#[derive(Serialize)]
pub struct SessionRow {
    #[serde(rename = "id")]
    pub session_id: String,
    pub tool: String,
    /// The NATIVE session file path. Never serialized as-is; it is surfaced only
    /// as the extracted `native_id` UUID (or null when the filename carries no
    /// UUID) via [`ser_native_id`].
    #[serde(rename = "native_id", serialize_with = "ser_native_id")]
    pub path: String,
    pub project: String,
    pub title: String,
    pub started: Option<String>,
    /// When the session last had a message. A session resumed for weeks began
    /// long ago; this is what makes it recent. Absent where it was not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_active: Option<String>,
    #[serde(rename = "msgs")]
    pub msg_count: i64,
    pub kind: String,
    /// Tail of the conversation (last assistant message), so a list can show
    /// how the session ended without opening it.
    pub preview: Option<String>,
    /// Cached LLM synopsis, if `summarize` has been run for this session.
    pub summary: Option<String>,
    /// Comma-joined user tags, if any. Serialized as a string array (or null).
    #[serde(serialize_with = "ser_tags")]
    pub tags: Option<String>,
    /// True if the tool deleted the original and we kept the indexed copy.
    pub archived: bool,
    /// The swapdex account profile active when this session started, when a
    /// swapdex switch timeline exists on the machine. Null otherwise - a
    /// missing badge, never a guess.
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// Tags are stored comma-joined but the JSON contract is an array (matching the
/// web API). Null when there are no tags.
fn ser_tags<S: serde::Serializer>(tags: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
    match tags {
        Some(t) => s.collect_seq(t.split(',')),
        None => s.serialize_none(),
    }
}

/// Serialize a session's stored `path` as its `native_id` only: the tool's own
/// session UUID (or null when the filename carries no UUID). The absolute path
/// is never emitted - only the extracted UUID reaches the JSON contract.
fn ser_native_id<S: serde::Serializer>(path: &str, s: S) -> Result<S::Ok, S::Error> {
    match native_id_of(path) {
        Some(id) => s.serialize_some(&id),
        None => s.serialize_none(),
    }
}

/// Extract the native session UUID embedded in a session file's path - the id
/// the originating tool (and a harness "tower") knows the session by: the Codex
/// rollout UUID (`rollout-<ts>-<uuid>.jsonl`) or the Claude Code transcript UUID
/// (`<uuid>.jsonl`, or `agent-<uuid>.jsonl` for a subagent). Returns the first
/// canonical 8-4-4-4-12 UUID found in the file NAME, lowercased, or None when the
/// filename carries no UUID (tools that key sessions differently). Scanning the
/// file name (not the whole path) keeps a codex timestamp or a parent directory
/// from being mistaken for the session's own id.
pub fn native_id_of(path: &str) -> Option<String> {
    let name = std::path::Path::new(path).file_name()?.to_string_lossy();
    find_uuid(&name)
}

/// The first canonical UUID (8-4-4-4-12 hex with dashes) appearing in `s`,
/// lowercased. None when there is no such substring.
fn find_uuid(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 36 {
        return None;
    }
    for start in 0..=b.len() - 36 {
        if is_uuid_bytes(&b[start..start + 36]) {
            return Some(s[start..start + 36].to_ascii_lowercase());
        }
    }
    None
}

/// Whether a 36-byte window is a canonical UUID: hex everywhere except dashes at
/// positions 8, 13, 18, 23.
fn is_uuid_bytes(b: &[u8]) -> bool {
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Whether `q` could be a native-id lookup (full UUID or a UUID prefix), as
/// opposed to a sessionwiki short id (always 12 hex chars, no dashes). A valid
/// UUID prefix longer than its first 8-hex group must carry a dash (position 8
/// is always `-`), so a plain-hex string of 9+ chars can only be a short id and
/// never triggers the native scan - which keeps short-id resolution unchanged.
fn looks_like_native_prefix(q: &str) -> bool {
    let len = q.len();
    if !(4..=36).contains(&len) {
        return false;
    }
    if !q.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
        return false;
    }
    // Plain hex, no dash: only a first-group prefix (<= 8 chars) can be a UUID.
    q.contains('-') || len <= 8
}

/// Correlated subquery for the preview column; messages.id preserves
/// insertion order, which is message order.
const PREVIEW_SQL: &str = "(SELECT substr(m2.text, 1, 280) FROM messages m2
    WHERE m2.session_id = f.session_id AND m2.role = 'assistant'
    ORDER BY m2.id DESC LIMIT 1)";

const SUMMARY_SQL: &str = "(SELECT s.summary FROM summaries s WHERE s.session_id = f.session_id)";

const TAGS_SQL: &str =
    "(SELECT group_concat(t.tag, ',') FROM tags t WHERE t.session_id = f.session_id)";

/// Session-level filters for a listing, shared by [`recent`] and
/// [`recent_range`] so callers add one filter without touching every signature.
#[derive(Debug, Clone, Copy, Default)]
pub struct ListQuery<'a> {
    pub tool: Option<&'a str>,
    pub project: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub since: Option<&'a str>,
    pub until: Option<&'a str>,
    /// Keep subagent sessions; listings hide them by default.
    pub include_subagents: bool,
    /// Only these `files.project` values (see [`project_folders`]). `project`
    /// treats a repository and its subfolders as one project; this is for a view
    /// of one folder, which must not lose rows to a limit applied to the whole
    /// repository first.
    pub folders: Option<&'a [String]>,
}

pub fn recent(
    conn: &Connection,
    limit: usize,
    tool: Option<&str>,
    project: Option<&str>,
    tag: Option<&str>,
    include_subagents: bool,
) -> Result<Vec<SessionRow>> {
    recent_range(
        conn,
        limit,
        &ListQuery {
            tool,
            project,
            tag,
            include_subagents,
            ..Default::default()
        },
        SessionSort::LastActive,
    )
}

/// Order of a session listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSort {
    /// Most recently active first (a session resumed today is recent).
    LastActive,
    /// Most recently started first.
    Created,
    /// Most messages first, most recently active among equals.
    Messages,
}

impl SessionSort {
    pub fn parse(value: &str) -> Self {
        match value {
            "created" => Self::Created,
            "msgs" => Self::Messages,
            _ => Self::LastActive,
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            Self::LastActive => "COALESCE(f.ended, f.started) DESC",
            Self::Created => "f.started DESC",
            Self::Messages => "f.msg_count DESC, COALESCE(f.ended, f.started) DESC",
        }
    }
}

/// A `since` bound in the stored form (UTC RFC 3339), so a timestamp from a
/// browser (`...T21:00:00.000Z`) compares as text with the stored
/// `...T21:00:00+00:00`. A bare date stays as is: it sorts before that day.
fn since_bound(value: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(t) => t
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false),
        Err(_) => value.to_string(),
    }
}

/// An inclusive `until` bound: a bare date covers that whole UTC day, and a
/// timestamp keeps full precision so a session started in its last second is in.
fn until_bound(value: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(value) {
        Ok(t) => t
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),
        Err(_) if value.len() == 10 => format!("{value}T23:59:59.999999999Z"),
        Err(_) => value.to_string(),
    }
}

/// Restrict `f` (a `files` row) to sessions active within [since, until]:
/// started no later than `until` and last active no earlier than `since`.
fn push_date_scope(
    sql: &mut String,
    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
    since: Option<&str>,
    until: Option<&str>,
) {
    if let Some(s) = since {
        sql.push_str(" AND COALESCE(f.ended, f.started) >= ?");
        args.push(Box::new(since_bound(s)));
    }
    if let Some(u) = until {
        sql.push_str(" AND f.started <= ?");
        args.push(Box::new(until_bound(u)));
    }
}

/// The session-level filters every listing and search mode shares, on `f`.
fn push_session_scope(
    sql: &mut String,
    args: &mut Vec<Box<dyn rusqlite::ToSql>>,
    filters: &SearchFilters<'_>,
) {
    if let Some(t) = filters.tool {
        sql.push_str(" AND f.tool = ?");
        args.push(Box::new(t.to_string()));
    }
    if let Some(p) = filters.project {
        push_project_scope(sql, args, p);
    }
    if let Some(t) = filters.tag {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM tags g WHERE g.session_id = f.session_id AND g.tag = ?)",
        );
        // Stored tags are normalized; match their form.
        args.push(Box::new(norm_tag(t)));
    }
    push_date_scope(sql, args, filters.since, filters.until);
}

/// Restrict `f` to one project.
///
/// `files.project_key` is the repository a session belongs to, spelled one way
/// for every tool, while the filter is the folder a human typed. Either can be
/// the wider path, so the test runs in both directions: the same key, a key
/// filed under it (a session started in a subfolder of the repo), or a query
/// that points into the stored folder. `LIKE` is used where the indexed column
/// is on the left, `instr` where the pattern would have to come from the row.
///
/// A query that is not a path (`api`, a fragment) keeps the substring match it
/// always had, and a row whose key has not been filled yet - the backfill
/// defers when the index is busy - still matches through `f.project`.
fn push_project_scope(sql: &mut String, args: &mut Vec<Box<dyn rusqlite::ToSql>>, project: &str) {
    let raw = project.trim();
    let key = crate::util::project_key(raw);
    if !raw.contains(['/', '\\', ':']) || key.is_empty() {
        sql.push_str(" AND (f.project_key LIKE ? ESCAPE '\\' OR f.project LIKE ? ESCAPE '\\')");
        let pattern = format!("%{}%", escape_like(&crate::util::nfc(raw)));
        args.push(Box::new(pattern.clone()));
        args.push(Box::new(pattern));
        return;
    }
    sql.push_str(
        " AND (f.project_key = ?
                  OR f.project_key LIKE ? ESCAPE '\\'
                  OR (f.project_key != '' AND instr(?, f.project_key || '/') = 1)
                  OR (f.project_key = '' AND f.project LIKE ? ESCAPE '\\'))",
    );
    args.push(Box::new(key.clone()));
    args.push(Box::new(format!("{}/%", escape_like(&key))));
    args.push(Box::new(key.clone()));
    args.push(Box::new(format!("%{}%", escape_like(&key))));
}

pub fn recent_range(
    conn: &Connection,
    limit: usize,
    query: &ListQuery<'_>,
    sort: SessionSort,
) -> Result<Vec<SessionRow>> {
    let ListQuery {
        tool,
        project,
        tag,
        since,
        until,
        include_subagents,
        folders,
    } = *query;
    let mut sql = format!(
        "SELECT session_id, tool, path, project, title, started, msg_count, kind, {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL), COALESCE(ended, started), parent_id
         FROM files f WHERE 1=1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    // A tag filter is an explicit ask for *those* sessions; don't hide subagent
    // hits behind the main-only default (the tag cloud counts every kind, so a
    // sub-only tag would otherwise show in the cloud but return nothing here).
    if !include_subagents && tag.is_none() {
        sql.push_str(" AND kind = 'main'");
    }
    push_session_scope(
        &mut sql,
        &mut args,
        &SearchFilters {
            tool,
            project,
            roles: None,
            tag,
            since,
            until,
        },
    );
    if let Some(folders) = folders {
        if folders.is_empty() {
            sql.push_str(" AND 0");
        } else {
            let marks = vec!["?"; folders.len()].join(",");
            sql.push_str(&format!(" AND f.project IN ({marks})"));
            for folder in folders {
                args.push(Box::new(folder.clone()));
            }
        }
    }
    // Recent means recently ACTIVE by default: a session resumed for weeks
    // began long ago, and ordering by its start buried it below everything
    // begun since. Sorting here rather than in the caller makes the "created"
    // and "most messages" orders cover every session, not the newest `limit`.
    // Both columns are UTC RFC 3339, so they order as text.
    sql.push_str(&format!(" ORDER BY {} LIMIT {limit}", sort.order_by()));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
        |r| {
            Ok(SessionRow {
                last_active: r.get(12)?,
                session_id: r.get(0)?,
                tool: r.get(1)?,
                path: r.get(2)?,
                project: r.get(3)?,
                title: r.get(4)?,
                started: r.get(5)?,
                msg_count: r.get(6)?,
                kind: r.get(7)?,
                preview: r.get(8)?,
                summary: r.get(9)?,
                tags: r.get(10)?,
                archived: r.get(11)?,
                parent_id: r.get(13)?,
                account: None,
            })
        },
    )?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut());
    Ok(out)
}

/// Recent main sessions whose launch project is this directory or the
/// repository around it (for the SessionStart recall hook). Matched on
/// `project_key` - the folder folded up to the repository root and spelled one
/// way for every tool - with the old exact `project` comparison kept beside it
/// so a row whose key has not been filled yet is not lost. Never the substring
/// `--project` filter, which over-matches sibling paths. Newest first, stable.
pub fn project_brief(conn: &Connection, project: &str, limit: usize) -> Result<Vec<SessionRow>> {
    let key = crate::util::project_key(project);
    let native = crate::util::project_key_as_on_disk(project.trim_end_matches('/'));
    let mut stmt = conn.prepare(&format!(
        "SELECT session_id, tool, path, project, title, started, msg_count, kind, {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)
         FROM files f
         WHERE (f.project_key = ?1 OR f.project = ?2) AND f.kind = 'main'
         ORDER BY f.started DESC, f.session_id
         LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![key, native, limit as i64], |r| {
        Ok(SessionRow {
            last_active: None,
            session_id: r.get(0)?,
            tool: r.get(1)?,
            path: r.get(2)?,
            project: r.get(3)?,
            title: r.get(4)?,
            started: r.get(5)?,
            msg_count: r.get(6)?,
            kind: r.get(7)?,
            preview: r.get(8)?,
            summary: r.get(9)?,
            tags: r.get(10)?,
            archived: r.get(11)?,
            parent_id: None,
            account: None,
        })
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut());
    Ok(out)
}

pub struct Hit {
    pub row: SessionRow,
    pub role: String,
    pub snippet: String,
}

#[derive(Clone)]
struct SearchTerm {
    text: String,
}

/// Split user text into whitespace-separated terms, keeping quoted text as one
/// phrase. An unmatched opening quote consumes the remainder of the query.
fn parse_search_terms(query: &str) -> Vec<SearchTerm> {
    fn push(terms: &mut Vec<SearchTerm>, text: String) {
        if !text.trim().is_empty() {
            terms.push(SearchTerm { text });
        }
    }

    let normalized = crate::util::nfc(query);
    let mut chars = normalized.chars().peekable();
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut phrase = String::new();
    let mut in_phrase = false;

    while let Some(ch) = chars.next() {
        if in_phrase {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    phrase.push('"');
                } else {
                    push(&mut terms, std::mem::take(&mut phrase));
                    in_phrase = false;
                }
            } else {
                phrase.push(ch);
            }
        } else if ch == '"' {
            if !current.is_empty() {
                push(&mut terms, std::mem::take(&mut current));
            }
            in_phrase = true;
        } else if ch.is_whitespace() {
            if !current.is_empty() {
                push(&mut terms, std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if in_phrase {
        push(&mut terms, phrase);
    } else if !current.is_empty() {
        push(&mut terms, current);
    }
    terms
}

/// Return an MCP query rejection, if any. Trigram would otherwise let a remote
/// client trigger the bounded LIKE scan with a query made only of short terms;
/// word tokenizers can handle those terms directly in FTS.
pub fn mcp_search_guard(conn: &Connection, query: &str) -> Result<Option<&'static str>> {
    let terms = parse_search_terms(query);
    if terms.is_empty() {
        return Ok(Some("query must contain at least one search term"));
    }
    if is_trigram_tokenizer(&tokenizer_spec(conn)?)
        && !terms.iter().any(|term| term.text.chars().count() >= 3)
    {
        return Ok(Some(
            "trigram search requires at least one term of 3 or more characters",
        ));
    }
    Ok(None)
}

fn is_trigram_tokenizer(spec: &str) -> bool {
    spec.split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("trigram"))
}

/// Search each message for every query term, then return its best session hit.
/// Terms are ANDed in one message; double quotes keep their contents together
/// as a phrase. Trigram supports substring and CJK searches; other configured
/// tokenizers use their own word-search behavior.
pub fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    tool: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<Hit>> {
    search_with_roles(conn, query, limit, tool, project, None)
}

/// Search messages and return each session's best matching hit, optionally
/// restricted to the supplied message roles. The role restriction is applied
/// before the candidate cap and before sessions are grouped.
pub fn search_with_roles(
    conn: &Connection,
    query: &str,
    limit: usize,
    tool: Option<&str>,
    project: Option<&str>,
    roles: Option<&[Role]>,
) -> Result<Vec<Hit>> {
    search_scoped(
        conn,
        query,
        limit,
        tool,
        project,
        SearchScope {
            roles,
            ..SearchScope::default()
        },
    )
}

/// [`search_with_roles`] with the date and tag limits applied in SQL, before
/// the result limit - so a narrow window still gets `limit` hits, not the
/// share of the corpus-wide top hits that happens to fall inside it.
pub fn search_scoped(
    conn: &Connection,
    query: &str,
    limit: usize,
    tool: Option<&str>,
    project: Option<&str>,
    scope: SearchScope<'_>,
) -> Result<Vec<Hit>> {
    let roles = scope.roles;
    if roles.is_some_and(|roles| roles.is_empty()) {
        return Ok(Vec::new());
    }
    let filters = SearchFilters {
        tool,
        project,
        roles,
        tag: scope.tag,
        since: scope.since,
        until: scope.until,
    };
    let terms = parse_search_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let trigram = is_trigram_tokenizer(&tokenizer_spec(conn)?);
    if trigram && terms.iter().all(|term| term.text.chars().count() < 3) {
        return search_like_terms(conn, &terms, limit, filters);
    }
    let fts_terms: Vec<SearchTerm> = if trigram {
        terms
            .iter()
            .filter(|term| term.text.chars().count() >= 3)
            .cloned()
            .collect()
    } else {
        // unicode61, porter, and ascii tokenize punctuation as separators. A
        // punctuation-only phrase has no FTS tokens and must not form an empty
        // MATCH phrase. Dropping it also makes an all-punctuation query empty.
        terms
            .iter()
            .filter(|term| term.text.chars().any(char::is_alphanumeric))
            .cloned()
            .collect()
    };
    if fts_terms.is_empty() {
        return Ok(Vec::new());
    }
    search_fts_terms(conn, &terms, &fts_terms, trigram, limit, filters)
}

#[derive(Clone, Copy)]
struct SearchFilters<'a> {
    tool: Option<&'a str>,
    project: Option<&'a str>,
    roles: Option<&'a [Role]>,
    tag: Option<&'a str>,
    since: Option<&'a str>,
    until: Option<&'a str>,
}

/// Session-level limits for [`search_scoped`] beyond tool and project.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchScope<'a> {
    pub roles: Option<&'a [Role]>,
    /// Only sessions carrying this tag.
    pub tag: Option<&'a str>,
    /// Only sessions active at or after this (date or RFC 3339 timestamp).
    pub since: Option<&'a str>,
    /// Only sessions started at or before this (date or RFC 3339 timestamp).
    pub until: Option<&'a str>,
}

fn search_fts_terms(
    conn: &Connection,
    terms: &[SearchTerm],
    fts_terms: &[SearchTerm],
    trigram: bool,
    limit: usize,
    filters: SearchFilters<'_>,
) -> Result<Vec<Hit>> {
    let fts_query = fts_terms
        .iter()
        .map(|term| format!("\"{}\"", term.text.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ");

    // snippet()/rank only work in a plain FTS5 query context, not under joins
    // or GROUP BY, so match in a subquery and attach metadata outside.
    //
    // Tradeoff: only the top 4000 matching messages by rank reach grouping.
    // A very common query can omit sessions whose matches rank below that cap;
    // narrowing the terms surfaces the long tail.
    let mut sql = String::from(
        "SELECT f.session_id, f.tool, f.path, f.project, f.title, f.started, f.msg_count, f.kind,
                m.role, x.snip, min(x.rank) AS best, (f.archived_at IS NOT NULL), COALESCE(f.ended, f.started)
         FROM (SELECT rowid AS mid,
                      snippet(msgs, 0, char(2), char(3), char(8230), 18) AS snip,
                      rank
               FROM msgs WHERE msgs MATCH ?",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(fts_query)];
    if let Some(roles) = filters.roles {
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM messages role_message
                          WHERE role_message.id = msgs.rowid AND role_message.role IN (",
        );
        for (index, role) in roles.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
            args.push(Box::new(role.label().to_string()));
        }
        sql.push_str("))");
    }
    sql.push_str(
        " ORDER BY rank LIMIT 4000) x
         JOIN messages m ON m.id = x.mid
         JOIN files f ON f.session_id = m.session_id
         WHERE 1=1",
    );
    if trigram {
        for term in terms.iter().filter(|term| term.text.chars().count() < 3) {
            sql.push_str(" AND m.text LIKE ? ESCAPE '\\'");
            args.push(Box::new(format!("%{}%", escape_like(&term.text))));
        }
    }
    push_session_scope(&mut sql, &mut args, &filters);
    sql.push_str(&format!(
        " GROUP BY f.session_id ORDER BY best LIMIT {limit}"
    ));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(args.iter().map(|arg| arg.as_ref())),
        |r| {
            Ok(Hit {
                row: SessionRow {
                    last_active: r.get(12)?,
                    session_id: r.get(0)?,
                    tool: r.get(1)?,
                    path: r.get(2)?,
                    project: r.get(3)?,
                    title: r.get(4)?,
                    started: r.get(5)?,
                    msg_count: r.get(6)?,
                    kind: r.get(7)?,
                    preview: None,
                    summary: None,
                    tags: None,
                    archived: r.get(11)?,
                    account: None,
                    parent_id: None,
                },
                role: r.get(8)?,
                snippet: r.get(9)?,
            })
        },
    )?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut().map(|hit| &mut hit.row));
    Ok(out)
}

/// Find sessions where all query terms are present across any of the session's messages
/// (multi-term intersection). Used when multiple terms are searched and they might appear
/// in different turns of the same conversation.
pub fn search_sessions_multi_terms(
    conn: &Connection,
    terms: &[&str],
    limit: usize,
    tool: Option<&str>,
    scope: SearchScope<'_>,
) -> Result<Vec<SessionRow>> {
    if terms.is_empty() || scope.roles.is_some_and(|r| r.is_empty()) {
        return Ok(Vec::new());
    }
    // One CTE per term: the sessions with a message matching it, and how many.
    let mut ctes = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for (i, term) in terms.iter().enumerate() {
        let clean = term.trim_matches('"').replace('"', "\"\"");
        let mut cte = format!(
            "t{i} AS (SELECT m.session_id AS sid, count(*) AS c FROM messages m \
             WHERE m.id IN (SELECT rowid FROM msgs WHERE msgs MATCH ?)"
        );
        params.push(Box::new(format!("\"{clean}\"")));
        if let Some(roles) = scope.roles {
            cte.push_str(" AND m.role IN (");
            for (j, role) in roles.iter().enumerate() {
                if j > 0 {
                    cte.push_str(", ");
                }
                cte.push('?');
                params.push(Box::new(role.label().to_string()));
            }
            cte.push(')');
        }
        cte.push_str(" GROUP BY m.session_id)");
        ctes.push(cte);
    }
    let joins: String = (0..terms.len())
        .map(|i| format!(" JOIN t{i} ON t{i}.sid = f.session_id"))
        .collect();
    let counts: Vec<String> = (0..terms.len()).map(|i| format!("t{i}.c")).collect();
    // Relevance: sessions where even the rarest term comes up often rank
    // first, then by total matches, then by recency.
    let weakest = if counts.len() == 1 {
        counts[0].clone()
    } else {
        format!("min({})", counts.join(", "))
    };
    let mut sql = format!(
        "WITH {} SELECT f.session_id, f.tool, f.path, f.project, f.title, f.started, f.msg_count, f.kind, \
                (f.archived_at IS NOT NULL), COALESCE(f.ended, f.started), {TAGS_SQL} \
         FROM files f{joins} WHERE 1=1",
        ctes.join(", ")
    );
    push_session_scope(
        &mut sql,
        &mut params,
        &SearchFilters {
            tool,
            project: None,
            roles: None,
            tag: scope.tag,
            since: scope.since,
            until: scope.until,
        },
    );
    sql.push_str(&format!(
        " ORDER BY {weakest} DESC, ({}) DESC, COALESCE(f.ended, f.started) DESC LIMIT ?",
        counts.join(" + ")
    ));
    params.push(Box::new(limit as i64));

    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(param_refs.as_slice(), |r| {
        Ok(SessionRow {
            session_id: r.get(0)?,
            tool: r.get(1)?,
            path: r.get(2)?,
            project: r.get(3)?,
            title: r.get(4)?,
            started: r.get(5)?,
            last_active: r.get(9)?,
            msg_count: r.get(6)?,
            kind: r.get(7)?,
            preview: None,
            summary: None,
            tags: r.get(10)?,
            archived: r.get(8)?,
            parent_id: None,
            account: None,
        })
    })?;
    let mut out = Vec::new();
    for r in rows.flatten() {
        out.push(r);
    }
    crate::account_link::annotate(out.iter_mut());
    Ok(out)
}

/// Substring search for terms below the trigram tokenizer's three-character
/// floor. All terms must match the same messages.text row.
///
/// Perf: SQLite scans messages to find matching rows, then the newest 50,000
/// matches reach session grouping. A session with no match among those newest
/// candidates can be missed. With trigram, the unified search dispatcher uses
/// this only when every term is short; any longer term stays on FTS. Other
/// tokenizer specs use FTS for short terms too. LIKE has no rank, so results
/// are ordered by recency.
pub fn search_like(
    conn: &Connection,
    query: &str,
    limit: usize,
    tool: Option<&str>,
    project: Option<&str>,
) -> Result<Vec<Hit>> {
    let terms = parse_search_terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    search_like_terms(
        conn,
        &terms,
        limit,
        SearchFilters {
            tool,
            project,
            roles: None,
            tag: None,
            since: None,
            until: None,
        },
    )
}

fn search_like_terms(
    conn: &Connection,
    terms: &[SearchTerm],
    limit: usize,
    filters: SearchFilters<'_>,
) -> Result<Vec<Hit>> {
    const SCAN_CAP: i64 = 50_000;

    let mut sql = String::from(
        "SELECT f.session_id, f.tool, f.path, f.project, f.title, f.started, f.msg_count, f.kind,
                x.role, x.text, (f.archived_at IS NOT NULL), COALESCE(f.ended, f.started)
         FROM (SELECT m.session_id AS sid, m.role AS role, m.text AS text, m.id AS mid
               FROM messages m WHERE 1=1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(roles) = filters.roles {
        sql.push_str(" AND m.role IN (");
        for (index, role) in roles.iter().enumerate() {
            if index > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
            args.push(Box::new(role.label().to_string()));
        }
        sql.push(')');
    }
    for term in terms {
        sql.push_str(" AND m.text LIKE ? ESCAPE '\\'");
        args.push(Box::new(format!("%{}%", escape_like(&term.text))));
    }
    sql.push_str(" ORDER BY m.id DESC LIMIT ?)");
    args.push(Box::new(SCAN_CAP));
    sql.push_str(
        " x JOIN files f ON f.session_id = x.sid
         WHERE 1=1",
    );
    push_session_scope(&mut sql, &mut args, &filters);
    sql.push_str(&format!(
        " GROUP BY f.session_id ORDER BY max(x.mid) DESC LIMIT {limit}"
    ));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(args.iter().map(|arg| arg.as_ref())),
        |r| {
            let text: String = r.get(9)?;
            Ok(Hit {
                row: SessionRow {
                    last_active: r.get(11)?,
                    session_id: r.get(0)?,
                    tool: r.get(1)?,
                    path: r.get(2)?,
                    project: r.get(3)?,
                    title: r.get(4)?,
                    started: r.get(5)?,
                    msg_count: r.get(6)?,
                    kind: r.get(7)?,
                    preview: None,
                    summary: None,
                    tags: None,
                    archived: r.get(10)?,
                    account: None,
                    parent_id: None,
                },
                role: r.get(8)?,
                snippet: snippet_around(&text, terms),
            })
        },
    )?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut().map(|hit| &mut hit.row));
    Ok(out)
}

/// Build a short raw-text window around the first LIKE term, highlighting every
/// term occurrence in that window. Matching folds case, while a character map
/// keeps highlights aligned to the unmodified transcript.
fn snippet_around(text: &str, terms: &[SearchTerm]) -> String {
    const WINDOW: usize = 36;
    let chars: Vec<char> = text.chars().collect();
    let mut folded = Vec::new();
    let mut source = Vec::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        for lower in ch.to_lowercase() {
            folded.push(lower);
            source.push((index, index + 1));
        }
    }

    let mut matches = Vec::new();
    for term in terms {
        let needle: Vec<char> = term.text.chars().flat_map(char::to_lowercase).collect();
        if needle.is_empty() || needle.len() > folded.len() {
            continue;
        }
        for start in 0..=folded.len() - needle.len() {
            let end = start + needle.len();
            if folded[start..end] == needle[..] {
                matches.push((source[start].0, source[end - 1].1));
            }
        }
    }

    let Some(&(anchor_start, anchor_end)) = matches.iter().min_by_key(|range| range.0) else {
        return chars
            .iter()
            .take(WINDOW * 2)
            .collect::<String>()
            .replace('\n', " ");
    };
    let lo = anchor_start.saturating_sub(WINDOW);
    let hi = (anchor_end + WINDOW).min(chars.len());
    matches.retain(|(start, end)| *start >= lo && *end <= hi);
    matches.sort_unstable();

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (start, end) in matches {
        if let Some(previous) = ranges.last_mut() {
            if start < previous.1 {
                previous.1 = previous.1.max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    let mut out = String::new();
    if lo > 0 {
        out.push('\u{2026}');
    }
    let mut cursor = lo;
    for (start, end) in ranges {
        out.extend(&chars[cursor..start]);
        out.push('\u{2}');
        out.extend(&chars[start..end]);
        out.push('\u{3}');
        cursor = end;
    }
    out.extend(&chars[cursor..hi]);
    if hi < chars.len() {
        out.push('\u{2026}');
    }
    out.replace('\n', " ")
}

/// A path as the path lookups compare it. On Windows `\` cannot be part of a
/// name and tools record `C:\x\y` and `C:/x/y` alike, so it reads as `/`;
/// elsewhere it is an ordinary character in a name.
fn path_query(query: &str) -> String {
    let q = crate::util::nfc(query.trim());
    let q = if cfg!(windows) {
        q.replace('\\', "/")
    } else {
        q
    };
    q.trim_start_matches("./").to_owned()
}

/// A stored path column, read the way [`path_query`] reads a query.
fn path_col(col: &str) -> String {
    if cfg!(windows) {
        format!("replace({col}, char(92), '/')")
    } else {
        col.to_owned()
    }
}

/// Escape LIKE metacharacters ('\' first) so an id/path fragment like "%" or
/// "_" can't turn a prefix match into a wildcard that resolves every session.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The SELECT column list every id-resolution query shares, in the order
/// [`map_resolve_row`] reads.
const RESOLVE_COLS: &str =
    "session_id, tool, path, project, title, started, msg_count, kind, parent_id";

/// Map a resolve-query row to a [`SessionRow`]. Callers must SELECT
/// [`RESOLVE_COLS`] followed by `{SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)`.
fn map_resolve_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        last_active: None,
        session_id: r.get(0)?,
        tool: r.get(1)?,
        path: r.get(2)?,
        project: r.get(3)?,
        title: r.get(4)?,
        started: r.get(5)?,
        msg_count: r.get(6)?,
        kind: r.get(7)?,
        parent_id: r.get(8)?,
        preview: None,
        summary: r.get(9)?,
        tags: r.get(10)?,
        archived: r.get(11)?,
        account: None,
    })
}

/// Resolve a (possibly abbreviated) session id to its file row(s). Matches on
/// the sessionwiki short id (prefix) AND on the tool's own native id (the codex
/// rollout / claude transcript UUID, full or prefix), so a harness "tower" row -
/// which knows only the native id - can be reopened directly. Short-id behavior
/// is unchanged: the native scan only runs for native-shaped queries, and never
/// displaces an existing short-id match on a plain-hex prefix (see
/// [`looks_like_native_prefix`]).
pub fn resolve(conn: &Connection, id_prefix: &str) -> Result<Vec<SessionRow>> {
    let mut out = resolve_by_short_id(conn, id_prefix)?;

    // Native-id join. A plain-hex prefix stays short-id-only when it already
    // matched something (no new ambiguity); a dashed prefix is unambiguously a
    // UUID (short ids have no dashes) so it always broadens to the native scan.
    let native_ok =
        looks_like_native_prefix(id_prefix) && (out.is_empty() || id_prefix.contains('-'));
    if native_ok {
        for row in resolve_by_native_id(conn, id_prefix)? {
            if out.len() >= 10 {
                break;
            }
            if out.iter().all(|r| r.session_id != row.session_id) {
                out.push(row);
            }
        }
    }
    Ok(out)
}

/// Prefix match on the sessionwiki short id (the historical `resolve`).
fn resolve_by_short_id(conn: &Connection, id_prefix: &str) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RESOLVE_COLS}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)
         FROM files f WHERE session_id LIKE ?1 ESCAPE '\\' LIMIT 10",
    ))?;
    let pattern = format!("{}%", escape_like(id_prefix));
    let rows = stmt.query_map(params![pattern], map_resolve_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Match rows whose native id (derived from the filename) starts with `prefix`.
/// The native id is a substring of the stored path, so a coarse `path LIKE
/// '%prefix%'` yields a bounded superset which we then confirm per row - a
/// planted path fragment can never satisfy the exact `native_id_of` check.
fn resolve_by_native_id(conn: &Connection, prefix: &str) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {RESOLVE_COLS}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)
         FROM files f WHERE path LIKE ?1 ESCAPE '\\' LIMIT 500",
    ))?;
    let pattern = format!("%{}%", escape_like(prefix));
    let want = prefix.to_ascii_lowercase();
    let rows = stmt.query_map(params![pattern], map_resolve_row)?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        if native_id_of(&row.path).is_some_and(|n| n.starts_with(&want)) {
            out.push(row);
            if out.len() >= 10 {
                break;
            }
        }
    }
    Ok(out)
}

/// Locate a session file directly on disk by its NATIVE id (codex rollout UUID
/// or claude transcript UUID), full or prefix, WITHOUT the index. This is the
/// live-session path: a session started moments ago may not be indexed yet, but
/// its file already exists under the tool's store root, so `session_window` /
/// `show` can still open it in one call. Scans only the codex and claude roots -
/// the two tools whose native id a harness tower knows - and returns the first
/// (tool, path) whose filename-derived native id matches. None if nothing on
/// disk matches (or the query is not native-shaped).
pub fn locate_by_native_id(prefix: &str) -> Option<(String, PathBuf)> {
    if !looks_like_native_prefix(prefix) {
        return None;
    }
    let want = prefix.to_ascii_lowercase();
    for name in ["claude-code", "codex"] {
        let Some(adapter) = adapters::by_name(name) else {
            continue;
        };
        let Some(root) = adapter.root() else { continue };
        if !root.exists() {
            continue;
        }
        let hit = walkdir::WalkDir::new(&root)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .find(|e| {
                e.file_type().is_file()
                    && e.path().extension().is_some_and(|x| x == "jsonl")
                    && native_id_of(&e.path().to_string_lossy())
                        .is_some_and(|n| n.starts_with(&want))
            });
        if let Some(e) = hit {
            return Some((name.to_string(), e.into_path()));
        }
    }
    None
}

/// A minimal, un-indexed [`SessionRow`] for a live session located on disk by
/// [`locate_by_native_id`]. The real path and tool drive a direct parse (via
/// `load_session`); the metadata fields are placeholders the window/show render
/// path does not consult (it reads the parsed transcript). The `session_id`
/// matches the id the adapter would assign, so a later sync reconciles cleanly.
pub fn live_row(tool: String, path: PathBuf) -> SessionRow {
    let path = path.to_string_lossy().into_owned();
    SessionRow {
        last_active: None,
        session_id: crate::util::short_id(&path),
        tool,
        path,
        project: String::new(),
        title: String::new(),
        started: None,
        msg_count: 0,
        kind: "main".into(),
        preview: None,
        summary: None,
        tags: None,
        archived: false,
        account: None,
        parent_id: None,
    }
}

/// Store (or replace) the cached synopsis for a session. The synopsis comes from
/// the user's own LLM over the raw transcript, so it can echo a secret - redact
/// before it lands in this durable table.
pub fn set_summary(conn: &Connection, session_id: &str, summary: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO summaries(session_id, summary, created, kind)
         VALUES (?1, ?2, datetime('now'), 'llm')",
        params![session_id, crate::redact::redact(summary).as_ref()],
    )?;
    Ok(())
}

/// Most recent main sessions that have no model-written summary yet (an
/// instant extractive one does not count: `summarize` replaces it).
pub fn unsummarized(
    conn: &Connection,
    limit: usize,
    tool: Option<&str>,
) -> Result<Vec<SessionRow>> {
    summary_candidates(conn, limit, tool, true)
}

/// Main sessions lacking a summary: a model-written one (`llm_only`), or
/// any at all.
fn summary_candidates(
    conn: &Connection,
    limit: usize,
    tool: Option<&str>,
    llm_only: bool,
) -> Result<Vec<SessionRow>> {
    let kind = if llm_only { " AND s.kind = 'llm'" } else { "" };
    let mut sql = format!(
        "SELECT session_id, tool, path, project, title, started, msg_count, kind, {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)
         FROM files f
         WHERE kind = 'main' AND NOT EXISTS
               (SELECT 1 FROM summaries s WHERE s.session_id = f.session_id{kind})",
    );
    let mut args: Vec<String> = Vec::new();
    if let Some(t) = tool {
        sql.push_str(" AND tool = ?");
        args.push(t.to_string());
    }
    sql.push_str(&format!(" ORDER BY started DESC LIMIT {limit}"));
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
        Ok(SessionRow {
            last_active: None,
            session_id: r.get(0)?,
            tool: r.get(1)?,
            path: r.get(2)?,
            project: r.get(3)?,
            title: r.get(4)?,
            started: r.get(5)?,
            msg_count: r.get(6)?,
            kind: r.get(7)?,
            preview: r.get(8)?,
            summary: r.get(9)?,
            tags: r.get(10)?,
            archived: r.get(11)?,
            account: None,
            parent_id: None,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// --- curation (the editable wiki layer) ---

/// One canonical form per tag: trimmed, lowercased, NFC-normalized - the same
/// normalization every other indexed string gets, so a tag typed in decomposed
/// form (macOS IME, some CJK inputs) matches on add, filter, and remove alike.
fn norm_tag(tag: &str) -> String {
    crate::util::nfc(&tag.trim().to_lowercase())
}

pub fn add_tag(conn: &Connection, session_id: &str, tag: &str) -> Result<()> {
    // Tags are joined with ',' at read time and the JSON/web contract splits on
    // it, so a comma inside a tag would corrupt the array into two elements.
    // Reject it (and the empty tag) at the input boundary.
    let tag = norm_tag(tag);
    if tag.is_empty() || tag.contains(',') {
        anyhow::bail!("a tag must be non-empty and contain no commas");
    }
    conn.execute(
        "INSERT OR IGNORE INTO tags(session_id, tag) VALUES (?1, ?2)",
        params![session_id, tag],
    )?;
    Ok(())
}

pub fn remove_tag(conn: &Connection, session_id: &str, tag: &str) -> Result<usize> {
    Ok(conn.execute(
        "DELETE FROM tags WHERE session_id = ?1 AND tag = ?2",
        params![session_id, norm_tag(tag)],
    )?)
}

pub fn set_note(conn: &Connection, session_id: &str, note: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO notes(session_id, note, updated)
         VALUES (?1, ?2, datetime('now'))",
        params![session_id, note],
    )?;
    Ok(())
}

pub fn note_for(conn: &Connection, session_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT note FROM notes WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .ok())
}

/// All tags in use, with how many sessions carry each.
pub fn tag_counts(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT tag, count(*) FROM tags GROUP BY tag ORDER BY count(*) DESC, tag")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_favorites(conn: &Connection) -> Result<Vec<String>> {
    ensure_favorites_table(conn)?;
    let mut stmt = conn.prepare("SELECT session_id FROM favorites ORDER BY created_at DESC")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut ids = Vec::new();
    for id in rows {
        ids.push(id?);
    }
    Ok(ids)
}

pub fn set_favorite(conn: &Connection, session_id: &str, favorite: bool) -> Result<()> {
    ensure_favorites_table(conn)?;
    if favorite {
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO favorites(session_id, created_at) VALUES (?1, ?2)",
            params![session_id, now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM favorites WHERE session_id = ?1",
            params![session_id],
        )?;
    }
    Ok(())
}

pub fn is_favorite(conn: &Connection, session_id: &str) -> Result<bool> {
    ensure_favorites_table(conn)?;
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM favorites WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(count > 0)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Snippet {
    pub id: i64,
    pub session_id: String,
    pub message_idx: usize,
    pub title: String,
    pub lang: Option<String>,
    pub content: String,
    pub tags: Option<String>,
    pub created_at: String,
    pub tool: Option<String>,
    pub role: Option<String>,
    pub message_ts: Option<String>,
}

/// What a caller asks for when saving a snippet. `tool`, `role` and
/// `message_ts` are inferred from the session when left empty.
#[derive(Debug, Clone, Copy, Default)]
pub struct NewSnippet<'a> {
    pub session_id: &'a str,
    pub message_idx: usize,
    pub title: &'a str,
    pub lang: Option<&'a str>,
    pub content: &'a str,
    pub tags: Option<&'a str>,
    pub tool: Option<&'a str>,
    pub role: Option<&'a str>,
    pub message_ts: Option<&'a str>,
}

pub fn save_snippet(conn: &Connection, snippet: &NewSnippet<'_>) -> Result<i64> {
    let NewSnippet {
        session_id,
        message_idx,
        title,
        lang,
        content,
        tags,
        tool,
        role,
        message_ts,
    } = *snippet;
    ensure_snippets_table(conn)?;
    let now = chrono::Utc::now().to_rfc3339();

    let effective_tool: Option<String> = match tool {
        Some(t) if !t.trim().is_empty() => Some(t.trim().to_string()),
        _ => conn
            .query_row(
                "SELECT tool FROM files WHERE session_id = ?1 LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None)
            .or_else(|| {
                conn.query_row(
                    "SELECT tool FROM archive WHERE session_id = ?1",
                    params![session_id],
                    |r| r.get(0),
                )
                .optional()
                .unwrap_or(None)
            }),
    };

    let effective_role = match role {
        Some(r) if !r.trim().is_empty() => r.trim(),
        _ => "assistant",
    };

    let effective_ts: String = match message_ts {
        Some(ts) if !ts.trim().is_empty() => ts.trim().to_string(),
        _ => conn
            .query_row(
                "SELECT started FROM files WHERE session_id = ?1 LIMIT 1",
                params![session_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None)
            .flatten()
            .unwrap_or_else(|| now.clone()),
    };

    conn.execute(
        "INSERT INTO snippets(session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            session_id,
            message_idx as i64,
            title,
            lang,
            content,
            tags,
            now,
            effective_tool,
            effective_role,
            effective_ts,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn rename_snippet(conn: &Connection, id: i64, new_title: &str) -> Result<bool> {
    ensure_snippets_table(conn)?;
    let count = conn.execute(
        "UPDATE snippets SET title = ?1 WHERE id = ?2",
        params![new_title.trim(), id],
    )?;
    Ok(count > 0)
}

pub fn delete_snippet(conn: &Connection, id: i64) -> Result<bool> {
    ensure_snippets_table(conn)?;
    let count = conn.execute("DELETE FROM snippets WHERE id = ?1", params![id])?;
    Ok(count > 0)
}

pub fn list_snippets(
    conn: &Connection,
    query: Option<&str>,
    lang: Option<&str>,
    session_id: Option<&str>,
    tool: Option<&str>,
    role: Option<&str>,
    limit: usize,
) -> Result<Vec<Snippet>> {
    ensure_snippets_table(conn)?;
    let mut sql = String::from(
        "SELECT id, session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts FROM snippets WHERE 1=1",
    );
    let mut params_vec: Vec<rusqlite::types::Value> = Vec::new();

    if let Some(s) = session_id {
        if !s.is_empty() {
            sql.push_str(" AND session_id = ?");
            params_vec.push(rusqlite::types::Value::Text(s.to_string()));
        }
    }
    if let Some(l) = lang {
        if !l.is_empty() && l != "all" {
            sql.push_str(" AND lower(lang) = lower(?)");
            params_vec.push(rusqlite::types::Value::Text(l.to_string()));
        }
    }
    if let Some(t) = tool {
        let t_trim = t.trim();
        if !t_trim.is_empty() && t_trim != "all" {
            sql.push_str(" AND lower(tool) = lower(?)");
            params_vec.push(rusqlite::types::Value::Text(t_trim.to_string()));
        }
    }
    if let Some(r) = role {
        let r_trim = r.trim();
        if !r_trim.is_empty() && r_trim != "all" {
            if r_trim == "user,assistant" {
                sql.push_str(" AND lower(role) IN ('user', 'assistant')");
            } else {
                sql.push_str(" AND lower(role) = lower(?)");
                params_vec.push(rusqlite::types::Value::Text(r_trim.to_string()));
            }
        }
    }
    if let Some(q) = query {
        let q_trim = q.trim();
        if !q_trim.is_empty() {
            sql.push_str(" AND (title LIKE ? OR content LIKE ? OR tags LIKE ?)");
            let pattern = format!("%{}%", q_trim);
            params_vec.push(rusqlite::types::Value::Text(pattern.clone()));
            params_vec.push(rusqlite::types::Value::Text(pattern.clone()));
            params_vec.push(rusqlite::types::Value::Text(pattern));
        }
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    params_vec.push(rusqlite::types::Value::Integer(limit as i64));

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map(rusqlite::params_from_iter(params_vec), |r| {
        Ok(Snippet {
            id: r.get(0)?,
            session_id: r.get(1)?,
            message_idx: r.get::<_, i64>(2)? as usize,
            title: r.get(3)?,
            lang: r.get(4)?,
            content: r.get(5)?,
            tags: r.get(6)?,
            created_at: r.get(7)?,
            tool: r.get(8)?,
            role: r.get(9)?,
            message_ts: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn snippet_indices_for_session(conn: &Connection, session_id: &str) -> Result<Vec<usize>> {
    ensure_snippets_table(conn)?;
    let mut stmt = match conn.prepare("SELECT message_idx FROM snippets WHERE session_id = ?1") {
        Ok(s) => s,
        Err(_) => return Ok(Vec::new()),
    };
    let rows = stmt.query_map(params![session_id], |r| Ok(r.get::<_, i64>(0)? as usize))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// --- memory proposals: what an agent offers, before a person accepts it ---

/// The longest title and body a proposal may carry. An agent asked to "save
/// everything" otherwise queues a whole transcript as one note.
pub const PROPOSAL_TITLE_MAX: usize = 200;
pub const PROPOSAL_CONTENT_MAX: usize = 20_000;

/// A note an agent proposed to remember.
///
/// `remember` writes straight into the knowledge base, so a wrong conclusion is
/// recalled as fact by every later session. A proposal stays out of the index
/// until a human approves it, and the queue is where they decide.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MemoryProposal {
    pub id: String,
    pub title: String,
    pub content: String,
    pub tags: String,
    pub session_id: Option<String>,
    pub tool: Option<String>,
    pub created_at: String,
    /// `pending`, `approved` or `rejected`.
    pub status: String,
    pub decided_at: Option<String>,
    /// Set when the approval wrote a snippet, so approving twice is one snippet.
    pub snippet_id: Option<i64>,
}

/// Proposals are kept data, not something a cache rebuild can re-derive.
///
/// The DDL is best-effort for the same reason the favorites table is: a
/// read-only handle (the MCP server, the web UI) cannot create it, and the
/// writer that owns the index does so when it opens. Turning that into an error
/// here would hide an empty queue behind a failure.
pub fn ensure_memory_proposals_table(conn: &Connection) -> Result<()> {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS memory_proposals(
            id          TEXT PRIMARY KEY,
            title       TEXT NOT NULL,
            content     TEXT NOT NULL,
            tags        TEXT NOT NULL DEFAULT '',
            session_id  TEXT,
            tool        TEXT,
            created_at  TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'pending',
            decided_at  TEXT,
            snippet_id  INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_memprop_status ON memory_proposals(status, created_at);",
    );
    Ok(())
}

/// Queue a note for approval and answer with its id.
///
/// The content is redacted here rather than at approval: the queue is readable
/// by the web UI and lives in backups, and a secret must not reach either while
/// waiting for a decision.
pub fn propose_memory(
    conn: &Connection,
    title: &str,
    content: &str,
    tags: Option<&str>,
    session_id: Option<&str>,
    tool: Option<&str>,
) -> Result<String> {
    ensure_memory_proposals_table(conn)?;
    let title = title.trim();
    let content = content.trim();
    if title.is_empty() {
        anyhow::bail!("title is required");
    }
    if content.is_empty() {
        anyhow::bail!("content is required");
    }
    // Redact before capping: a cut through a secret would no longer match its
    // pattern, and the rest of it would be stored.
    let title = crate::util::truncate(&crate::redact::redact(title), PROPOSAL_TITLE_MAX);
    let content = cap_chars(&crate::redact::redact(content), PROPOSAL_CONTENT_MAX);
    let now = chrono::Utc::now().to_rfc3339();
    let id = crate::util::short_id(&format!("{title}\u{1}{content}\u{1}{now}"));
    conn.execute(
        "INSERT INTO memory_proposals(id, title, content, tags, session_id, tool, created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![
            id,
            title,
            content,
            crate::redact::redact(tags.unwrap_or("").trim()).as_ref(),
            session_id.filter(|s| !s.trim().is_empty()).map(str::trim),
            tool.filter(|t| !t.trim().is_empty()).map(str::trim),
            now,
        ],
    )?;
    Ok(id)
}

/// Proposals newest first. `status` narrows to one state; empty or `all` answers
/// with every state, decided ones included.
pub fn list_memory_proposals(
    conn: &Connection,
    status: Option<&str>,
) -> Result<Vec<MemoryProposal>> {
    ensure_memory_proposals_table(conn)?;
    let wanted = status
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("all"))
        .map(str::to_ascii_lowercase);
    let mut stmt = conn.prepare(
        "SELECT id, title, content, tags, session_id, tool, created_at, status,
                decided_at, snippet_id
         FROM memory_proposals
         WHERE (?1 IS NULL OR status = ?1)
         ORDER BY created_at DESC, id",
    )?;
    let rows = stmt.query_map(params![wanted], |r| {
        Ok(MemoryProposal {
            id: r.get(0)?,
            title: r.get(1)?,
            content: r.get(2)?,
            tags: r.get(3)?,
            session_id: r.get(4)?,
            tool: r.get(5)?,
            created_at: r.get(6)?,
            status: r.get(7)?,
            decided_at: r.get(8)?,
            snippet_id: r.get(9)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Save a memory the way the MCP `remember` tool does, so a note an agent saved
/// and one a person approved from the queue cannot drift apart in shape.
pub fn save_memory_snippet(
    conn: &Connection,
    title: &str,
    content: &str,
    tags: Option<&str>,
    session_id: &str,
) -> Result<i64> {
    // What an agent asks to remember is untrusted text, in every field of it.
    let title = crate::redact::redact(title);
    let content = crate::redact::redact(content);
    let tags = tags.map(crate::redact::redact);
    save_snippet(
        conn,
        &NewSnippet {
            session_id,
            message_idx: 0,
            title: &title,
            lang: Some("markdown"),
            content: &content,
            tags: tags.as_deref(),
            tool: Some("assistant"),
            role: Some("assistant_memory"),
            message_ts: None,
        },
    )
}

/// Write an approved proposal to the knowledge base. Idempotent: a proposal that
/// already carries a snippet answers with that id instead of saving a second one.
/// A rejected one stays rejected: that decision is the record the queue keeps.
pub fn approve_memory_proposal(conn: &Connection, id: &str) -> Result<i64> {
    ensure_memory_proposals_table(conn)?;
    let row = conn.query_row(
        "SELECT title, content, tags, session_id, snippet_id, status
         FROM memory_proposals WHERE id = ?1",
        params![id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, String>(5)?,
            ))
        },
    );
    let Ok((title, content, tags, session_id, existing, status)) = row else {
        anyhow::bail!("no proposal {id}");
    };
    if let Some(snippet_id) = existing {
        return Ok(snippet_id);
    }
    if status == "rejected" {
        anyhow::bail!("proposal {id} was rejected");
    }
    let snippet_id = save_memory_snippet(
        conn,
        &title,
        &content,
        (!tags.is_empty()).then_some(tags.as_str()),
        session_id.as_deref().unwrap_or("global"),
    )?;
    conn.execute(
        "UPDATE memory_proposals SET status = 'approved', decided_at = ?2, snippet_id = ?3
         WHERE id = ?1",
        params![id, chrono::Utc::now().to_rfc3339(), snippet_id,],
    )?;
    Ok(snippet_id)
}

/// Turn a proposal down. It stays in the table as a record of the decision;
/// deleting it would lose the only trace that the agent offered it.
pub fn reject_memory_proposal(conn: &Connection, id: &str) -> Result<bool> {
    ensure_memory_proposals_table(conn)?;
    let status: Option<String> = conn
        .query_row(
            "SELECT status FROM memory_proposals WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    if status.as_deref() == Some("approved") {
        // It is already in the knowledge base; saying "rejected" would be false.
        anyhow::bail!("proposal {id} was already approved");
    }
    let changed = conn.execute(
        "UPDATE memory_proposals SET status = 'rejected', decided_at = ?2
         WHERE id = ?1 AND status != 'approved'",
        params![id, chrono::Utc::now().to_rfc3339()],
    )?;
    if changed > 0 {
        return Ok(true);
    }
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_proposals WHERE id = ?1)",
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(false);
    Ok(exists)
}

/// Keep the first `max` characters of a body, marked as cut. Unlike
/// `util::truncate` this preserves newlines: the content is markdown.
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max).collect();
    cut.push_str("\n… truncated");
    cut
}

// --- provenance: sessions <-> the code they produced ---

/// Files a session edited or created, in the order it first touched them.
pub fn files_for(conn: &Connection, session_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT path FROM touched WHERE session_id = ?1 ORDER BY rowid")?;
    let rows = stmt.query_map(params![session_id], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One recorded edit to a file - the evidence layer behind `touched`. `matched_path`
/// is the absolute stored path the query resolved to.
#[derive(Debug, Serialize)]
pub struct FileEdit {
    pub session_id: String,
    pub kind: String,
    pub ts: Option<String>,
    pub snippet: String,
    pub matched_path: String,
}

/// The concrete edits made to a file, newest first - the evidence for "why does
/// this file look like this". Resolves a relative path against the absolute one
/// on disk by suffix, the same match `sessions_for_file` uses, so
/// `edits_for("src/auth.rs")` finds `/home/me/proj/src/auth.rs`.
/// A `usize` limit clamped to a positive `i64` for SQLite: a value past i64::MAX
/// wraps negative, which SQLite reads as "unlimited" - defeating the memory bound.
fn sql_limit(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

pub fn edits_for(conn: &Connection, query: &str, limit: usize) -> Result<Vec<FileEdit>> {
    let q = path_query(query);
    // Escape LIKE metacharacters so a caller-supplied `%`/`_` matches literally.
    let esc = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let suffix = format!("%/{esc}");
    let p = path_col("path");
    let mut stmt = conn.prepare(&format!(
        "SELECT session_id, kind, ts, snippet, path
         FROM edits
         WHERE {p} = ?1 OR {p} LIKE ?2 ESCAPE '\\'
            OR (length(?1) > length(path)
                AND substr(?1, -length(path)) = {p}
                AND substr(?1, -length(path)-1, 1) = '/')
         ORDER BY ts DESC, rowid DESC LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![q, suffix, sql_limit(limit)], |r| {
        Ok(FileEdit {
            session_id: r.get(0)?,
            kind: r.get(1)?,
            ts: r.get(2)?,
            snippet: r.get(3)?,
            matched_path: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A single session's edits to a file, newest first - scoped by session so no
/// session's edits can be starved by another's under a shared cap. Uses the SAME
/// suffix path match as `sessions_for_file`, so a session that edited the file
/// under more than one spelling (abs + relative) contributes ALL its edits, not
/// just the one spelling `sessions_for_file`'s GROUP BY happened to pick.
pub fn edits_for_session(
    conn: &Connection,
    session_id: &str,
    query: &str,
    limit: usize,
) -> Result<Vec<FileEdit>> {
    let q = path_query(query);
    let esc = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let suffix = format!("%/{esc}");
    let rooted = Path::new(&q).has_root();
    let (ep, fp) = (path_col("e.path"), path_col("f.project"));
    let mut stmt = conn.prepare(&format!(
        "SELECT e.session_id, e.kind, e.ts, e.snippet, e.path
         FROM edits e LEFT JOIN files f ON f.session_id = e.session_id
         WHERE e.session_id = ?1
           AND ({ep} = ?2 COLLATE NOCASE OR {ep} LIKE ?3 ESCAPE '\\'
                OR (length(?2) > length(e.path)
                    AND substr(?2, -length(e.path)) = {ep} COLLATE NOCASE
                    AND substr(?2, -length(e.path)-1, 1) = '/'
                    AND (NOT ?5
                         OR rtrim({fp}, '/') || '/' || {ep} = ?2 COLLATE NOCASE)))
         ORDER BY e.ts DESC, e.rowid DESC LIMIT ?4"
    ))?;
    let rows = stmt.query_map(
        params![session_id, q, suffix, sql_limit(limit), rooted],
        |r| {
            Ok(FileEdit {
                session_id: r.get(0)?,
                kind: r.get(1)?,
                ts: r.get(2)?,
                snippet: r.get(3)?,
                matched_path: r.get(4)?,
            })
        },
    )?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One session's slice of a file's history: its metadata plus the edits it made
/// to this file.
#[derive(Serialize)]
pub struct SessionEvidence {
    pub session: SessionRow,
    pub edits: Vec<FileEdit>,
}

/// A file's full evidence chain - the sessions that edited it, newest first,
/// each carrying its own edits. What the file-history page renders.
#[derive(Serialize)]
pub struct FileHistory {
    pub path: String,
    pub sessions: Vec<SessionEvidence>,
}

/// Assemble a file's evidence chain: the sessions that touched it (with metadata,
/// newest first) joined to each session's recorded edits. Sessions that touched
/// the file but carry no structured edits (other adapters, archived sessions)
/// appear with an empty `edits` list - the touch is still evidence.
pub fn evidence_for(conn: &Connection, path: &str, limit: usize) -> Result<FileHistory> {
    let sessions = sessions_for_file(conn, path, limit)?;
    // Fetch edits PER session (scoped by session, matched by the SAME query path
    // so every spelling counts), so one session's edits are never starved by
    // another's - and `limit == 0` does no edit query.
    const PER_SESSION_EDIT_CAP: usize = 500;
    let sessions = sessions
        .into_iter()
        .map(|(session, _matched)| {
            let edits = edits_for_session(conn, &session.session_id, path, PER_SESSION_EDIT_CAP)?;
            Ok(SessionEvidence { session, edits })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(FileHistory {
        path: path.to_string(),
        sessions,
    })
}

/// Sessions that touched a file, newest first - the reverse provenance link.
/// Matches either the exact stored path or any stored path ending in the
/// query, so a relative `src/auth.rs` finds `/home/me/proj/src/auth.rs`. The
/// matched stored path is returned alongside each session.
/// The file name to retry with when a full path traces to nothing.
///
/// Folders get renamed. A session recorded `~/Project/lunch/diag.py`; the folder
/// is `~/Project/slack` now, so tracing by the path the file has TODAY matched
/// nothing and reported that no session had touched it - about a file whose
/// whole history was in the index under its old directory.
///
/// The name is the part that survives a move. `None` when there is nothing to
/// fall back to: a bare name would just repeat the same miss, and a trailing
/// slash names a directory rather than a file.
pub fn basename_fallback(query: &str) -> Option<String> {
    let q = query.trim();
    if q.is_empty() || q.ends_with('/') {
        return None;
    }
    let (head, name) = q.rsplit_once('/')?;
    (!head.is_empty() && !name.is_empty()).then(|| name.to_string())
}

pub fn sessions_for_file(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<(SessionRow, String)>> {
    let q = path_query(query.trim().trim_matches('"'));
    let esc = q
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let suffix = format!("%/{esc}");
    let rooted = Path::new(&q).has_root();
    let (tp, fp) = (path_col("t.path"), path_col("f.project"));
    let mut stmt = conn.prepare(&format!(
        "SELECT f.session_id, f.tool, f.path, f.project, f.title, f.started, f.msg_count, f.kind,
                {SUMMARY_SQL}, {TAGS_SQL}, t.path, (f.archived_at IS NOT NULL)
         FROM touched t JOIN files f ON f.session_id = t.session_id
         WHERE {tp} = ?1 COLLATE NOCASE OR {tp} LIKE ?2 ESCAPE '\\'
            OR (length(?1) > length(t.path)
                AND substr(?1, -length(t.path)) = {tp} COLLATE NOCASE
                AND substr(?1, -length(t.path)-1, 1) = '/'
                AND (NOT ?4
                     OR rtrim({fp}, '/') || '/' || {tp} = ?1 COLLATE NOCASE))
         GROUP BY f.session_id
         ORDER BY f.started DESC, f.session_id LIMIT ?3"
    ))?;
    let rows = stmt.query_map(params![q, suffix, sql_limit(limit), rooted], |r| {
        Ok((
            SessionRow {
                last_active: None,
                session_id: r.get(0)?,
                tool: r.get(1)?,
                path: r.get(2)?,
                project: r.get(3)?,
                title: r.get(4)?,
                started: r.get(5)?,
                msg_count: r.get(6)?,
                kind: r.get(7)?,
                preview: None,
                summary: r.get(8)?,
                tags: r.get(9)?,
                archived: r.get(11)?,
                account: None,
                parent_id: None,
            },
            r.get::<_, String>(10)?,
        ))
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut().map(|(r, _)| r));
    Ok(out)
}

/// Like `sessions_for_file` but returns the start/end epoch window and project
/// that blame's commit->session attribution needs. Matches both an exact stored
/// path and any stored path ending in the query suffix, so a repo-relative query
/// (e.g. `src/auth.rs`) catches Claude Code's absolute touched paths and Codex's
/// relative ones alike (NFC-normalized).
pub fn sessions_touching(
    conn: &Connection,
    query: &str,
) -> Result<Vec<crate::blame::TouchingSession>> {
    let q = path_query(query);
    let suffix = format!("%/{}", escape_like(&q));
    let tp = path_col("t.path");
    let mut stmt = conn.prepare(&format!(
        "SELECT f.session_id, f.tool, f.title, f.project, f.started, f.ended, (f.archived_at IS NOT NULL)
         FROM touched t JOIN files f ON f.session_id = t.session_id
         WHERE {tp} = ?1 OR {tp} LIKE ?2 ESCAPE '\\'
            OR (length(?1) > length(t.path)
                AND substr(?1, -length(t.path)) = {tp}
                AND substr(?1, -length(t.path)-1, 1) = '/')
         GROUP BY f.session_id"
    ))?;
    let rows = stmt.query_map(params![q, suffix], |r| {
        let started: Option<String> = r.get(4)?;
        let ended: Option<String> = r.get(5)?;
        Ok(crate::blame::TouchingSession {
            session_id: r.get(0)?,
            tool: r.get(1)?,
            title: r.get(2)?,
            project: r.get(3)?,
            started: started.as_deref().and_then(to_epoch),
            ended: ended.as_deref().and_then(to_epoch),
            archived: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn touched_paths_matching(
    conn: &Connection,
    session_id: &str,
    query: &str,
) -> Result<Vec<String>> {
    let q = path_query(query);
    let suffix = format!("%/{}", escape_like(&q));
    let p = path_col("path");
    let mut stmt = conn.prepare(&format!(
        "SELECT path FROM touched
         WHERE session_id = ?1
           AND ({p} = ?2 OR {p} LIKE ?3 ESCAPE '\\'
                OR (length(?2) > length(path)
                    AND substr(?2, -length(path)) = {p}
                    AND substr(?2, -length(path)-1, 1) = '/'))"
    ))?;
    let rows = stmt.query_map(params![session_id, q, suffix], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
}

/// Parse a stored timestamp (RFC3339) to epoch seconds; None if unparseable.
fn to_epoch(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

// --- commit tracking: passive commit SHA -> session linking (AI Git Blame) ---

/// Validate whether a token looks like a Git commit hash:
/// - length between 7 and 40 characters
/// - all ASCII hex digits
/// - contains at least one letter 'a'-'f' (or length 40) to prevent false positives from pure numbers
pub fn is_hex_sha(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 7 || bytes.len() > 40 {
        return false;
    }
    let mut has_hex_letter = false;
    for &b in bytes {
        if b.is_ascii_hexdigit() {
            if b.is_ascii_alphabetic() {
                has_hex_letter = true;
            }
        } else {
            return false;
        }
    }
    has_hex_letter || bytes.len() == 40
}

/// Extract the commits a session *created* from tool output: the `[<branch>
/// <sha>] <subject>` line `git commit` prints, plus the subject wrapped to the
/// next line. Callers feed this only assistant/tool text.
///
/// Mentions are not authorship, so they are deliberately not collected: a
/// `/commit/<sha>` link, or a `commit <sha>` line of `git log`/`git show`, says
/// somebody read that commit, not that this session made it. Attributing them
/// credited sessions with other people's history. `AI-Session:` trailers
/// (`hook install-git`) give the exact answer instead, when they exist.
pub fn extract_commits_from_text(text: &str) -> Vec<(String, Option<String>)> {
    let mut results = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    for (i, raw_line) in lines.iter().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Terminal git commit output: "[<branch> <sha>] <subject>"
        // Must start with '[' or whitespace+'[' (not '[[' or '([')
        if trimmed.starts_with('[') && !trimmed.starts_with("[[") {
            if let Some(close_idx) = trimmed.find(']') {
                let inner = trimmed[1..close_idx].trim();
                let parts: Vec<&str> = inner.split_whitespace().collect();
                if parts.len() >= 2 {
                    let candidate = parts[parts.len() - 1];
                    if is_hex_sha(candidate) {
                        let mut subj = trimmed[close_idx + 1..].trim();
                        // If subject was not on the same line, check next line
                        if subj.is_empty() && i + 1 < lines.len() {
                            let next = lines[i + 1].trim();
                            if !next.is_empty()
                                && !next.starts_with('[')
                                && !next.starts_with("```")
                                && !next.ends_with("changed")
                                && !next.contains("files changed")
                                && !next.contains("file changed")
                            {
                                subj = next;
                            }
                        }
                        let subj_opt = if subj.is_empty() {
                            None
                        } else {
                            Some(subj.to_string())
                        };
                        results.push((candidate.to_ascii_lowercase(), subj_opt));
                    }
                }
            }
        }
    }
    // Deduplicate preserving order
    let mut deduped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (sha, msg) in results {
        if seen.insert(sha.clone()) {
            deduped.push((sha, msg));
        }
    }
    deduped
}

/// Sessions that claim a commit, by exact or prefix SHA match, newest first.
///
/// More than one is normal: the `git commit` output can be quoted into a later
/// session, or two sessions can share a branch. Callers show the ambiguity
/// instead of silently picking one - `AI-Session:` trailers are the exact
/// answer and take precedence over this lookup.
pub fn find_session_by_commit(conn: &Connection, sha: &str) -> Result<Vec<SessionRow>> {
    let clean = sha.trim().to_ascii_lowercase();
    // A SHA is hex; anything else (including `%` and `_`, which LIKE would read
    // as wildcards) matches nothing.
    if clean.len() < 7 || !clean.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = conn
        .prepare_cached(
            "SELECT DISTINCT session_id FROM session_commits
             WHERE commit_sha LIKE ?1 || '%'
                OR (?1 LIKE commit_sha || '%' AND length(commit_sha) >= 7)",
        )?
        .query_map(params![clean], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::new();
    for id in ids {
        if let Some(row) = resolve(conn, &id)?.into_iter().next() {
            out.push(row);
        }
    }
    // Newest first; a session that never recorded a time sorts last.
    out.sort_by_cached_key(|r| {
        std::cmp::Reverse(
            r.last_active
                .as_deref()
                .or(r.started.as_deref())
                .unwrap_or("")
                .to_string(),
        )
    });
    Ok(out)
}

/// One row of `session_commits`: sha, the commit subject, and its unix time.
pub type SessionCommit = (String, Option<String>, Option<i64>);

/// Retrieve all commits recorded for a session.
pub fn get_session_commits(conn: &Connection, session_id: &str) -> Result<Vec<SessionCommit>> {
    let mut stmt = conn.prepare_cached(
        "SELECT commit_sha, message, timestamp FROM session_commits WHERE session_id = ?1 ORDER BY rowid ASC",
    )?;
    let rows = stmt
        .query_map(params![session_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- archive: serving and forgetting sessions whose originals are gone ---

/// The display name for a tool this binary's adapter registry does not know.
///
/// A program that embeds this crate can register its own adapters, so rows in
/// the index may name a tool the standalone binary has never heard of. Those
/// rows used to print as "unknown". `Session.tool` is `&'static str`, so the
/// row's own string has to outlive the call: intern it once and leak it. The
/// set of tool names is small and fixed by the tools a user actually runs, so
/// the leak is bounded by that, not by the number of sessions.
fn interned_tool(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut names = NAMES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = names.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

/// Reconstruct an archived or external-adapter session from its indexed copy.
/// Per-message timestamps are omitted; full tool output is present only when
/// the index's retention mode was `full` when that session was indexed.
pub fn session_from_index(conn: &Connection, row: &SessionRow) -> Result<crate::model::Session> {
    use crate::model::{Message, Role, ToolEvent, ToolSummary};
    let mut stmt = conn
        .prepare("SELECT role, text, full_text, COALESCE(is_rewound, 0) FROM messages WHERE session_id = ?1 ORDER BY id")?;
    let messages: Vec<Message> = stmt
        .query_map(params![row.session_id], |r| {
            let role: String = r.get(0)?;
            let text: String = r.get(1)?;
            let full_text: Option<String> = r.get(2)?;
            let is_rewound: bool = r.get::<_, i32>(3)? != 0;
            let role = match role.as_str() {
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => Role::Tool,
            };
            let tool = full_text.and_then(|output| {
                (role == Role::Tool && crate::tool_summary::is_compact_summary_line(&text))
                    .then_some(ToolEvent::Summary(ToolSummary {
                        name: "tool".into(),
                        args: serde_json::Value::Null,
                        output: Some(output),
                        is_error: None,
                        lines: None,
                    }))
            });
            Ok(Message {
                role,
                text,
                ts: None,
                tool,
                is_rewound,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);

    let tool = adapters::by_name(&row.tool)
        .map(|a| a.name())
        .unwrap_or_else(|| interned_tool(&row.tool));
    let started = row
        .started
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    Ok(crate::model::Session {
        id: row.session_id.clone(),
        tool,
        // A shared-store key carries a U+001F separator in the stored path;
        // render it human-readable (`<db>#<id>`) so it never leaks into show /
        // brief / web. Real file paths never contain U+001F, so this is a no-op
        // for every other adapter.
        path: std::path::PathBuf::from(row.path.replace('\u{1f}', "#")),
        project: row.project.clone(),
        started,
        ended: started,
        title: row.title.clone(),
        subagent: row.kind == "sub",
        parent_id: row.parent_id.clone(),
        messages,
        touched: files_for(conn, &row.session_id)?,
        edits: Vec::new(),
    })
}

/// Permanently remove a session from the index AND the archive - the only way
/// to undo archiving for a session the user genuinely wants gone. Curation for
/// it (tags/notes/summary) goes too, since the session no longer exists here.
/// All-or-nothing: a crash mid-forget must not leave the FTS index out of sync
/// with `messages`, nor an `archive` row that would resurrect it on rebuild.
pub fn forget(conn: &mut Connection, session_id: &str) -> Result<()> {
    ensure_favorites_table(conn)?;
    ensure_snippets_table(conn)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    delete_session_msgs(&tx, session_id)?;
    for table in [
        "files",
        "touched",
        "edits",
        "archive",
        "summaries",
        "tags",
        "notes",
        "favorites",
        "snippets",
    ] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            params![session_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

// --- related sessions (backlinks) ---

/// Sessions related to `session_id`. A session is most usefully "related" to
/// the others about the same codebase, so same-project sessions are the spine;
/// sessions sharing a user tag are layered on as explicit links. Both are
/// indexed lookups, so this is instant even over a large store - the earlier
/// full-text-on-title approach was both slow and noisy (generic title words
/// like "session" matched everything).
pub fn related(conn: &Connection, session_id: &str, limit: usize) -> Result<Vec<SessionRow>> {
    let Some(target) = resolve(conn, session_id)?.into_iter().next() else {
        return Ok(vec![]);
    };
    let target_tags: Vec<String> = target
        .tags
        .as_deref()
        .map(|t| t.split(',').map(String::from).collect())
        .unwrap_or_default();

    let mut out: Vec<SessionRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(target.session_id.clone());

    // 1. same project (exact), most recent first - the same-context spine.
    if !target.project.is_empty() {
        let sql = format!(
            "SELECT session_id, tool, path, project, title, started, msg_count, kind,
                    {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL)
             FROM files f
             WHERE kind = 'main' AND project = ?1 AND session_id != ?2
             ORDER BY started DESC LIMIT ?3"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            params![target.project, target.session_id, limit as i64 + 1],
            map_row,
        )?;
        for row in rows {
            let row = row?;
            if seen.insert(row.session_id.clone()) {
                out.push(row);
            }
        }
    }

    // 2. sessions that edited a file this one also edited - the strongest
    //    signal that two sessions are about the same work, and one no other
    //    session viewer has, since it comes from the provenance link.
    if out.len() < limit {
        // Compare files, not spellings: a relative path is read against its
        // session's folder (`src/auth.rs` from `/proj/api` is the full path
        // another tool recorded; from `/proj/web` it is another file), and
        // case is ignored as `trace` ignores it. Only a Unix folder is joined:
        // a Windows path (`C:\repo\src\a.rs`) is compared as recorded.
        let full = "CASE WHEN s.project LIKE '/%' AND t.path NOT LIKE '/%'
                         THEN rtrim(s.project, '/') || '/' || t.path ELSE t.path END";
        let sql = format!(
            "SELECT DISTINCT f.session_id, f.tool, f.path, f.project, f.title, f.started,
                    f.msg_count, f.kind, {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (f.archived_at IS NOT NULL)
             FROM touched t
             JOIN files s ON s.session_id = t.session_id
             JOIN files f ON f.session_id = t.session_id
             WHERE t.session_id != ?1 AND f.kind = 'main'
               AND ({full}) COLLATE NOCASE IN (
                   SELECT {full} FROM touched t JOIN files s ON s.session_id = t.session_id
                   WHERE t.session_id = ?1)
             ORDER BY f.started DESC LIMIT 50"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![target.session_id], map_row)?;
        for row in rows {
            let row = row?;
            if seen.insert(row.session_id.clone()) {
                out.push(row);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }

    // 3. sessions that share a tag with the target (explicit wiki links).
    if out.len() < limit && !target_tags.is_empty() {
        let placeholders = target_tags
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT DISTINCT f.session_id, f.tool, f.path, f.project, f.title, f.started,
                    f.msg_count, f.kind, {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (f.archived_at IS NOT NULL)
             FROM files f JOIN tags t ON t.session_id = f.session_id
             WHERE f.kind = 'main' AND t.tag IN ({placeholders})
             ORDER BY f.started DESC LIMIT 50"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(&target_tags), map_row)?;
        for row in rows {
            let row = row?;
            if seen.insert(row.session_id.clone()) {
                out.push(row);
                if out.len() >= limit {
                    break;
                }
            }
        }
    }

    out.truncate(limit);
    crate::account_link::annotate(out.iter_mut());
    Ok(out)
}

/// Row mapper for the full session-list column set.
fn map_row(r: &rusqlite::Row) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        last_active: None,
        session_id: r.get(0)?,
        tool: r.get(1)?,
        path: r.get(2)?,
        project: r.get(3)?,
        title: r.get(4)?,
        started: r.get(5)?,
        msg_count: r.get(6)?,
        kind: r.get(7)?,
        preview: r.get(8)?,
        summary: r.get(9)?,
        tags: r.get(10)?,
        archived: r.get(11)?,
        parent_id: r.get(12).ok(),
        account: None,
    })
}

/// Return all indexed subagent sessions for a given parent session id.
pub fn subagents_for_parent(conn: &Connection, parent_id: &str) -> Result<Vec<SessionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT session_id, tool, path, project, title, started, msg_count, kind,          {PREVIEW_SQL}, {SUMMARY_SQL}, {TAGS_SQL}, (archived_at IS NOT NULL), COALESCE(ended, started), parent_id          FROM files f          WHERE f.parent_id = ?1          ORDER BY started ASC, session_id ASC"
    ))?;
    let rows = stmt.query_map(params![parent_id], |r| {
        Ok(SessionRow {
            last_active: r.get(12)?,
            session_id: r.get(0)?,
            tool: r.get(1)?,
            path: r.get(2)?,
            project: r.get(3)?,
            title: r.get(4)?,
            started: r.get(5)?,
            msg_count: r.get(6)?,
            kind: r.get(7)?,
            preview: r.get(8)?,
            summary: r.get(9)?,
            tags: r.get(10)?,
            archived: r.get(11)?,
            parent_id: r.get(13)?,
            account: None,
        })
    })?;
    let mut out = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    crate::account_link::annotate(out.iter_mut());
    Ok(out)
}

// --- session engineering: management views ---

pub struct ProjectRow {
    pub project: String,
    /// The comparable form of `project`, so a client can filter with it and get
    /// every spelling of the folder back (see `util::project_key`).
    pub project_key: String,
    pub sessions: i64,
    pub messages: i64,
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

/// One row per project (a wiki "category" page), busiest first. `newest` is
/// the last activity, on the session list's clock: a long session begun months
/// ago but worked in yesterday makes its project recent.
/// Every spelling of `folder` stored in `files.project`, and of the folders below
/// it. The stored key is the repository root, so it cannot tell a subfolder from
/// its repository; the comparison has to happen on the folder itself.
pub fn project_folders(conn: &Connection, folder: &str) -> Result<Vec<String>> {
    let key = crate::util::project_key(folder);
    let below = format!("{key}/");
    let mut stmt = conn.prepare("SELECT DISTINCT project FROM files WHERE project != ''")?;
    let projects = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for project in projects {
        let project = project?;
        let candidate = crate::util::project_key(&project);
        if candidate == key || candidate.starts_with(&below) {
            out.push(project);
        }
    }
    Ok(out)
}

pub fn projects(conn: &Connection) -> Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT project, count(*), coalesce(sum(msg_count), 0), min(started),
                max(COALESCE(ended, started))
         FROM files WHERE kind = 'main' AND project != ''
         GROUP BY project ORDER BY count(*) DESC, max(COALESCE(ended, started)) DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        let project: String = r.get(0)?;
        Ok(ProjectRow {
            project_key: crate::util::project_root_key(&project),
            project,
            sessions: r.get(1)?,
            messages: r.get(2)?,
            oldest: r.get(3)?,
            newest: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub struct Stats {
    pub per_tool: Vec<(String, i64, i64)>, // tool, sessions, messages
    pub per_month: Vec<(String, i64)>,     // YYYY-MM, sessions
    pub total_sessions: i64,
    pub total_messages: i64,
    pub projects: i64,
    pub tags: i64,
    pub summarized: i64,
    /// Distinct files linked to at least one session (provenance coverage).
    pub files: i64,
    /// Sessions kept after the tool deleted their originals (archive mode).
    pub archived: i64,
}

pub fn stats(conn: &Connection) -> Result<Stats> {
    let mut per_tool_stmt = conn.prepare(
        "SELECT tool, count(*), coalesce(sum(msg_count),0) FROM files WHERE kind='main'
         GROUP BY tool ORDER BY count(*) DESC",
    )?;
    let per_tool = per_tool_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut per_month_stmt = conn.prepare(
        "SELECT substr(started,1,7) AS ym, count(*) FROM files
         WHERE kind='main' AND started IS NOT NULL
         GROUP BY ym ORDER BY ym DESC LIMIT 12",
    )?;
    let per_month = per_month_stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let one = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };
    Ok(Stats {
        per_tool,
        per_month,
        total_sessions: one("SELECT count(*) FROM files WHERE kind='main'")?,
        total_messages: one("SELECT coalesce(sum(msg_count),0) FROM files WHERE kind='main'")?,
        projects: one(
            "SELECT count(DISTINCT project) FROM files WHERE kind='main' AND project!=''",
        )?,
        tags: one("SELECT count(DISTINCT tag) FROM tags")?,
        summarized: one("SELECT count(*) FROM summaries")?,
        files: one("SELECT count(DISTINCT path) FROM touched")?,
        archived: one("SELECT count(*) FROM files WHERE kind='main' AND archived_at IS NOT NULL")?,
    })
}

#[cfg(test)]
mod native_id_tests {
    use super::*;

    // Realistic native store paths: a Codex rollout (uuid trails a timestamp) and
    // a Claude Code transcript (uuid IS the filename), plus a subagent transcript.
    const CODEX: &str = "/home/u/.codex/sessions/2025/05/13/rollout-2025-05-13T18-19-30-0a000000-0000-4000-8000-000000000001.jsonl";
    const CODEX_UUID: &str = "0a000000-0000-4000-8000-000000000001";
    const CLAUDE: &str =
        "/home/u/.claude/projects/-home-u-proj/1b111111-1111-4111-8111-111111111111.jsonl";
    const CLAUDE_UUID: &str = "1b111111-1111-4111-8111-111111111111";
    const SUBAGENT: &str = "/home/u/.claude/projects/-x/1b111111-1111-4111-8111-111111111111/subagents/agent-2c222222-2222-4222-8222-222222222222.jsonl";
    const SUBAGENT_UUID: &str = "2c222222-2222-4222-8222-222222222222";

    fn mem() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE files(
                 path TEXT PRIMARY KEY, mtime INTEGER NOT NULL DEFAULT 0,
                 size INTEGER NOT NULL DEFAULT 0, session_id TEXT NOT NULL,
                 tool TEXT NOT NULL, project TEXT NOT NULL DEFAULT '',
                 title TEXT NOT NULL DEFAULT '', started TEXT, ended TEXT,
                 msg_count INTEGER NOT NULL DEFAULT 0,
                 kind TEXT NOT NULL DEFAULT 'main', parent_id TEXT, archived_at TEXT);
             CREATE TABLE summaries(session_id TEXT PRIMARY KEY, summary TEXT NOT NULL, created TEXT NOT NULL);
             CREATE TABLE tags(session_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(session_id, tag));",
        )
        .unwrap();
        c
    }

    /// Insert a files row and return the sessionwiki short id it was keyed by.
    fn seed(c: &Connection, tool: &str, path: &str) -> String {
        let sid = crate::util::short_id(path);
        c.execute(
            "INSERT INTO files(path, session_id, tool, msg_count) VALUES(?1,?2,?3,3)",
            params![path, sid, tool],
        )
        .unwrap();
        sid
    }

    #[test]
    fn native_id_extracted_per_tool() {
        assert_eq!(native_id_of(CODEX).as_deref(), Some(CODEX_UUID));
        assert_eq!(native_id_of(CLAUDE).as_deref(), Some(CLAUDE_UUID));
        // A subagent transcript resolves to its OWN uuid (scanned from the file
        // name), not the parent uuid in the directory above it.
        assert_eq!(native_id_of(SUBAGENT).as_deref(), Some(SUBAGENT_UUID));
        // Files with no uuid in the name have no native id (not every tool keys
        // sessions this way).
        assert_eq!(native_id_of("/x/opencode.db#session-42"), None);
    }

    #[test]
    fn native_id_uppercase_is_normalized_to_lowercase() {
        let p = "/a/b/AB000000-0000-4000-8000-0000000000FF.jsonl";
        assert_eq!(
            native_id_of(p).as_deref(),
            Some("ab000000-0000-4000-8000-0000000000ff")
        );
    }

    #[test]
    fn resolve_by_full_native_id() {
        let c = mem();
        let sid = seed(&c, "codex", CODEX);
        let hits = resolve(&c, CODEX_UUID).unwrap();
        assert_eq!(hits.len(), 1, "full native uuid resolves");
        assert_eq!(hits[0].session_id, sid);
    }

    #[test]
    fn resolve_by_native_prefix_hex_and_dashed() {
        let c = mem();
        let sid = seed(&c, "claude-code", CLAUDE);
        // First-group (8 hex) prefix.
        let a = resolve(&c, "1b111111").unwrap();
        assert_eq!(a.len(), 1, "8-hex native prefix resolves");
        assert_eq!(a[0].session_id, sid);
        // Dashed prefix past the first group.
        let b = resolve(&c, "1b111111-1111").unwrap();
        assert_eq!(b.len(), 1, "dashed native prefix resolves");
        assert_eq!(b[0].session_id, sid);
    }

    #[test]
    fn resolve_still_matches_short_id_unchanged() {
        let c = mem();
        let sid = seed(&c, "codex", CODEX);
        // Full short id and a short-id prefix both resolve (existing behavior).
        assert_eq!(resolve(&c, &sid).unwrap().len(), 1);
        assert_eq!(resolve(&c, &sid[..6]).unwrap()[0].session_id, sid);
    }

    #[test]
    fn short_id_lookup_does_not_run_the_native_scan() {
        // A 12-hex, dash-free string is short-id-shaped and must never trigger a
        // native scan (which would be a needless path scan and could add a
        // spurious collision). Guard the gate that governs it directly.
        assert!(!looks_like_native_prefix("abcdef012345"));
        // ... while genuine native shapes do pass.
        assert!(looks_like_native_prefix("0a000000"));
        assert!(looks_like_native_prefix(
            "0a000000-0000-4000-8000-000000000001"
        ));
        assert!(looks_like_native_prefix("0a000000-0000"));
        // Non-hex, too short, or empty never look native.
        assert!(!looks_like_native_prefix("zzz"));
        assert!(!looks_like_native_prefix("a1"));
        assert!(!looks_like_native_prefix(""));
    }

    #[test]
    fn native_prefix_never_hides_an_existing_short_id_match() {
        // A plain-hex prefix that already matched a short id stays short-id-only:
        // even if some other session's native uuid also starts with those hex
        // digits, the plain-hex query keeps the established (short-id) result.
        let c = mem();
        // Seed a session whose SHORT id begins with the same 8 hex as another
        // session's native uuid, and the native one too.
        let native_path =
            "/home/u/.codex/sessions/2025/01/01/rollout-2025-01-01T00-00-00-deadbeef-0000-4000-8000-000000000009.jsonl";
        seed(&c, "codex", native_path);
        // Force a files row whose short id we control to start with "deadbeef".
        c.execute(
            "INSERT INTO files(path, session_id, tool, msg_count) VALUES('/synthetic', 'deadbeef1234', 'codex', 1)",
            [],
        )
        .unwrap();
        let hits = resolve(&c, "deadbeef").unwrap();
        // Short id matched, so the query stays short-id-only: exactly the one row.
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "deadbeef1234");
    }

    #[test]
    fn session_row_serializes_native_id_not_path() {
        let row = SessionRow {
            last_active: None,
            session_id: "abc123def456".into(),
            tool: "codex".into(),
            path: CODEX.into(),
            project: "proj".into(),
            title: "t".into(),
            started: None,
            msg_count: 3,
            kind: "main".into(),
            preview: None,
            summary: None,
            tags: None,
            archived: false,
            account: None,
            parent_id: None,
        };
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["id"], "abc123def456");
        assert_eq!(v["native_id"], CODEX_UUID, "native_id present in JSON");
        assert!(
            v.get("path").is_none(),
            "the absolute path is never serialized"
        );
        // A pathless/uuid-less session serializes native_id as null, never a guess.
        let mut row2 = row;
        row2.path = "/x/opencode.db#s1".into();
        let v2 = serde_json::to_value(&row2).unwrap();
        assert!(v2["native_id"].is_null());
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        c
    }

    fn session(c: &Connection, sid: &str, started: &str, ended: &str, msgs: i64) {
        c.execute(
            "INSERT INTO files(path, mtime, size, session_id, tool, started, ended, msg_count)
             VALUES (?1, 1, 1, ?2, 'codex', ?3, ?4, ?5)",
            params![format!("/{sid}.jsonl"), sid, started, ended, msgs],
        )
        .unwrap();
    }

    fn message(c: &Connection, sid: &str, role: &str, text: &str) {
        c.execute(
            "INSERT INTO messages(session_id, role, text) VALUES (?1, ?2, ?3)",
            params![sid, role, text],
        )
        .unwrap();
        c.execute(
            "INSERT INTO msgs(rowid, text) VALUES (?1, ?2)",
            params![c.last_insert_rowid(), text],
        )
        .unwrap();
    }

    fn ids(rows: &[SessionRow]) -> Vec<&str> {
        rows.iter().map(|r| r.session_id.as_str()).collect()
    }

    #[test]
    fn message_count_order_covers_sessions_beyond_the_limit_by_activity() {
        let c = conn();
        session(
            &c,
            "old-big",
            "2026-01-01T00:00:00+00:00",
            "2026-01-01T01:00:00+00:00",
            900,
        );
        session(
            &c,
            "new-small",
            "2026-10-01T00:00:00+00:00",
            "2026-10-01T01:00:00+00:00",
            3,
        );
        let by_msgs = recent_range(&c, 1, &ListQuery::default(), SessionSort::Messages).unwrap();
        assert_eq!(ids(&by_msgs), ["old-big"]);
        let by_activity =
            recent_range(&c, 1, &ListQuery::default(), SessionSort::LastActive).unwrap();
        assert_eq!(ids(&by_activity), ["new-small"]);
    }

    #[test]
    fn a_window_keeps_sessions_active_in_it_and_reads_browser_timestamps() {
        let c = conn();
        // Begun the day before, still going inside the window.
        session(
            &c,
            "resumed",
            "2026-10-22T20:00:00+00:00",
            "2026-10-23T09:00:00.5+00:00",
            5,
        );
        // Started exactly when the window opens (stored without fraction).
        session(
            &c,
            "edge",
            "2026-10-22T21:00:00+00:00",
            "2026-10-22T21:00:00+00:00",
            5,
        );
        session(
            &c,
            "before",
            "2026-10-20T10:00:00+00:00",
            "2026-10-20T11:00:00+00:00",
            5,
        );
        // A local (UTC+3) day 2026-10-23, as the browser sends it.
        let rows = recent_range(
            &c,
            50,
            &ListQuery {
                since: Some("2026-10-22T21:00:00.000Z"),
                until: Some("2026-10-23T20:59:59.999Z"),
                ..Default::default()
            },
            SessionSort::LastActive,
        )
        .unwrap();
        assert_eq!(ids(&rows), ["resumed", "edge"]);
    }

    #[test]
    fn search_applies_date_and_tag_before_the_limit() {
        let c = conn();
        session(
            &c,
            "recent",
            "2026-10-23T10:00:00+00:00",
            "2026-10-23T11:00:00+00:00",
            1,
        );
        session(
            &c,
            "old",
            "2026-01-01T10:00:00+00:00",
            "2026-01-01T11:00:00+00:00",
            1,
        );
        // The old session ranks first (two hits in one message).
        message(&c, "old", "user", "migration migration plan");
        message(&c, "recent", "user", "migration plan");
        add_tag(&c, "recent", "db").unwrap();
        let scope = SearchScope {
            since: Some("2026-10-01"),
            ..SearchScope::default()
        };
        let hits = search_scoped(&c, "migration", 1, None, None, scope).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].row.session_id, "recent");
        let scope = SearchScope {
            tag: Some("db"),
            ..SearchScope::default()
        };
        let hits = search_scoped(&c, "migration", 10, None, None, scope).unwrap();
        assert_eq!(
            hits.iter()
                .map(|h| h.row.session_id.as_str())
                .collect::<Vec<_>>(),
            ["recent"]
        );
    }

    #[test]
    fn multi_term_search_honours_roles_and_ranks_by_matches() {
        let c = conn();
        session(
            &c,
            "dense",
            "2026-01-01T10:00:00+00:00",
            "2026-01-01T11:00:00+00:00",
            4,
        );
        session(
            &c,
            "sparse",
            "2026-10-01T10:00:00+00:00",
            "2026-10-01T11:00:00+00:00",
            2,
        );
        session(
            &c,
            "toolonly",
            "2026-10-02T10:00:00+00:00",
            "2026-10-02T11:00:00+00:00",
            2,
        );
        for _ in 0..2 {
            message(&c, "dense", "user", "alpha topic");
            message(&c, "dense", "assistant", "beta topic");
        }
        message(&c, "sparse", "user", "alpha topic");
        message(&c, "sparse", "assistant", "beta topic");
        message(&c, "toolonly", "tool", "alpha beta");
        let rows =
            search_sessions_multi_terms(&c, &["alpha", "beta"], 10, None, SearchScope::default())
                .unwrap();
        // Most matches first; equal counts fall back to the more recent session.
        assert_eq!(ids(&rows), ["dense", "toolonly", "sparse"]);
        let roles = [Role::User, Role::Assistant];
        let scope = SearchScope {
            roles: Some(&roles),
            ..SearchScope::default()
        };
        let rows = search_sessions_multi_terms(&c, &["alpha", "beta"], 10, None, scope).unwrap();
        assert_eq!(ids(&rows), ["dense", "sparse"]);
    }

    #[test]
    fn the_instant_summary_version_only_moves_up() {
        let mut c = conn();
        let line = |c: &Connection, sid: &str, text: &str, kind: &str| {
            c.execute(
                "INSERT OR REPLACE INTO summaries(session_id, summary, created, kind) VALUES (?1, ?2, 'now', ?3)",
                params![sid, text, kind],
            )
            .unwrap();
        };
        let count = |c: &Connection, kind: &str| -> i64 {
            c.query_row(
                "SELECT count(*) FROM summaries WHERE kind = ?1",
                [kind],
                |r| r.get(0),
            )
            .unwrap()
        };
        // First run: nothing recorded, the version is claimed.
        assert!(claim_auto_summaries(&mut c, 2).unwrap());
        assert_eq!(stored_auto_summary_version(&c).unwrap(), 2);
        line(&c, "a", "v2 line", "auto");
        line(&c, "b", "model line", "llm");
        // An older binary leaves the newer lines alone and stays out.
        assert!(!claim_auto_summaries(&mut c, 1).unwrap());
        assert_eq!(
            (count(&c, "auto"), stored_auto_summary_version(&c).unwrap()),
            (1, 2)
        );
        // The same version again changes nothing.
        assert!(claim_auto_summaries(&mut c, 2).unwrap());
        assert_eq!(count(&c, "auto"), 1);
        // A newer one drops the instant lines for re-deriving; model lines stay.
        assert!(claim_auto_summaries(&mut c, 3).unwrap());
        assert_eq!((count(&c, "auto"), count(&c, "llm")), (0, 1));
        assert_eq!(stored_auto_summary_version(&c).unwrap(), 3);
    }

    #[test]
    fn instant_summaries_fill_in_but_never_replace_a_model_one() {
        let mut c = conn();
        for sid in ["a", "b"] {
            session(&c, sid, "2026-10-01T10:00:00Z", "2026-10-01T11:00:00Z", 2);
            message(&c, sid, "user", "Fix the CSV parser please");
            message(
                &c,
                sid,
                "assistant",
                "Fixed: quoted fields now parse and all tests pass.",
            );
        }
        set_summary(&c, "b", "model-written synopsis").unwrap();
        assert_eq!(
            backfill_auto_summaries(&mut c).unwrap(),
            1,
            "only the session without one"
        );
        let get = |sid: &str| -> (String, String) {
            c.query_row(
                "SELECT summary, kind FROM summaries WHERE session_id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        };
        let (line, kind) = get("a");
        assert_eq!(kind, "auto");
        assert!(
            line.starts_with("Fix the CSV parser please → Fixed: quoted fields"),
            "{line}"
        );
        assert_eq!(get("b"), ("model-written synopsis".into(), "llm".into()));
        // Reindexing rewrites an instant line, never a model one.
        store_auto_summary(&c, "a", Some("newer line")).unwrap();
        store_auto_summary(&c, "b", Some("newer line")).unwrap();
        assert_eq!(get("a").0, "newer line");
        assert_eq!(get("b").0, "model-written synopsis");
        // `summarize` still offers the session that only has an instant line.
        let todo: Vec<String> = unsummarized(&c, 10, None)
            .unwrap()
            .into_iter()
            .map(|r| r.session_id)
            .collect();
        assert_eq!(todo, vec!["a".to_string()]);
    }
}

#[cfg(test)]
mod snippet_tests {
    use super::*;

    #[test]
    fn saved_snippet_inherits_tool_and_start_of_a_live_session() {
        let c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        c.execute(
            "INSERT INTO files(path, mtime, size, session_id, tool, started)
             VALUES ('/s.jsonl', 1, 1, 'sid1', 'codex', '2026-10-01T10:00:00+00:00')",
            [],
        )
        .unwrap();
        let id = save_snippet(
            &c,
            &NewSnippet {
                session_id: "sid1",
                message_idx: 3,
                title: "t",
                content: "body",
                ..Default::default()
            },
        )
        .unwrap();
        let (tool, ts): (Option<String>, Option<String>) = c
            .query_row(
                "SELECT tool, message_ts FROM snippets WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tool.as_deref(), Some("codex"));
        assert_eq!(ts.as_deref(), Some("2026-10-01T10:00:00+00:00"));
    }

    #[test]
    fn reopening_writes_nothing_once_columns_and_backfill_are_done() {
        let c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        ensure_snippets_table(&c).unwrap();
        let before = c.total_changes();
        ensure_snippets_table(&c).unwrap();
        assert_eq!(c.total_changes(), before);
    }
}

#[cfg(test)]
mod edits_tests {
    use super::*;

    fn conn_with_edits() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE edits(session_id TEXT NOT NULL, path TEXT NOT NULL,
                 kind TEXT NOT NULL, ts TEXT, snippet TEXT NOT NULL);
             CREATE INDEX idx_edits_path ON edits(path);
             CREATE TABLE files(session_id TEXT, project TEXT);",
        )
        .unwrap();
        c
    }

    fn add(c: &Connection, sid: &str, path: &str, kind: &str, ts: &str, snip: &str) {
        c.execute(
            "INSERT INTO edits(session_id, path, kind, ts, snippet) VALUES(?1,?2,?3,?4,?5)",
            params![sid, path, kind, ts, snip],
        )
        .unwrap();
    }

    #[test]
    fn edits_for_returns_a_files_edits_by_suffix_newest_first() {
        let c = conn_with_edits();
        add(
            &c,
            "s1",
            "/home/me/proj/src/auth.rs",
            "edit",
            "2026-06-08T10:00:00Z",
            "let a = 1;",
        );
        add(
            &c,
            "s2",
            "/home/me/proj/src/auth.rs",
            "write",
            "2026-06-09T10:00:00Z",
            "fn main() {}",
        );
        add(
            &c,
            "s3",
            "/home/me/proj/src/other.rs",
            "edit",
            "2026-06-10T10:00:00Z",
            "nope",
        );

        // A relative path finds the absolute stored path by suffix, like `trace`.
        let hits = edits_for(&c, "src/auth.rs", 50).unwrap();

        assert_eq!(hits.len(), 2, "both auth.rs edits, not other.rs");
        assert_eq!(hits[0].kind, "write", "newest edit first");
        assert!(hits[0].snippet.contains("fn main()"));
        assert_eq!(hits[1].kind, "edit");
    }

    #[test]
    fn index_one_persists_a_sessions_edits() {
        use crate::model::{EditEvent, EditKind, Session};
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();

        let session = Session {
            id: "sx".into(),
            tool: "claude-code",
            path: "/store/sx.jsonl".into(),
            project: "/proj".into(),
            started: None,
            ended: None,
            title: "t".into(),
            subagent: false,
            parent_id: None,
            messages: vec![],
            touched: vec!["/proj/src/auth.rs".into()],
            edits: vec![EditEvent {
                path: "/proj/src/auth.rs".into(),
                kind: EditKind::Write,
                snippet: "fn main() {}".into(),
                ts: None,
            }],
        };

        let tx = c.transaction().unwrap();
        index_one(&tx, &session, "/store/sx.jsonl", 0, 0, ToolOutputMode::Full).unwrap();
        tx.commit().unwrap();

        let hits = edits_for(&c, "src/auth.rs", 50).unwrap();
        assert_eq!(hits.len(), 1, "the session's one edit was persisted");
        assert_eq!(hits[0].session_id, "sx");
        assert_eq!(hits[0].kind, "write");
        assert!(hits[0].snippet.contains("fn main()"));
    }

    #[test]
    fn forget_removes_a_sessions_edits() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        c.execute(
            "INSERT INTO files(path, session_id, tool, mtime, size) VALUES('/store/s.jsonl','s1','claude-code',0,0)",
            [],
        )
        .unwrap();
        c.execute(
            "INSERT INTO edits(session_id,path,kind,ts,snippet) VALUES('s1','/proj/a.rs','write',NULL,'x')",
            [],
        )
        .unwrap();
        assert_eq!(edits_for(&c, "a.rs", 10).unwrap().len(), 1);

        forget(&mut c, "s1").unwrap();

        assert!(
            edits_for(&c, "a.rs", 10).unwrap().is_empty(),
            "forget must remove the session's edits, not orphan them"
        );
    }

    /// One web chat written the way a sync writes it: `key` is the export file
    /// plus the chat's own id, `id` the session id the reader gives it.
    fn index_web_chat(conn: &mut Connection, key: &str, id: &str, text: &str) {
        use crate::model::{Message, Role, Session};
        let session = Session {
            id: id.to_string(),
            tool: "claude-web",
            path: PathBuf::from(key.replace('\u{1f}', "#")),
            project: "web".into(),
            started: None,
            ended: None,
            title: "a web chat".into(),
            subagent: false,
            parent_id: None,
            messages: vec![Message::plain(Role::User, text, None)],
            touched: vec!["src/upload_spec.rs".into()],
            edits: vec![],
        };
        let tx = conn.transaction().unwrap();
        index_one(&tx, &session, key, 5, 0, ToolOutputMode::Full).unwrap();
        tx.commit().unwrap();
    }

    fn marked(c: &Connection, table: &str, session_id: &str) -> i64 {
        c.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
            params![session_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn mark(c: &Connection, table: &str, session_id: &str) {
        let sql = match table {
            "favorites" => "INSERT INTO favorites(session_id, created_at) VALUES(?1,'2026-01-01T00:00:00+00:00')",
            "tags" => "INSERT INTO tags(session_id, tag) VALUES(?1,'verified')",
            "notes" => "INSERT INTO notes(session_id, note, updated) VALUES(?1,'keep','2026-01-01T00:00:00+00:00')",
            "summaries" => "INSERT INTO summaries(session_id, summary, created, kind) VALUES(?1,'s','2026-01-01T00:00:00+00:00','llm')",
            _ => "INSERT INTO snippets(session_id, message_idx, title, content, created_at) VALUES(?1,0,'fix','cargo sync','2026-01-01T00:00:00+00:00')",
        };
        c.execute(sql, params![session_id]).unwrap();
    }

    #[test]
    fn a_reparsed_id_takes_the_users_marks_with_it() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        let key = "C:\\exports\\claude.json\u{1f}chat-1".to_string();
        index_web_chat(&mut c, &key, "old-id", "read by the old reader");
        for table in ["favorites", "tags", "notes", "summaries", "snippets"] {
            mark(&c, table, "old-id");
        }

        // The reader fix that respells the id: same key, new session id.
        index_web_chat(&mut c, &key, "new-id", "read again");

        let listed: String = c
            .query_row(
                "SELECT session_id FROM files WHERE path = ?1",
                [key.as_str()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(listed, "new-id");
        for table in ["favorites", "tags", "notes", "summaries", "snippets"] {
            assert_eq!(
                marked(&c, table, "new-id"),
                1,
                "the {table} followed the chat"
            );
            assert_eq!(
                marked(&c, table, "old-id"),
                0,
                "no mark stays on the old id"
            );
        }
        assert_eq!(
            marked(&c, "messages", "old-id"),
            0,
            "the transcript the old id held is re-read under the new one, not left behind"
        );
        assert_eq!(marked(&c, "touched", "old-id"), 0);
        assert_eq!(marked(&c, "messages", "new-id"), 1);
    }

    #[test]
    fn a_re_exported_chat_is_not_treated_as_a_deleted_session() {
        let dir = tempfile::tempdir().unwrap();
        let older = dir.path().join("claude-2026-03.json");
        let newer = dir.path().join("claude-2026-06.json");
        std::fs::write(&older, b"[]").unwrap();
        std::fs::write(&newer, b"[]").unwrap();
        // A third export of the same chat whose file the user has since deleted.
        let deleted = format!(
            "{}\u{1f}chat-1",
            dir.path().join("claude-2026-01.json").display()
        );

        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        let old_key = format!("{}\u{1f}chat-1", older.display());
        let new_key = format!("{}\u{1f}chat-1", newer.display());
        index_web_chat(&mut c, &old_key, "kept-id", "the first export");
        index_web_chat(&mut c, &deleted, "dropped-id", "an export since removed");
        index_web_chat(&mut c, &new_key, "new-id", "the newest export");
        mark(&c, "favorites", "kept-id");

        // The store lists this chat at the newest export only, so the other two
        // keys are gone while the conversation itself is anything but.
        let archived =
            archive_or_prune(&c, "claude-web", std::slice::from_ref(&new_key), true, None).unwrap();
        assert_eq!(
            archived, 0,
            "a second listing of a live chat is not archived as a deleted session"
        );
        let rows: i64 = c
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "only the newest export lists the chat");
        assert_eq!(
            marked(&c, "favorites", "new-id"),
            1,
            "the star follows the conversation to the id it answers to now"
        );
        assert_eq!(
            marked(&c, "messages", "kept-id"),
            1,
            "its export file is still on disk, so that copy of the transcript waits with it"
        );
        assert_eq!(
            marked(&c, "messages", "dropped-id"),
            0,
            "with the file deleted too, the rows are a leftover, not a record"
        );
    }

    #[test]
    fn edits_for_is_deterministic_when_timestamps_tie() {
        let c = conn_with_edits();
        add(&c, "s1", "/p/a.rs", "edit", "2026-01-01T00:00:00Z", "first");
        add(
            &c,
            "s2",
            "/p/a.rs",
            "write",
            "2026-01-01T00:00:00Z",
            "second",
        );
        let hits = edits_for(&c, "a.rs", 10).unwrap();
        // Equal ts -> deterministic tie-break by rowid DESC (latest insert first).
        assert_eq!(hits[0].snippet, "second");
        assert_eq!(hits[1].snippet, "first");
    }

    #[test]
    fn edits_for_session_returns_only_that_sessions_edits() {
        let c = conn_with_edits();
        add(
            &c,
            "s1",
            "/p/a.rs",
            "edit",
            "2026-01-01T00:00:00Z",
            "s1-edit",
        );
        add(
            &c,
            "s2",
            "/p/a.rs",
            "write",
            "2026-02-01T00:00:00Z",
            "s2-edit",
        );
        // Scoped by exact session + path, so one session's edits can never be
        // starved by another's under a shared cap.
        let hits = edits_for_session(&c, "s1", "/p/a.rs", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "s1-edit");
    }

    #[test]
    fn edits_for_session_reads_a_relative_path_against_the_session_folder() {
        let c = conn_with_edits();
        c.execute("INSERT INTO files VALUES ('s1', '/proj/api')", [])
            .unwrap();
        let ts = "2026-01-01T00:00:00Z";
        add(&c, "s1", "/proj/other/README.md", "edit", ts, "other");
        add(&c, "s1", "README.md", "edit", ts, "own");
        let snips = |q: &str| -> Vec<String> {
            edits_for_session(&c, "s1", q, 10)
                .unwrap()
                .into_iter()
                .map(|e| e.snippet)
                .collect()
        };
        assert_eq!(snips("/proj/other/README.md"), ["other"]);
        assert_eq!(snips("/proj/api/README.md"), ["own"]);
        assert_eq!(snips("api/README.md"), ["own"]);
    }

    #[test]
    fn edits_for_session_matches_every_spelling_of_the_path() {
        let c = conn_with_edits();
        // One session edited the file under two path spellings (abs + relative).
        add(
            &c,
            "s1",
            "/proj/src/auth.rs",
            "edit",
            "2026-01-01T00:00:00Z",
            "abs",
        );
        add(
            &c,
            "s1",
            "src/auth.rs",
            "write",
            "2026-01-02T00:00:00Z",
            "rel",
        );
        add(
            &c,
            "s2",
            "/other/auth.rs",
            "edit",
            "2026-01-03T00:00:00Z",
            "different-file",
        );
        // Suffix match scoped to s1 must catch BOTH spellings - never miss edits
        // just because sessions_for_file's GROUP BY picked the other spelling.
        let hits = edits_for_session(&c, "s1", "src/auth.rs", 10).unwrap();
        assert_eq!(hits.len(), 2, "all of s1's edits to the file, any spelling");
    }

    #[test]
    fn index_redacts_secrets_in_messages_and_edit_snippets() {
        use crate::model::{EditEvent, EditKind, Message, Role, Session};
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        let session = Session {
            id: "sx".into(),
            tool: "claude-code",
            path: "/s.jsonl".into(),
            project: "/p".into(),
            started: None,
            ended: None,
            title: "title with AKIAIOSFODNN7EXAMPLE in it".into(),
            subagent: false,
            parent_id: None,
            messages: vec![Message {
                role: Role::User,
                text: format!(
                    "my key is {} ok",
                    concat!("s", "k-abcdef012345678901234567890123")
                ),
                ts: None,
                tool: None,
                is_rewound: false,
            }],
            touched: vec!["/p/a.rs".into()],
            edits: vec![EditEvent {
                path: "/p/a.rs".into(),
                kind: EditKind::Write,
                snippet: format!(
                    "const T = \"{}\";",
                    concat!("gh", "p_016C7f9aBcDeFgHiJkLmNoPqRsTuVwXyZ012")
                ),
                ts: None,
            }],
        };
        let tx = c.transaction().unwrap();
        index_one(&tx, &session, "/s.jsonl", 0, 0, ToolOutputMode::Full).unwrap();
        tx.commit().unwrap();

        let msg: String = c
            .query_row("SELECT text FROM messages WHERE session_id='sx'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !msg.contains(concat!("s", "k-abcdef")),
            "message secret redacted: {msg}"
        );
        assert!(msg.contains("[redacted:openai]"), "{msg}");
        let snip = edits_for(&c, "a.rs", 10).unwrap()[0].snippet.clone();
        assert!(
            !snip.contains(concat!("gh", "p_016C")),
            "edit secret redacted: {snip}"
        );
        assert!(snip.contains("[redacted:github]"), "{snip}");
        // Title is durable (copied into archives) - must be redacted too.
        let title: String = c
            .query_row("SELECT title FROM files WHERE session_id='sx'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !title.contains("AKIAIOSFODNN7EXAMPLE"),
            "title secret redacted: {title}"
        );
        // LLM synopsis can echo a secret into the durable summaries table.
        set_summary(
            &c,
            "sx",
            &format!(
                "we set {} as the key",
                concat!("s", "k-abcdef012345678901234567890123")
            ),
        )
        .unwrap();
        let sum: String = c
            .query_row(
                "SELECT summary FROM summaries WHERE session_id='sx'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !sum.contains(concat!("s", "k-abcdef")),
            "summary secret redacted: {sum}"
        );
    }

    #[test]
    fn evidence_for_assembles_sessions_with_their_edits_newest_first() {
        use crate::model::{EditEvent, EditKind, Session};
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();

        for (sid, store, started, kind, snip) in [
            (
                "old",
                "/store/old.jsonl",
                "2026-06-01T00:00:00Z",
                EditKind::Edit,
                "v1",
            ),
            (
                "new",
                "/store/new.jsonl",
                "2026-06-09T00:00:00Z",
                EditKind::Write,
                "v2",
            ),
        ] {
            let started = chrono::DateTime::parse_from_rfc3339(started)
                .unwrap()
                .with_timezone(&chrono::Utc);
            let session = Session {
                id: sid.into(),
                tool: "claude-code",
                path: store.into(),
                project: "/proj".into(),
                started: Some(started),
                ended: None,
                title: format!("{sid} title"),
                subagent: false,
                parent_id: None,
                messages: vec![],
                touched: vec!["/proj/src/a.rs".into()],
                edits: vec![EditEvent {
                    path: "/proj/src/a.rs".into(),
                    kind,
                    snippet: snip.into(),
                    ts: None,
                }],
            };
            let tx = c.transaction().unwrap();
            index_one(&tx, &session, store, 0, 0, ToolOutputMode::Full).unwrap();
            tx.commit().unwrap();
        }

        let hist = evidence_for(&c, "src/a.rs", 50).unwrap();
        assert_eq!(hist.path, "src/a.rs");
        assert_eq!(hist.sessions.len(), 2, "both sessions that edited the file");
        assert_eq!(
            hist.sessions[0].session.session_id, "new",
            "newest session first"
        );
        assert_eq!(hist.sessions[0].edits.len(), 1);
        assert_eq!(hist.sessions[0].edits[0].snippet, "v2");
        assert_eq!(hist.sessions[1].session.session_id, "old");
    }
}

#[cfg(test)]
mod commit_tests {
    use super::*;

    #[test]
    fn test_is_hex_sha_rules() {
        assert!(is_hex_sha("7f2a1b9"));
        assert!(is_hex_sha("ca54746"));
        assert!(is_hex_sha("0feb3fdd929bcd6649e0e7c5a688cf7dd012ef21"));
        assert!(!is_hex_sha("123456")); // too short (<7)
        assert!(!is_hex_sha("33843667")); // pure numbers < 40 chars
        assert!(!is_hex_sha("5221473")); // pure numbers < 40 chars
        assert!(!is_hex_sha("7f2a1b9z")); // invalid hex character 'z'
    }

    #[test]
    fn test_extract_commits_from_various_formats() {
        let text = r#"
I made the commit:
[main 7f2a1b9] fix: resolve race condition in token refresh
 2 files changed, 10 insertions(+)

Next is another commit:
[feat/auth ca54746]
feat: implement oauth token exchange
1 file changed, 50 insertions(+)

Random numbers: [4844312, 5221473] and [[PubMed 33843667]] should be ignored.
"#;
        let commits = extract_commits_from_text(text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].0, "7f2a1b9");
        assert_eq!(
            commits[0].1.as_deref(),
            Some("fix: resolve race condition in token refresh")
        );
        assert_eq!(commits[1].0, "ca54746");
        assert_eq!(
            commits[1].1.as_deref(),
            Some("feat: implement oauth token exchange")
        );
    }

    /// Quoting somebody's history is not authoring their commit, so nothing a
    /// session *read* is credited to it - not a `git log` line, not a link.
    #[test]
    fn mentions_of_a_commit_are_not_authorship() {
        let text = r#"
commit 0feb3fdd929bcd6649e0e7c5a688cf7dd012ef21
Author: Test <test@example.com>
Date:   Sat Oct 31 12:00:00 2026
    commit 7f2a1b9c8d0e1f2a3b4c5d6e7f8091a2b3c4d5e6
    Merge: 1a2b3c4 5d6e7f8

See https://github.com/AlexZander85/crypto/commit/0623bd8 for details.
"#;
        assert_eq!(extract_commits_from_text(text), Vec::new());
    }

    #[test]
    fn test_session_commit_lookup_and_prefix() {
        let conn = Connection::open_in_memory().unwrap();
        create_cache_schema(&conn).unwrap();

        conn.execute(
            "INSERT INTO files(path, mtime, size, session_id, tool, project, title, msg_count, kind, reader)
             VALUES('p1', 1, 1, 's1', 'claude-code', '/proj', 'Fix auth', 5, 'main', 0)",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO session_commits(session_id, commit_sha, message, timestamp) VALUES('s1', '7f2a1b9c8d0e1f', 'fix auth token', 1700000000)",
            []
        ).unwrap();

        // 1. Exact match
        let res = find_session_by_commit(&conn, "7f2a1b9c8d0e1f").unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].session_id, "s1");

        // 2. Short SHA prefix of full SHA
        let res = find_session_by_commit(&conn, "7f2a1b9").unwrap();
        assert_eq!(res[0].session_id, "s1");

        // 3. Stored is short SHA, query is full SHA
        conn.execute(
            "INSERT INTO session_commits(session_id, commit_sha, message, timestamp) VALUES('s1', 'ca54746', 'batch fix', 1700000000)",
            []
        ).unwrap();
        let res = find_session_by_commit(&conn, "ca547461234567890abcdef").unwrap();
        assert_eq!(res[0].session_id, "s1");

        // 4. Unknown SHA, and a non-hex string that would otherwise read as
        // LIKE wildcards
        assert!(find_session_by_commit(&conn, "fffffff").unwrap().is_empty());
        assert!(find_session_by_commit(&conn, "s1%").unwrap().is_empty());

        // 5. A SHA two sessions both claim comes back as both, newest first
        conn.execute(
            "INSERT INTO files(path, mtime, size, session_id, tool, project, title, started, msg_count, kind, reader)
             VALUES('p2', 1, 1, 's2', 'claude-code', '/proj', 'Later chat', '2026-10-05T00:00:00+00:00', 3, 'main', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_commits(session_id, commit_sha, message, timestamp) VALUES('s2', '7f2a1b9c8d0e1f', NULL, 1700000100)",
            [],
        )
        .unwrap();
        let res = find_session_by_commit(&conn, "7f2a1b9c8d0e1f").unwrap();
        let ids: Vec<&str> = res.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, vec!["s2", "s1"], "both candidates, newest first");

        // 6. Get all commits for session
        let list = get_session_commits(&conn, "s1").unwrap();
        assert_eq!(list.len(), 2);
    }
}

#[cfg(test)]
mod moved_file_tests {
    use super::*;

    /// Folders get renamed. A session recorded `~/Project/lunch/diag.py`; the
    /// folder is now `~/Project/slack`, so tracing the file by the path it has
    /// TODAY found nothing and said "no session touched a file matching" - about
    /// a file whose whole history was sitting in the index under its old name.
    ///
    /// The basename is the part that survives a move, so a full path that finds
    /// nothing falls back to it, and the caller is told the match was by name so
    /// it can say the folder has moved.
    #[test]
    fn a_renamed_folder_still_traces_by_file_name() {
        assert_eq!(
            basename_fallback("/Users/b/Project/slack/diag.py").as_deref(),
            Some("diag.py"),
            "a full path falls back to its file name"
        );
        // Already a bare name: there is nothing to fall back to, and retrying
        // the same query would just repeat the miss.
        assert_eq!(basename_fallback("diag.py"), None);
        assert_eq!(basename_fallback(""), None);
        // A trailing slash names a directory, not a file to trace.
        assert_eq!(basename_fallback("/Users/b/Project/slack/"), None);
    }
}

#[cfg(test)]
mod legacy_migration_tests {
    use super::*;

    /// The migration looked for the old directories under `dirs::data_dir()`
    /// no matter where the index was actually going, and then RENAMED what it
    /// found into that destination. With `SESSIONWIKI_DATA` pointed at a temp
    /// dir - which eight test files do - a machine still holding
    /// `~/.local/share/sessiondex` would have had its real index moved into
    /// that temp dir and deleted with it. The comment above says the tags,
    /// notes and summaries in there are not rebuildable.
    #[test]
    fn a_legacy_index_is_only_looked_for_beside_the_new_one() {
        let under_home = std::path::Path::new("/home/someone/.local/share/sessionwiki");
        let got = legacy_candidates(under_home);
        assert_eq!(
            got,
            vec![
                std::path::PathBuf::from("/home/someone/.local/share/sessiondex"),
                std::path::PathBuf::from("/home/someone/.local/share/session-atlas"),
            ],
            "the normal case must keep working"
        );

        let redirected = std::path::Path::new("/tmp/sessionwiki-test-xyz");
        for c in legacy_candidates(redirected) {
            assert!(
                c.starts_with("/tmp"),
                "a redirected run reached outside its own tree: {}",
                c.display()
            );
        }
    }

    #[test]
    fn a_destination_with_no_parent_offers_nothing_to_migrate() {
        assert!(legacy_candidates(std::path::Path::new("/")).is_empty());
    }
}

#[cfg(test)]
mod codex_history_tests;

#[cfg(test)]
mod embedder_hook_tests {
    use super::*;
    use crate::adapters::{Adapter, Discovered, Store};
    use crate::model::{Message, Role, Session};
    use std::path::Path;

    /// A shared-store adapter an embedding program could supply: it lists only
    /// the keys under its own prefix and reconciles only that prefix.
    struct FakeStore {
        keys: Vec<(String, i64)>,
        scope: Option<String>,
    }

    impl Adapter for FakeStore {
        fn name(&self) -> &'static str {
            "mjolnir"
        }
        fn root(&self) -> Option<PathBuf> {
            // Any existing directory: the reconciliation guard only asks
            // whether the store root is still there.
            Some(std::env::current_dir().unwrap())
        }
        fn discover(&self) -> Discovered {
            Discovered {
                files: Vec::new(),
                had_error: false,
            }
        }
        fn parse(&self, _path: &Path) -> Result<Session> {
            anyhow::bail!("shared store")
        }
        fn store(&self) -> Option<Store> {
            Some(Store {
                keys: self.keys.clone(),
                files: Vec::new(),
                had_error: false,
            })
        }
        fn parse_key(&self, key: &str) -> Result<Session> {
            Ok(Session {
                id: key.rsplit('/').next().unwrap().to_string(),
                tool: "mjolnir",
                path: PathBuf::from(key),
                project: "/proj".into(),
                started: None,
                ended: None,
                title: "a restored session".into(),
                subagent: false,
                parent_id: None,
                messages: vec![Message {
                    role: Role::User,
                    text: "make the tests green".into(),
                    ts: None,
                    tool: None,
                    is_rewound: false,
                }],
                touched: vec![],
                edits: vec![],
            })
        }
        fn reconcile_scope(&self) -> Option<String> {
            self.scope.clone()
        }
    }

    fn insert_live_row(c: &Connection, path: &str, sid: &str) {
        c.execute(
            "INSERT INTO files(path, session_id, tool, mtime, size, project, title, msg_count, kind)
             VALUES(?1, ?2, 'mjolnir', 0, 0, '/proj', 't', 1, 'session')",
            params![path, sid],
        )
        .unwrap();
        c.execute(
            "INSERT INTO messages(session_id, role, text) VALUES(?1,'user','hello')",
            params![sid],
        )
        .unwrap();
    }

    fn archived_at(c: &Connection, path: &str) -> Option<String> {
        c.query_row(
            "SELECT archived_at FROM files WHERE path = ?1",
            params![path],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Two installations of one tool share a tool name and one index. A sync
    /// driven by the first must not archive the second's rows just because it
    /// never lists them.
    #[test]
    fn reconcile_scope_limits_archiving_to_the_adapters_own_keys() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        insert_live_row(&c, "/data/one/sess-a", "sa");
        insert_live_row(&c, "/data/two/sess-b", "sb");

        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(FakeStore {
            keys: Vec::new(),
            scope: Some("/data/one/".to_string()),
        })];
        sync_with(&mut c, &adapters, None).unwrap();

        assert!(
            archived_at(&c, "/data/one/sess-a").is_some(),
            "the in-scope row the adapter no longer lists must be archived"
        );
        assert!(
            archived_at(&c, "/data/two/sess-b").is_none(),
            "the other installation's row must be left live"
        );
    }

    /// Without a scope the adapter still speaks for every row of its tool.
    #[test]
    fn an_unscoped_adapter_still_archives_every_row_of_its_tool() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        insert_live_row(&c, "/data/one/sess-a", "sa");
        insert_live_row(&c, "/data/two/sess-b", "sb");

        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(FakeStore {
            keys: Vec::new(),
            scope: None,
        })];
        sync_with(&mut c, &adapters, None).unwrap();

        assert!(archived_at(&c, "/data/one/sess-a").is_some());
        assert!(archived_at(&c, "/data/two/sess-b").is_some());
    }

    #[test]
    fn a_shared_store_restores_an_archived_row_without_a_changed_token() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        let key = "/data/one/sess-a";
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(FakeStore {
            keys: vec![(key.to_string(), 42)],
            scope: Some("/data/one/".to_string()),
        })];
        sync_with(&mut c, &adapters, None).unwrap();
        let message_id: i64 = c
            .query_row(
                "SELECT id FROM messages WHERE session_id = 'sess-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        archive_session(&c, key, "sess-a").unwrap();

        sync_with(&mut c, &adapters, Some(100)).unwrap();

        assert_eq!(archived_at(&c, key), None);
        assert_eq!(
            c.query_row(
                "SELECT id FROM messages WHERE session_id = 'sess-a'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            message_id,
            "unchanged transcripts need no reparse"
        );
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM archive WHERE session_id = 'sess-a'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn a_file_store_restores_an_archived_row_without_changed_file_metadata() {
        let home = tempfile::tempdir().unwrap();
        let path = home
            .path()
            .join("sessions")
            .join("2026")
            .join("01")
            .join("01")
            .join("rollout-2026-01-01T10-00-00-abc.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"cwd\":\"/repo\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"restic\"}}\n",
        )
        .unwrap();
        let adapters: Vec<Box<dyn Adapter>> =
            vec![Box::new(crate::adapters::Codex::in_home(home.path()))];
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        sync_with(&mut c, &adapters, None).unwrap();
        let key = path.to_string_lossy();
        let sid: String = c
            .query_row(
                "SELECT session_id FROM files WHERE path = ?1",
                [key.as_ref()],
                |row| row.get(0),
            )
            .unwrap();
        let message_id: i64 = c
            .query_row(
                "SELECT id FROM messages WHERE session_id = ?1 LIMIT 1",
                [&sid],
                |row| row.get(0),
            )
            .unwrap();
        archive_session(&c, &key, &sid).unwrap();

        sync_with(&mut c, &adapters, Some(i64::MAX)).unwrap();

        assert_eq!(archived_at(&c, &key), None);
        assert_eq!(
            c.query_row(
                "SELECT id FROM messages WHERE session_id = ?1 LIMIT 1",
                [&sid],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            message_id,
            "unchanged files need no reparse"
        );

        archive_session(&c, &key, &sid).unwrap();
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"new content\"}}\n").unwrap();
        sync_with(&mut c, &adapters, Some(i64::MAX)).unwrap();
        assert_eq!(archived_at(&c, &key), None);
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM messages WHERE session_id = ?1 AND text = 'new content'",
                [&sid],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1,
            "changed archived files must be reparsed"
        );
    }

    /// "Recent" is about activity. Ordering by when a session STARTED put one
    /// resumed every day for three months below everything begun this week -
    /// on the machine this was found, eleven sessions active in the last day
    /// were buried that way, including the one asking.
    #[test]
    fn recent_orders_by_last_activity_not_by_start() {
        let c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        for (id, started, ended) in [
            ("resumed", "2026-06-01T00:00:00Z", "2026-09-30T01:00:00Z"),
            ("fresh", "2026-09-20T00:00:00Z", "2026-09-20T01:00:00Z"),
        ] {
            c.execute(
                "INSERT INTO files(path, mtime, size, session_id, tool, started, ended, msg_count, kind)
                 VALUES(?1, 0, 0, ?1, 'codex', ?2, ?3, 1, 'main')",
                rusqlite::params![id, started, ended],
            )
            .unwrap();
        }
        let rows = recent(&c, 10, None, None, None, false).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
        assert_eq!(ids, ["resumed", "fresh"]);
        assert_eq!(rows[0].last_active.as_deref(), Some("2026-09-30T01:00:00Z"));
        assert_eq!(rows[0].started.as_deref(), Some("2026-06-01T00:00:00Z"));
    }

    /// The point of `sync_with`: an embedding program indexes its own sessions
    /// with its own adapter, which is in no built-in registry.
    #[test]
    fn sync_with_indexes_a_session_from_a_supplied_adapter() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();

        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(FakeStore {
            keys: vec![("/data/one/sess-a".to_string(), 42)],
            scope: Some("/data/one/".to_string()),
        })];
        sync_with(&mut c, &adapters, None).unwrap();

        let rows = recent(&c, 10, Some("mjolnir"), None, None, false).unwrap();
        assert_eq!(rows.len(), 1, "the supplied adapter's session was indexed");
        assert_eq!(rows[0].session_id, "sess-a");
        assert_eq!(rows[0].title, "a restored session");
        assert_eq!(rows[0].msg_count, 1);
        assert!(!rows[0].archived, "a listed session stays live");
    }

    /// A row whose tool only an embedder's adapter knows still shows that
    /// tool's name, rather than the "unknown" the registry lookup used to give.
    #[test]
    fn a_session_keeps_its_own_tool_name_when_no_adapter_is_registered() {
        let mut c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();

        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(FakeStore {
            keys: vec![("/data/one/sess-a".to_string(), 42)],
            scope: Some("/data/one/".to_string()),
        })];
        sync_with(&mut c, &adapters, None).unwrap();

        let rows = recent(&c, 10, Some("mjolnir"), None, None, false).unwrap();
        assert!(
            crate::adapters::by_name("mjolnir").is_none(),
            "the built-in registry must not know this tool, or the test proves nothing"
        );
        let session = session_from_index(&c, &rows[0]).unwrap();
        assert_eq!(session.tool, "mjolnir");
    }

    /// The interner hands back one leaked string per name, however often it is
    /// asked, so the leak is bounded by the number of tool names.
    #[test]
    fn interning_a_tool_name_twice_yields_the_same_string() {
        let first = interned_tool("a-tool-no-adapter-knows");
        let second = interned_tool(&String::from("a-tool-no-adapter-knows"));
        assert_eq!(first, "a-tool-no-adapter-knows");
        assert!(std::ptr::eq(first, second));
    }
}

#[cfg(test)]
mod tool_output_tests {
    use super::*;
    use crate::model::{Message, Role, Session, ToolEvent, ToolSummary};
    use std::cell::Cell;
    use std::rc::Rc;

    const OUTPUT: &str = "fulloutputneedle: private tool result\nsecond diagnostic";

    fn tool_session(id: &str, path: &str) -> Session {
        Session {
            id: id.into(),
            tool: "codex",
            path: path.into(),
            project: "/project".into(),
            started: None,
            ended: None,
            title: "tool output test".into(),
            subagent: false,
            parent_id: None,
            messages: vec![Message {
                role: Role::Tool,
                text: "→ Bash(cargo test) ⇒ ok · 2 lines".into(),
                ts: None,
                is_rewound: false,
                tool: Some(ToolEvent::Summary(ToolSummary {
                    name: "Bash".into(),
                    args: serde_json::json!({"command": "cargo test"}),
                    output: Some(OUTPUT.into()),
                    is_error: Some(false),
                    lines: Some(2),
                })),
            }],
            touched: Vec::new(),
            edits: Vec::new(),
        }
    }

    fn insert_tool_session(conn: &mut Connection, id: &str, path: &str) {
        let session = tool_session(id, path);
        let tx = conn.transaction().unwrap();
        index_one(
            &tx,
            &session,
            path,
            123,
            456,
            tool_output_mode(&tx).unwrap(),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    #[test]
    fn full_mode_archives_and_rehydrates_output_without_adding_it_to_fts() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_cache_schema(&conn).unwrap();
        assert!(set_tool_output_mode(&conn, ToolOutputMode::Full).unwrap());
        assert_eq!(tool_output_mode(&conn).unwrap(), ToolOutputMode::Full);
        insert_tool_session(&mut conn, "full-session", "/gone/full-session.jsonl");

        let stored: String = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'full-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, OUTPUT);
        let fts_hits: i64 = conn
            .query_row(
                "SELECT count(*) FROM msgs WHERE msgs MATCH 'fulloutputneedle'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fts_hits, 0, "full output must not be indexed by FTS");

        archive_session(&conn, "/gone/full-session.jsonl", "full-session").unwrap();
        let transcript: String = conn
            .query_row(
                "SELECT transcript FROM archive WHERE session_id = 'full-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archived = decode_archive_transcript(&transcript).unwrap();
        assert_eq!(archived[0].full_text.as_deref(), Some(OUTPUT));

        // Rehydrate is the archive path used after a schema rebuild.
        rehydrate_archive(&conn).unwrap();
        let row = resolve(&conn, "full-session").unwrap().remove(0);
        let session = session_from_index(&conn, &row).unwrap();
        let stored_output = match session.messages[0].tool.as_ref().unwrap() {
            ToolEvent::Summary(summary) => summary.output.as_deref(),
            ToolEvent::Part(_) => panic!("indexed output must be a summary"),
        };
        assert_eq!(stored_output, Some(OUTPUT));
    }

    #[test]
    fn fresh_index_defaults_to_summary_mode() {
        let conn = Connection::open_in_memory().unwrap();
        create_cache_schema(&conn).unwrap();
        assert_eq!(tool_output_mode(&conn).unwrap(), ToolOutputMode::Summary);
    }

    #[test]
    fn summary_mode_preserves_archived_output_and_full_mode_marks_live_rows_stale() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_cache_schema(&conn).unwrap();
        assert!(set_tool_output_mode(&conn, ToolOutputMode::Full).unwrap());
        insert_tool_session(&mut conn, "archived", "/gone/archived.jsonl");
        insert_tool_session(&mut conn, "live", "/live/live.jsonl");
        archive_session(&conn, "/gone/archived.jsonl", "archived").unwrap();

        assert!(set_tool_output_mode(&conn, ToolOutputMode::Summary).unwrap());
        assert_eq!(tool_output_mode(&conn).unwrap(), ToolOutputMode::Summary);
        let live_output: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'live'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(live_output, None, "live output is cleared");
        let archived_output: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(archived_output.as_deref(), Some(OUTPUT));
        let transcript: String = conn
            .query_row(
                "SELECT transcript FROM archive WHERE session_id = 'archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(transcript.contains("fulloutputneedle"));
        let archived_messages = decode_archive_transcript(&transcript).unwrap();
        assert_eq!(archived_messages[0].full_text.as_deref(), Some(OUTPUT));
        let archived_token_before_rehydrate: (i64, i64) = conn
            .query_row(
                "SELECT mtime, size FROM files WHERE session_id = 'archived'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(archived_token_before_rehydrate, (123, 456));

        // Rehydrating after a schema rebuild preserves the archived copy in
        // messages as well, so indexed readers can still return --full output.
        rehydrate_archive(&conn).unwrap();
        let rehydrated_output: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rehydrated_output.as_deref(), Some(OUTPUT));
        let archived_row = resolve(&conn, "archived").unwrap().remove(0);
        let archived_session = session_from_index(&conn, &archived_row).unwrap();
        let shown_output = match archived_session.messages[0].tool.as_ref().unwrap() {
            ToolEvent::Summary(summary) => summary.output.as_deref(),
            ToolEvent::Part(_) => panic!("indexed output must be a summary"),
        };
        assert_eq!(shown_output, Some(OUTPUT));

        // Newly indexed sessions follow the changed mode.
        insert_tool_session(&mut conn, "summary-new", "/live/summary-new.jsonl");
        let new_output: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'summary-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new_output, None);
        archive_session(&conn, "/live/summary-new.jsonl", "summary-new").unwrap();
        let new_archive: String = conn
            .query_row(
                "SELECT transcript FROM archive WHERE session_id = 'summary-new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archived_messages = decode_archive_transcript(&new_archive).unwrap();
        assert_eq!(archived_messages.len(), 1);
        assert_eq!(
            archived_messages[0].text,
            "→ Bash(cargo test) ⇒ ok · 2 lines"
        );
        assert_eq!(archived_messages[0].full_text, None);
        assert!(!new_archive.contains("fulloutputneedle"));

        assert!(set_tool_output_mode(&conn, ToolOutputMode::Full).unwrap());
        let token: (i64, i64) = conn
            .query_row(
                "SELECT mtime, size FROM files WHERE session_id = 'live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(token, (-1, -1), "live row must be reparsed on next sync");
        let archived_token: (i64, i64, Option<String>) = conn
            .query_row(
                "SELECT mtime, size, archived_at FROM files WHERE session_id = 'archived'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((archived_token.0, archived_token.1), (0, 0));
        assert!(archived_token.2.is_some(), "archived row stays archived");
        let archived_output_after_full: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'archived'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            archived_output_after_full.as_deref(),
            Some(OUTPUT),
            "archived rows keep their prior contents"
        );
    }

    struct CountedStore {
        parses: Rc<Cell<usize>>,
    }

    impl Adapter for CountedStore {
        fn name(&self) -> &'static str {
            "tool-output-sync-test"
        }

        fn root(&self) -> Option<PathBuf> {
            std::env::current_dir().ok()
        }

        fn discover(&self) -> crate::adapters::Discovered {
            Vec::new().into()
        }

        fn parse(&self, _path: &std::path::Path) -> Result<Session> {
            anyhow::bail!("shared-store adapter")
        }

        fn store(&self) -> Option<crate::adapters::Store> {
            Some(crate::adapters::Store {
                keys: vec![("/store/forced-session".into(), 42)],
                files: Vec::new(),
                had_error: false,
            })
        }

        fn parse_key(&self, key: &str) -> Result<Session> {
            self.parses.set(self.parses.get() + 1);
            Ok(tool_session("forced-session", key))
        }
    }

    #[test]
    fn enabling_full_reparses_stale_sessions_during_a_bounded_sync() {
        let mut conn = Connection::open_in_memory().unwrap();
        create_cache_schema(&conn).unwrap();
        let parses = Rc::new(Cell::new(0));
        let adapters: Vec<Box<dyn Adapter>> = vec![Box::new(CountedStore {
            parses: Rc::clone(&parses),
        })];
        sync_with(&mut conn, &adapters, None).unwrap();
        assert_eq!(parses.get(), 1);

        set_tool_output_mode(&conn, ToolOutputMode::Summary).unwrap();
        set_tool_output_mode(&conn, ToolOutputMode::Full).unwrap();
        sync_with(&mut conn, &adapters, Some(i64::MAX)).unwrap();

        assert_eq!(parses.get(), 2, "forced sync re-parses despite its cutoff");
        let full_text: Option<String> = conn
            .query_row(
                "SELECT full_text FROM messages WHERE session_id = 'forced-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(full_text.as_deref(), Some(OUTPUT));
    }
}

#[cfg(test)]
mod separator_tests {
    use super::*;

    /// What each path lookup finds for `q`: sessions_for_file, edits_for,
    /// edits_for_session, sessions_touching, touched_paths_matching.
    fn found(c: &Connection, sid: &str, q: &str) -> [bool; 5] {
        [
            sessions_for_file(c, q, 10)
                .unwrap()
                .iter()
                .any(|(s, _)| s.session_id == sid),
            edits_for(c, q, 10)
                .unwrap()
                .iter()
                .any(|e| e.session_id == sid),
            !edits_for_session(c, sid, q, 10).unwrap().is_empty(),
            sessions_touching(c, q)
                .unwrap()
                .iter()
                .any(|s| s.session_id == sid),
            !touched_paths_matching(c, sid, q).unwrap().is_empty(),
        ]
    }

    /// A Windows tool records `D:\p\src\a.rs` or `D:/p/src/b.rs`, and Codex a
    /// path relative to its folder. `\` cannot be part of a name on Windows,
    /// so there a path typed with either separator finds them; elsewhere `\`
    /// is an ordinary character and only the stored spelling matches it.
    #[test]
    fn a_windows_path_is_found_by_either_separator() {
        let c = Connection::open_in_memory().unwrap();
        create_cache_schema(&c).unwrap();
        for (sid, project, path) in [
            ("s1", r"D:\p", r"D:\p\src\a.rs"),
            ("s1", r"D:\p", "D:/p/src/b.rs"),
            ("s2", r"D:\q", "src/c.rs"),
        ] {
            c.execute(
                "INSERT OR IGNORE INTO files(path, mtime, size, session_id, tool, project, title,
                                             started, ended, msg_count, kind)
                 VALUES (?1, 0, 0, ?1, 'claude-code', ?2, 't', '2026-10-01T10:00:00Z',
                         '2026-10-01T11:00:00Z', 1, 'main')",
                [sid, project],
            )
            .unwrap();
            c.execute(
                "INSERT INTO touched(session_id, path) VALUES (?1, ?2)",
                [sid, path],
            )
            .unwrap();
            c.execute(
                "INSERT INTO edits(session_id, path, kind, ts, snippet)
                 VALUES (?1, ?2, 'edit', '2026-10-01T10:30:00Z', 'x')",
                [sid, path],
            )
            .unwrap();
        }
        let windows = cfg!(windows);
        let cases: &[(&str, &str, bool)] = &[
            ("s1", r"D:\p\src\a.rs", true),
            ("s1", "D:/p/src/a.rs", windows),
            ("s1", r"src\a.rs", windows),
            ("s1", "src/a.rs", windows),
            ("s1", r".\src\a.rs", windows),
            ("s1", "a.rs", windows),
            ("s1", r"p\src\a.rs", windows),
            ("s1", r"D:\p\src\b.rs", windows),
            ("s1", r"src\b.rs", windows),
            ("s1", "src/b.rs", true),
            ("s1", "b.rs", true),
            // not a whole name, another folder, another drive
            ("s1", r"rc\a.rs", false),
            ("s1", r"x\a.rs", false),
            ("s1", r"E:\p\src\a.rs", false),
        ];
        for &(sid, q, want) in cases {
            assert_eq!(found(&c, sid, q), [want; 5], "{q}");
        }
        // A relative stored path is read against its session's folder when the
        // query is a full path (edits_for and sessions_touching match by name).
        let c2 = [true, true, true, true, true];
        assert_eq!(found(&c, "s2", r"src\c.rs"), [windows; 5]);
        if windows {
            assert_eq!(found(&c, "s2", r"D:\q\src\c.rs"), c2);
            assert_eq!(
                found(&c, "s2", r"D:\other\src\c.rs"),
                [false, true, false, true, true]
            );
        }
    }
}
