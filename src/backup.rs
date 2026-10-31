//! Backup and sync wikiagents data to a private GitHub repository.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Create a Command for `git` that never opens a console window on Windows.
pub fn new_git_cmd() -> Command {
    let mut cmd = Command::new("git");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

pub const MAX_CHUNK_BYTES: u64 = 45 * 1024 * 1024; // 45 MB chunks (safely under GitHub's 100 MB hard limit)
pub const REPO_URL_KEY: &str = "github_backup_repo";
pub const LAST_BACKUP_KEY: &str = "github_backup_last_at";
pub const LAST_COMMIT_KEY: &str = "github_backup_last_commit";
pub const LAST_STATUS_KEY: &str = "github_backup_last_status";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupConfig {
    pub repo_url: String,
    pub last_backup_at: Option<i64>,
    pub last_commit: Option<String>,
    pub last_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupReport {
    pub success: bool,
    pub commit_hash: Option<String>,
    pub total_sessions: i64,
    pub total_files_backed_up: usize,
    pub message: String,
    pub timestamp: i64,
}

/// What one restore actually did. `skipped` matters as much as `restored`: a
/// session already in `files` is newer locally, and quietly re-adding its
/// messages would duplicate every row on the second run.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct RestoreReport {
    pub restored: usize,
    pub skipped: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupProgress {
    pub is_running: bool,
    pub step: u8,
    pub total_steps: u8,
    pub stage: String,
    pub log_messages: Vec<String>,
    pub error: Option<String>,
    pub last_report: Option<BackupReport>,
}

impl BackupProgress {
    pub const fn new() -> Self {
        Self {
            is_running: false,
            step: 0,
            total_steps: 5,
            stage: String::new(),
            log_messages: Vec::new(),
            error: None,
            last_report: None,
        }
    }
}

impl Default for BackupProgress {
    fn default() -> Self {
        Self::new()
    }
}

static BACKUP_PROGRESS: std::sync::Mutex<BackupProgress> =
    std::sync::Mutex::new(BackupProgress::new());

pub fn get_progress() -> BackupProgress {
    BACKUP_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub fn is_running() -> bool {
    BACKUP_PROGRESS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_running
}

/// Backup and restore share one staging directory, so only one of them may run
/// at a time. Checked here, before a worker thread exists: the progress flag is
/// set inside the thread, which left a window where two clicks both passed the
/// `is_running()` test.
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Claim the single slot. A `false` answer means a run is already in progress
/// and the caller must start nothing.
pub fn try_begin() -> bool {
    RUNNING
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
}

/// Releases the slot claimed by [`try_begin`] when dropped, so a panic or an
/// early `?` inside a run cannot wedge backups forever.
#[derive(Debug)]
pub struct RunningGuard;

impl Drop for RunningGuard {
    fn drop(&mut self) {
        RUNNING.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// `https://user:token@host/repo.git` puts the credential in the URL itself, and
/// that URL is echoed into manifest.json and into git's own error text. Keep the
/// address, drop the secret. The stored config keeps the full URL: push needs it.
pub fn strip_url_credentials(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let after = &url[scheme_end + 3..];
    // Credentials only ever appear in the authority section, which ends at the
    // first `/`, `?` or `#`.
    let authority_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let Some(at) = after[..authority_end].rfind('@') else {
        return url.to_string();
    };
    format!("{}{}", &url[..scheme_end + 3], &after[at + 1..])
}

pub fn set_progress<F>(f: F)
where
    F: FnOnce(&mut BackupProgress),
{
    let mut p = BACKUP_PROGRESS.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut p);
}

pub fn log_step(step: u8, stage: &str, message: &str) {
    set_progress(|p| {
        p.step = step;
        p.stage = stage.to_string();
        p.log_messages.push(message.to_string());
    });
}

pub fn set_error(err: &str) {
    set_progress(|p| {
        p.is_running = false;
        p.error = Some(err.to_string());
        p.log_messages.push(format!("ОШИБКА: {err}"));
    });
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionTestResult {
    pub ok: bool,
    pub message: String,
    pub git_version: String,
    pub git_user: Option<String>,
}

/// Retrieve backup configuration from the index meta table.
pub fn get_backup_config(conn: &Connection) -> Result<BackupConfig> {
    let repo_url = get_meta(conn, REPO_URL_KEY)?.unwrap_or_default();
    let last_backup_at = get_meta(conn, LAST_BACKUP_KEY)?.and_then(|s| s.parse::<i64>().ok());
    let last_commit = get_meta(conn, LAST_COMMIT_KEY)?;
    let last_status = get_meta(conn, LAST_STATUS_KEY)?;

    Ok(BackupConfig {
        repo_url,
        last_backup_at,
        last_commit,
        last_status,
    })
}

/// Save GitHub repository URL into the index meta table.
pub fn set_backup_config(conn: &Connection, repo_url: &str) -> Result<()> {
    set_meta(conn, REPO_URL_KEY, repo_url.trim())
}

/// Test connection to the remote repository using `git ls-remote`.
pub fn test_connection(repo_url: &str) -> ConnectionTestResult {
    let git_version = new_git_cmd()
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "git not found".to_string());

    let git_user = new_git_cmd()
        .args(["config", "--get", "user.name"])
        .output()
        .ok()
        .and_then(|o| {
            let u = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if u.is_empty() {
                None
            } else {
                Some(u)
            }
        });

    if repo_url.trim().is_empty() {
        return ConnectionTestResult {
            ok: false,
            message: "URL репозитория не указан".to_string(),
            git_version,
            git_user,
        };
    }

    match new_git_cmd().args(["ls-remote", repo_url.trim()]).output() {
        Ok(output) if output.status.success() => ConnectionTestResult {
            ok: true,
            message: "Подключение успешно! Доступ к репозиторию подтвержден \
                      (убедитесь, что репозиторий приватный)"
                .to_string(),
            git_version,
            git_user,
        },
        Ok(output) => {
            let raw_err = String::from_utf8_lossy(&output.stderr);
            let err = strip_url_credentials(raw_err.trim());
            let msg = if err.is_empty() {
                format!("Ошибка проверки доступа (код {})", output.status)
            } else {
                err
            };
            ConnectionTestResult {
                ok: false,
                message: msg,
                git_version,
                git_user,
            }
        }
        Err(e) => ConnectionTestResult {
            ok: false,
            message: format!("Не удалось вызвать git: {e}"),
            git_version,
            git_user,
        },
    }
}

/// Resolve the local staging directory where the backup Git repository lives.
pub fn resolve_backup_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("WIKIAGENTS_BACKUP_DIR") {
        let p = PathBuf::from(dir);
        fs::create_dir_all(&p)?;
        return Ok(p);
    }

    let base = match crate::index::db_path() {
        Ok(p) => p.parent().unwrap_or(Path::new(".")).to_path_buf(),
        Err(_) => PathBuf::from("data"),
    };
    let backup_dir = base.join("backup_repo");
    fs::create_dir_all(&backup_dir)?;
    Ok(backup_dir)
}

/// Execute full backup to GitHub:
/// 1. Exports curated user data (favorites, tags, notes, snippets, summaries).
/// 2. Exports raw web session JSONs (chunking files > 45 MB).
/// 3. Exports full SQLite database (vacuum -> gzip -> 45 MB chunks).
/// 4. Generates manifest.json and README.md.
/// 5. Commits and pushes to GitHub.
pub fn run_backup(conn: &Connection, repo_url: &str) -> Result<BackupReport> {
    let repo_url = repo_url.trim();
    if repo_url.is_empty() {
        let err = "GitHub repository URL is empty".to_string();
        set_error(&err);
        anyhow::bail!(err);
    }

    set_progress(|p| {
        p.is_running = true;
        p.step = 1;
        p.total_steps = 5;
        p.stage = "Проверка Git-репозитория...".to_string();
        p.error = None;
        p.last_report = None;
        p.log_messages = vec!["[1/5] Проверка и подготовка Git-репозитория...".to_string()];
    });

    let res = (|| -> Result<BackupReport> {
        let backup_dir = resolve_backup_dir()?;
        let now = Utc::now();
        let timestamp = now.timestamp();

        // 1. Initialize or configure git repo in backup_dir
        ensure_git_repo(&backup_dir, repo_url)?;
        log_step(
            1,
            "Git-репозиторий настроен",
            "[1/5] Проверка Git-репозитория... OK",
        );

        // 2. Export curated tables
        log_step(
            2,
            "Экспорт метаданных и заметок...",
            "[2/5] Экспорт избранного, заметок, тегов и сниппетов...",
        );
        let curated_dir = backup_dir.join("curated");
        fs::create_dir_all(&curated_dir)?;
        let curated_count = export_curated_tables(conn, &curated_dir)?;
        log_step(
            2,
            "Метаданные экспортированы",
            &format!(
                "[2/5] Экспорт метаданных и сниппетов ({} записей)... OK",
                curated_count
            ),
        );

        // 3. Export web session files (with 45MB chunking)
        log_step(
            3,
            "Экспорт веб-чатов...",
            "[3/5] Экспорт веб-чатов (Claude, DeepSeek, Gemini, Qwen, GLM)...",
        );
        let web_dir = backup_dir.join("web_session");
        fs::create_dir_all(&web_dir)?;
        let web_count = export_web_session_files(&web_dir)?;
        log_step(
            3,
            "Веб-чаты экспортированы",
            &format!("[3/5] Экспорт веб-чатов ({} файлов)... OK", web_count),
        );

        // 4. Export granular sessions (sessions/<tool>/<session_id>.json)
        log_step(
            4,
            "Инкрементальный экспорт сессий...",
            "[4/5] Проверка сессий IDE-агентов (инкрементально)...",
        );
        let (total_s, newly_written) = export_granular_sessions(conn, &backup_dir)?;
        log_step(
            4,
            "Сессии сохранены",
            &format!(
                "[4/5] Экспорт сессий... OK (всего: {}, новых/обновленных: {})",
                total_s, newly_written
            ),
        );

        // 5. Generate manifest, README and restore scripts
        let (total_sessions, tool_stats) = query_session_stats(conn)?;
        write_manifest_and_readme(&backup_dir, repo_url, total_sessions, &tool_stats)?;
        write_restore_scripts(&backup_dir)?;

        // 6. Git commit and push
        log_step(
            5,
            "Отправка на GitHub (git push)...",
            "[5/5] Подготовка коммита и отправка в GitHub (git push)...",
        );
        run_git_cmd(&backup_dir, &["add", "-A"])?;

        let status = run_git_cmd(&backup_dir, &["status", "--porcelain"])?;
        let commit_hash;

        let message = if status.is_empty() {
            commit_hash = run_git_cmd(&backup_dir, &["rev-parse", "--short", "HEAD"]).ok();
            let _ = run_git_cmd(&backup_dir, &["push", "-u", "origin", "main"]);
            "Данные уже синхронизированы с GitHub, изменений нет.".to_string()
        } else {
            let commit_msg = format!(
                "wikiagents backup: {} ({} new/updated, {} total across {} tools)",
                now.format("%Y-%m-%d %H:%M:%S UTC"),
                newly_written,
                total_sessions,
                tool_stats.len()
            );
            run_git_cmd(&backup_dir, &["commit", "-m", &commit_msg])?;
            commit_hash = Some(run_git_cmd(&backup_dir, &["rev-parse", "--short", "HEAD"])?);

            // Push to origin main, integrating anything the other machine
            // already sent rather than overwriting it.
            push_integrating(&backup_dir)?;
            format!(
                "Успешно выгружено в GitHub! Коммит: {}, выгружено сессий: {} (всего: {})",
                commit_hash.as_deref().unwrap_or(""),
                newly_written,
                total_sessions
            )
        };

        // 7. Save metadata in index
        set_meta(conn, REPO_URL_KEY, repo_url)?;
        set_meta(conn, LAST_BACKUP_KEY, &timestamp.to_string())?;
        if let Some(ref h) = commit_hash {
            set_meta(conn, LAST_COMMIT_KEY, h)?;
        }
        set_meta(conn, LAST_STATUS_KEY, "ok")?;

        let total_files = count_files_in_dir(&backup_dir)?;
        let hash_str = commit_hash.as_deref().unwrap_or("");
        log_step(
            5,
            "Готово",
            &format!("[5/5] УСПЕШНО! Выгружено в GitHub! Коммит: {}", hash_str),
        );

        Ok(BackupReport {
            success: true,
            commit_hash,
            total_sessions,
            total_files_backed_up: total_files,
            message,
            timestamp,
        })
    })();

    match res {
        Ok(report) => {
            set_progress(|p| {
                p.is_running = false;
                p.stage = "Завершено успешно".to_string();
                p.last_report = Some(report.clone());
            });
            Ok(report)
        }
        Err(e) => {
            set_error(&e.to_string());
            Err(e)
        }
    }
}

/// Restore full database from GitHub backup repository:
/// 1. Pulls latest commits from GitHub repository.
/// 2. Restores curated tables (favorites, tags, notes, summaries, snippets).
/// 3. Restores granular sessions into SQLite index tables.
pub fn run_restore(conn: &mut Connection, repo_url: &str) -> Result<RestoreReport> {
    let repo_url = repo_url.trim();
    if repo_url.is_empty() {
        let err = "GitHub repository URL is empty".to_string();
        set_error(&err);
        anyhow::bail!(err);
    }

    set_progress(|p| {
        p.is_running = true;
        p.step = 1;
        p.total_steps = 4;
        p.stage = "Синхронизация с GitHub...".to_string();
        p.error = None;
        p.log_messages = vec!["[1/4] Подключение к GitHub и git pull...".to_string()];
    });

    let res = (|| -> Result<RestoreReport> {
        let backup_dir = resolve_backup_dir()?;
        ensure_git_repo(&backup_dir, repo_url)?;

        // 1. Fetch and pull latest from origin main. Failures here are fatal:
        // restoring from a stale local clone would silently hand back old data
        // and report success.
        run_git_cmd(&backup_dir, &["fetch", "origin", "main"])
            .context("не удалось получить данные из GitHub: проверьте URL и доступ")?;
        run_git_cmd(&backup_dir, &["checkout", "-B", "main", "origin/main"])
            .context("в репозитории нет ветки main с данными бэкапа")?;
        log_step(
            1,
            "Репозиторий обновлен",
            "[1/4] Подключение к GitHub и git pull... OK",
        );

        // 2. Restore curated
        log_step(
            2,
            "Восстановление метаданных...",
            "[2/4] Восстановление заметок, тегов, избранного и сниппетов...",
        );
        restore_curated_tables(conn, &backup_dir.join("curated"))?;
        log_step(
            2,
            "Метаданные восстановлены",
            "[2/4] Восстановление метаданных и заметок... OK",
        );

        // 3. Restore granular sessions
        log_step(
            3,
            "Восстановление сессий...",
            "[3/4] Восстановление сессий агентов из sessions/...",
        );
        let report = restore_granular_sessions(conn, &backup_dir.join("sessions"))?;
        log_step(
            3,
            "Сессии восстановлены",
            &format!(
                "[3/4] Восстановление сессий... OK (новых: {}, пропущено: {}, ошибок: {})",
                report.restored, report.skipped, report.failed
            ),
        );

        // 4. Update local meta
        set_meta(conn, REPO_URL_KEY, repo_url)?;
        set_meta(conn, LAST_STATUS_KEY, "ok")?;
        log_step(
            4,
            "Готово",
            &format!(
                "[4/4] УСПЕШНО! Восстановлено {} сессий из GitHub бэкапа ({} уже были, {} файлов пропущено).",
                report.restored, report.skipped, report.failed
            ),
        );

        Ok(report)
    })();

    match res {
        Ok(report) => {
            set_progress(|p| {
                p.is_running = false;
                p.stage = "Восстановление завершено".to_string();
            });
            Ok(report)
        }
        Err(e) => {
            set_error(&e.to_string());
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers: Git execution
// ---------------------------------------------------------------------------

fn ensure_git_repo(dir: &Path, repo_url: &str) -> Result<()> {
    let git_dir = dir.join(".git");
    if !git_dir.exists() {
        run_git_cmd(dir, &["init"])?;
        run_git_cmd(dir, &["branch", "-M", "main"])?;
        run_git_cmd(dir, &["remote", "add", "origin", repo_url])?;
    } else {
        // Ensure remote URL is up-to-date
        let _ = run_git_cmd(dir, &["remote", "set-url", "origin", repo_url]);
    }

    // Configure sensible git repo settings for backup
    let _ = run_git_cmd(dir, &["config", "core.autocrlf", "false"]);
    let _ = run_git_cmd(dir, &["config", "http.postBuffer", "524288000"]);
    Ok(())
}

fn run_git_cmd(dir: &Path, args: &[&str]) -> Result<String> {
    let output = new_git_cmd()
        .current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))?;

    if !output.status.success() {
        // git echoes the remote URL in its failures, and that URL can carry the
        // token: never let it reach a log line or the UI.
        let raw_stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = crate::redact::redact(&raw_stderr);
        anyhow::bail!(
            "git {} failed: {}",
            args.join(" "),
            strip_url_credentials(stderr.trim())
        );
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Push `main` without ever rewriting remote history. A rejected push means
/// another machine got there first, so its commits are pulled in under ours and
/// the push retried; a real conflict is reported, not resolved silently.
fn push_integrating(dir: &Path) -> Result<()> {
    if run_git_cmd(dir, &["push", "-u", "origin", "main"]).is_ok() {
        return Ok(());
    }
    let mut last = String::from("git push origin main failed");
    for attempt in 1..=3 {
        std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
        run_git_cmd(dir, &["fetch", "origin", "main"])?;
        if let Err(e) = run_git_cmd(dir, &["rebase", "origin/main"]) {
            // Leave the staging tree where the user can see and fix it.
            let _ = run_git_cmd(dir, &["rebase", "--abort"]);
            return Err(e.context(
                "удалённый репозиторий содержит изменения, которые не удаётся \
                 совместить автоматически; выполните restore или разрешите \
                 конфликт вручную",
            ));
        }
        match run_git_cmd(dir, &["push", "-u", "origin", "main"]) {
            Ok(_) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
    }
    anyhow::bail!(
        "удалённый репозиторий содержит изменения с другой машины; \
         выполните restore или разрешите вручную ({last})"
    )
}

// ---------------------------------------------------------------------------
// Helpers: Curated Data Export
// ---------------------------------------------------------------------------

pub fn export_curated_tables(conn: &Connection, dest_dir: &Path) -> Result<usize> {
    // 1. Favorites
    let mut fav_stmt =
        conn.prepare("SELECT session_id, created_at FROM favorites ORDER BY created_at DESC")?;
    let fav_rows: Vec<Value> = fav_stmt
        .query_map([], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "created_at": r.get::<_, Option<String>>(1)?,
            }))
        })?
        // A row that fails to read is a row the backup would lose without
        // saying so, so these exports propagate instead of filtering.
        .collect::<Result<Vec<Value>, rusqlite::Error>>()?;
    fs::write(
        dest_dir.join("favorites.json"),
        serde_json::to_string_pretty(&fav_rows)?,
    )?;

    // 2. Tags
    let mut tag_stmt = conn.prepare("SELECT session_id, tag FROM tags ORDER BY session_id, tag")?;
    let tag_rows: Vec<Value> = tag_stmt
        .query_map([], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "tag": r.get::<_, String>(1)?,
            }))
        })?
        .collect::<Result<Vec<Value>, rusqlite::Error>>()?;
    fs::write(
        dest_dir.join("tags.json"),
        serde_json::to_string_pretty(&tag_rows)?,
    )?;

    // 3. Notes
    let mut notes_stmt =
        conn.prepare("SELECT session_id, note, updated FROM notes ORDER BY updated DESC")?;
    let note_rows: Vec<Value> = notes_stmt
        .query_map([], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "note": r.get::<_, String>(1)?,
                "updated": r.get::<_, Option<String>>(2)?,
            }))
        })?
        .collect::<Result<Vec<Value>, rusqlite::Error>>()?;
    fs::write(
        dest_dir.join("notes.json"),
        serde_json::to_string_pretty(&note_rows)?,
    )?;

    // 4. Snippets (KB solutions)
    let mut snip_stmt = conn.prepare(
        "SELECT id, session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts FROM snippets ORDER BY created_at DESC",
    )?;
    let snip_rows: Vec<Value> = snip_stmt
        .query_map([], |r| {
            // `id` is an INTEGER and `lang`/`tags`/`message_ts` are nullable, so
            // reading them as String/i64 threw on every real row and the caller
            // dropped the whole snippet - the export silently contained no
            // knowledge base at all.
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "session_id": r.get::<_, String>(1)?,
                "message_idx": r.get::<_, i64>(2)?,
                "title": r.get::<_, String>(3)?,
                "lang": r.get::<_, Option<String>>(4)?,
                "content": r.get::<_, String>(5)?,
                "tags": r.get::<_, Option<String>>(6)?,
                "created_at": r.get::<_, String>(7)?,
                "tool": r.get::<_, Option<String>>(8)?,
                "role": r.get::<_, Option<String>>(9)?,
                "message_ts": r.get::<_, Option<String>>(10)?,
            }))
        })?
        .collect::<Result<Vec<Value>, rusqlite::Error>>()?;
    fs::write(
        dest_dir.join("snippets.json"),
        serde_json::to_string_pretty(&snip_rows)?,
    )?;

    // 5. Summaries
    let mut sum_stmt = conn
        .prepare("SELECT session_id, summary, created, kind FROM summaries ORDER BY session_id")?;
    let sum_rows: Vec<Value> = sum_stmt
        .query_map([], |r| {
            Ok(json!({
                "session_id": r.get::<_, String>(0)?,
                "summary": r.get::<_, String>(1)?,
                "created": r.get::<_, Option<String>>(2)?,
                "kind": r.get::<_, Option<String>>(3)?,
            }))
        })?
        .collect::<Result<Vec<Value>, rusqlite::Error>>()?;
    fs::write(
        dest_dir.join("summaries.json"),
        serde_json::to_string_pretty(&sum_rows)?,
    )?;

    let total =
        fav_rows.len() + tag_rows.len() + note_rows.len() + snip_rows.len() + sum_rows.len();
    Ok(total)
}

// ---------------------------------------------------------------------------
// Helpers: Web Sessions Export with 45 MB Chunking
// ---------------------------------------------------------------------------

fn export_web_session_files(dest_dir: &Path) -> Result<usize> {
    let roots = crate::adapters::web_chat::web_session_roots();
    let mut copied_files = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        let entries = match fs::read_dir(&root) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            if ext != "json" {
                continue;
            }

            let file_name = path.file_name().unwrap().to_string_lossy().to_string();
            export_one_web_session(&path, dest_dir, &file_name)?;
            copied_files.push(file_name);
        }
    }

    // Write recombine helper scripts
    write_recombine_helpers(dest_dir)?;

    Ok(copied_files.len())
}

/// Copy one web-chat transcript into the backup with secrets removed. These
/// files leave the machine, and a pasted API key or cookie stays in them. The
/// markers `[redacted:kind]` hold no quotes, so the JSON stays parseable.
fn export_one_web_session(path: &Path, dest_dir: &Path, file_name: &str) -> Result<()> {
    let raw = fs::read(path)?;
    let text = String::from_utf8_lossy(&raw);
    let redacted = crate::redact::redact(&text);
    let bytes = redacted.as_bytes();
    if bytes.len() as u64 > MAX_CHUNK_BYTES {
        // Redact first, then split: a chunk boundary must never fall inside a
        // secret, and a reassembled file must never contain the original.
        split_bytes(bytes, dest_dir, file_name)?;
    } else {
        remove_derived_names(dest_dir, file_name)?;
        fs::write(dest_dir.join(file_name), bytes)?;
    }
    Ok(())
}

/// Split a too-big export into parts GitHub accepts. Anything the previous run
/// left under this name - whole file or parts with a different count - goes
/// first, or the restore would recombine a stale tail onto the new head.
fn split_bytes(data: &[u8], dest_dir: &Path, base_name: &str) -> Result<usize> {
    remove_derived_names(dest_dir, base_name)?;
    let mut chunk_idx = 0;
    for chunk in data.chunks(MAX_CHUNK_BYTES as usize) {
        let chunk_name = format!("{}.part{:03}", base_name, chunk_idx);
        fs::write(dest_dir.join(&chunk_name), chunk)?;
        chunk_idx += 1;
    }
    Ok(chunk_idx)
}

/// Remove `<name>` and every `<name>.partNNN` in `dest_dir`.
fn remove_derived_names(dest_dir: &Path, base_name: &str) -> Result<()> {
    let _ = fs::remove_file(dest_dir.join(base_name));
    let prefix = format!("{base_name}.part");
    let entries = match fs::read_dir(dest_dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn write_recombine_helpers(dest_dir: &Path) -> Result<()> {
    // Python script to recombine any .partNNN files in the directory
    let py_script = r#"#!/usr/bin/env python3
"""Recombines split .partNNN files into their original full-size files."""
import os, glob, re

def recombine():
    parts = glob.glob("*.part000")
    for first_part in parts:
        base_name = first_part[:-8]
        part_files = sorted(glob.glob(f"{base_name}.part*"))
        print(f"Recombining {len(part_files)} parts into {base_name}...")
        with open(base_name, "wb") as out:
            for p in part_files:
                with open(p, "rb") as inp:
                    out.write(inp.read())
        print(f"  ✓ {base_name} restored ({os.path.getsize(base_name)} bytes).")

if __name__ == "__main__":
    recombine()
"#;
    fs::write(dest_dir.join("recombine.py"), py_script)?;

    // PowerShell script to do the same on Windows
    let ps_script = r#"# Recombines split .partNNN files into their original full-size files
Get-ChildItem -Filter "*.part000" | ForEach-Object {
    $baseName = $_.Name.Substring(0, $_.Name.Length - 8)
    $parts = Get-ChildItem -Filter "$baseName.part*" | Sort-Object Name
    Write-Host "Recombining $($parts.Count) parts into $baseName..."
    $out = [System.IO.File]::Create((Join-Path $PSScriptRoot $baseName))
    foreach ($p in $parts) {
        $bytes = [System.IO.File]::ReadAllBytes($p.FullName)
        $out.Write($bytes, 0, $bytes.Length)
    }
    $out.Close()
    Write-Host "  ✓ $baseName restored."
}
"#;
    fs::write(dest_dir.join("recombine.ps1"), ps_script)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers: Granular Sessions Export & Zero-Dependency Restore Scripts
// ---------------------------------------------------------------------------

fn safe_file_component(s: &str) -> String {
    let safe: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "unnamed".to_string()
    } else {
        safe
    }
}

pub fn export_granular_sessions(conn: &Connection, dest_dir: &Path) -> Result<(usize, usize)> {
    // 1. Remove obsolete monolithic database directory if present
    let old_db = dest_dir.join("database");
    if old_db.exists() {
        let _ = fs::remove_dir_all(&old_db);
    }

    let sessions_dir = dest_dir.join("sessions");
    fs::create_dir_all(&sessions_dir)?;

    let mut stmt = conn.prepare(
        "SELECT session_id, tool, project, title, started, ended, kind, parent_id, mtime FROM files ORDER BY session_id"
    )?;

    struct FileRow {
        session_id: String,
        tool: String,
        project: String,
        title: String,
        started: Option<String>,
        ended: Option<String>,
        kind: String,
        parent_id: Option<String>,
        mtime: i64,
    }

    let rows: Vec<FileRow> = stmt
        .query_map([], |r| {
            Ok(FileRow {
                session_id: r.get(0)?,
                tool: r.get(1)?,
                project: r.get(2)?,
                title: r.get(3)?,
                started: r.get(4)?,
                ended: r.get(5)?,
                kind: r.get(6)?,
                parent_id: r.get(7)?,
                mtime: r.get(8)?,
            })
        })?
        // One unreadable row must not abort the whole export, but it must not
        // vanish without a trace either.
        .filter_map(|r| match r {
            Ok(row) => Some(row),
            Err(e) => {
                eprintln!("backup: skipping an unreadable files row ({e})");
                None
            }
        })
        .collect();
    drop(stmt);

    let total = rows.len();
    let mut newly_written = 0;
    // Every path this run accounts for, written or skipped as already current.
    let mut keep: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let mut msg_stmt = conn.prepare(
        "SELECT role, text, full_text, is_rewound FROM messages WHERE session_id = ?1 ORDER BY id",
    )?;
    let mut touched_stmt = conn.prepare("SELECT path FROM touched WHERE session_id = ?1")?;
    let mut commits_stmt =
        conn.prepare("SELECT commit_sha FROM session_commits WHERE session_id = ?1")?;

    for row in &rows {
        let tool_safe = safe_file_component(&row.tool);
        let id_safe = safe_file_component(&row.session_id);
        let tool_dir = sessions_dir.join(&tool_safe);
        fs::create_dir_all(&tool_dir)?;

        let dest_file = tool_dir.join(format!("{id_safe}.json"));
        keep.insert(dest_file.clone());

        let row_mtime_sec = if row.mtime > 100_000_000_000 {
            row.mtime / 1000
        } else {
            row.mtime
        };

        let should_skip = if let Ok(meta) = dest_file.metadata() {
            if meta.len() > 0 {
                if let Ok(mod_time) = meta.modified() {
                    let file_sec = mod_time
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    file_sec >= row_mtime_sec
                } else {
                    false
                }
            } else {
                false
            }
        } else {
            false
        };

        if should_skip {
            continue;
        }

        // Fetch messages for this session
        let messages: Vec<Value> = msg_stmt
            .query_map([&row.session_id], |r| {
                let role: String = r.get(0)?;
                let text: String = r.get(1)?;
                let full_text: Option<String> = r.get(2)?;
                let is_rewound: bool = r.get::<_, i32>(3).unwrap_or(0) != 0;
                Ok(json!({
                    "role": role,
                    "text": text,
                    "full_text": full_text,
                    "is_rewound": is_rewound,
                }))
            })?
            .collect::<Result<Vec<Value>, rusqlite::Error>>()?;

        // Fetch touched files
        let touched: Vec<String> = touched_stmt
            .query_map([&row.session_id], |r| r.get(0))?
            .collect::<Result<Vec<String>, rusqlite::Error>>()?;

        // Fetch commits
        let commits: Vec<String> = commits_stmt
            .query_map([&row.session_id], |r| r.get(0))?
            .collect::<Result<Vec<String>, rusqlite::Error>>()?;

        let session_data = json!({
            "id": row.session_id,
            "tool": row.tool,
            "project": row.project,
            "title": row.title,
            "started": row.started,
            "ended": row.ended,
            "kind": row.kind,
            "parent_id": row.parent_id,
            "mtime": row.mtime,
            "touched_files": touched,
            "commit_shas": commits,
            "messages": messages,
        });

        fs::write(&dest_file, serde_json::to_string_pretty(&session_data)?)?;
        newly_written += 1;
    }

    // A session forgotten locally must not outlive that in the backup: the next
    // restore would hand it straight back.
    let entries = fs::read_dir(&sessions_dir)?;
    for tool_dir in entries.flatten() {
        let tool_path = tool_dir.path();
        if !tool_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&tool_path) else {
            continue;
        };
        for file in files.flatten() {
            let p = file.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") && !keep.contains(&p) {
                let _ = fs::remove_file(&p);
            }
        }
    }

    Ok((total, newly_written))
}

fn write_restore_scripts(dest_dir: &Path) -> Result<()> {
    let py_restore = r#"#!/usr/bin/env python3
"""
WikiAgents Backup Restorer.
Restores all sessions, curated data, and web chats from this repository into a SQLite index.db.
Requires Python 3 (the standard-library sqlite3 is enough - no pip install).
Usage:
    python restore.py [dest_index.db]
"""
import os, sys, glob, json, sqlite3, datetime

def restore(dest_db="index.db"):
    print(f"[*] Restoring WikiAgents database to: {dest_db}")
    db_dir = os.path.dirname(os.path.abspath(dest_db))
    if db_dir:
        os.makedirs(db_dir, exist_ok=True)
    conn = sqlite3.connect(dest_db)
    cur = conn.cursor()

    cur.execute("""CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT)""")
    cur.execute("""CREATE TABLE IF NOT EXISTS files (
        path TEXT PRIMARY KEY, mtime INTEGER NOT NULL, size INTEGER NOT NULL,
        session_id TEXT NOT NULL, tool TEXT NOT NULL, project TEXT NOT NULL DEFAULT '',
        title TEXT NOT NULL DEFAULT '', started TEXT, ended TEXT, msg_count INTEGER NOT NULL DEFAULT 0,
        kind TEXT NOT NULL DEFAULT 'main', archived_at TEXT, parent_id TEXT, reader INTEGER NOT NULL DEFAULT 0
    )""")
    cur.execute("""CREATE TABLE IF NOT EXISTS messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL,
        text TEXT NOT NULL, full_text TEXT, is_rewound INTEGER NOT NULL DEFAULT 0
    )""")
    cur.execute("""CREATE TABLE IF NOT EXISTS touched (session_id TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(session_id, path))""")
    cur.execute("""CREATE TABLE IF NOT EXISTS session_commits (session_id TEXT NOT NULL, commit_sha TEXT NOT NULL, message TEXT, timestamp INTEGER, PRIMARY KEY(session_id, commit_sha))""")
    cur.execute("""CREATE TABLE IF NOT EXISTS summaries (session_id TEXT PRIMARY KEY, summary TEXT NOT NULL, created TEXT, kind TEXT)""")
    cur.execute("""CREATE TABLE IF NOT EXISTS tags (session_id TEXT NOT NULL, tag TEXT NOT NULL, PRIMARY KEY(session_id, tag))""")
    cur.execute("""CREATE TABLE IF NOT EXISTS notes (session_id TEXT PRIMARY KEY, note TEXT NOT NULL, updated TEXT)""")
    cur.execute("""CREATE TABLE IF NOT EXISTS favorites (session_id TEXT PRIMARY KEY, created_at TEXT)""")
    cur.execute("""CREATE TABLE IF NOT EXISTS snippets (
        id TEXT PRIMARY KEY, session_id TEXT NOT NULL, message_idx INTEGER NOT NULL,
        title TEXT NOT NULL, lang TEXT NOT NULL, content TEXT NOT NULL, tags TEXT NOT NULL,
        created_at TEXT NOT NULL, tool TEXT, role TEXT, message_ts INTEGER
    )""")

    # External-content FTS5 over `messages`: one `text` column whose rowid IS
    # messages.id, with the trigram tokenizer the app defaults to. A standalone
    # fts5 table (as this script used to create) does not match the real schema
    # and the app's `msgs(rowid, text)` insert would fail on it.
    cur.execute("""CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(
        text, content='messages', content_rowid='id', tokenize='trigram')""")

    # 1. Restore Curated
    curated_dir = "curated"
    if os.path.exists(curated_dir):
        print("[*] Restoring curated tables...")
        # Merge, never overwrite: what this machine already has stays. Same
        # rules as the app's restore, so both paths give the same result.
        for name, tbl, cols, sql in [
            ("favorites.json", "favorites", ["session_id", "created_at"],
             "INSERT OR IGNORE INTO favorites (session_id, created_at) VALUES (?, ?)"),
            ("tags.json", "tags", ["session_id", "tag"],
             "INSERT OR IGNORE INTO tags (session_id, tag) VALUES (?, ?)"),
            ("notes.json", "notes", ["session_id", "note", "updated"],
             """INSERT INTO notes (session_id, note, updated) VALUES (?, ?, ?)
                ON CONFLICT(session_id) DO UPDATE SET note=excluded.note, updated=excluded.updated
                WHERE COALESCE(excluded.updated,'') > COALESCE(notes.updated,'')"""),
            ("summaries.json", "summaries", ["session_id", "summary", "created", "kind"],
             """INSERT INTO summaries (session_id, summary, created, kind) VALUES (?, ?, ?, ?)
                ON CONFLICT(session_id) DO UPDATE SET summary=excluded.summary, created=excluded.created, kind=excluded.kind
                WHERE COALESCE(excluded.created,'') > COALESCE(summaries.created,'')"""),
            ("snippets.json", "snippets", ["session_id", "message_idx", "title", "lang", "content", "tags", "created_at", "tool", "role", "message_ts"],
             # ids are per-machine AUTOINCREMENT, so they collide; match on the content key instead.
             """INSERT INTO snippets (session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts)
                SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                WHERE NOT EXISTS (SELECT 1 FROM snippets WHERE session_id=? AND message_idx=? AND title=?)"""),
        ]:
            p = os.path.join(curated_dir, name)
            if os.path.exists(p):
                with open(p, "r", encoding="utf-8") as f:
                    rows = json.load(f)
                vals = [[r.get(c) for c in cols] for r in rows]
                if name == "snippets.json":
                    # the NOT EXISTS half repeats the key columns
                    vals = [v + v[:3] for v in vals]
                cur.executemany(sql, vals)
                print(f"  [OK] {tbl}: {len(rows)} rows merged.")

    # 2. Restore Sessions
    archived_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    session_files = sorted(glob.glob(os.path.join("sessions", "*", "*.json")))
    print(f"[*] Restoring {len(session_files)} granular sessions...")
    restored = skipped = failed = 0
    for sf in session_files:
        try:
            with open(sf, "r", encoding="utf-8") as f:
                s = json.load(f)
            sid = s["id"]
            tool = s.get("tool", "unknown")
            proj = s.get("project", "")
            title = s.get("title", "")
            started = s.get("started")
            ended = s.get("ended")
            kind = s.get("kind", "main")
            parent_id = s.get("parent_id")
            mtime = s.get("mtime", 0)
            msgs = s.get("messages", [])
            touched = s.get("touched_files", [])
            commits = s.get("commit_shas", [])

            cur.execute("SELECT 1 FROM files WHERE session_id = ? LIMIT 1", (sid,))
            if cur.fetchone():
                # The local copy is fresher; re-adding it would duplicate
                # every message of this session.
                skipped += 1
                continue

            fake_path = f"restored://{tool}/{sid}"
            # archived_at keeps the sync sweep away from restored:// rows:
            # it only removes sessions with archived_at IS NULL.
            cur.execute("""INSERT INTO files
                (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id, archived_at)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (fake_path, mtime, len(msgs), sid, tool, proj, title, started, ended, len(msgs), kind, parent_id, archived_at)
            )

            for m in msgs:
                role = m.get("role", "user")
                text = m.get("text", "")
                full_text = m.get("full_text")
                is_rewound = 1 if m.get("is_rewound") else 0
                cur.execute("""INSERT INTO messages (session_id, role, text, full_text, is_rewound)
                    VALUES (?, ?, ?, ?, ?)""", (sid, role, text, full_text, is_rewound))
                # msgs is external-content FTS: the rowid is the messages.id
                # just inserted, and the text must be byte-identical.
                cur.execute("INSERT INTO msgs(rowid, text) VALUES (?, ?)", (cur.lastrowid, text))

            for t in touched:
                cur.execute("INSERT OR IGNORE INTO touched (session_id, path) VALUES (?, ?)", (sid, t))

            for c in commits:
                cur.execute("INSERT OR IGNORE INTO session_commits (session_id, commit_sha) VALUES (?, ?)", (sid, c))

            restored += 1
        except Exception as e:
            print(f"  [SKIP] {sf}: {e}")
            failed += 1
            continue

    conn.commit()
    conn.close()
    print(f"[OK] SUCCESS: {restored} sessions restored, {skipped} already present, {failed} failed -> {dest_db}!")

if __name__ == "__main__":
    dest = sys.argv[1] if len(sys.argv) > 1 else "index.db"
    restore(dest)
"#;
    fs::write(dest_dir.join("restore.py"), py_restore)?;

    let ps_restore = r#"# WikiAgents Backup Restorer (PowerShell)
# REQUIRES PYTHON 3 - this script only locates python and runs restore.py.
# Without Python 3 nothing is restored; install it or use WikiAgents itself
# (Backup window -> "Синхронизировать / Восстановить").
param([string]$dest = "index.db")

if (Get-Command python -ErrorAction SilentlyContinue) {
    Write-Host "Restoring WikiAgents database via python restore.py $dest..."
    python restore.py $dest
} else {
    Write-Host "Python 3 not found. Install Python 3 or use WikiAgents to restore sessions."
}
"#;
    fs::write(dest_dir.join("restore.ps1"), ps_restore)?;

    Ok(())
}

pub fn restore_curated_tables(conn: &Connection, curated_dir: &Path) -> Result<()> {
    if !curated_dir.exists() {
        return Ok(());
    }

    // A restore merges, it does not overwrite. Whatever this machine already
    // knows is kept: stars, tags and snippets are sets, and a note or summary
    // edited after the backup would otherwise be lost to an older remote copy.

    // 1. Favorites
    let fav_path = curated_dir.join("favorites.json");
    if fav_path.exists() {
        let rows = read_curated_rows(&fav_path)?;
        let mut stmt = conn
            .prepare("INSERT OR IGNORE INTO favorites (session_id, created_at) VALUES (?1, ?2)")?;
        for r in rows {
            let sid = r.get("session_id").and_then(|v| v.as_str());
            let created = r.get("created_at").and_then(|v| v.as_str());
            if let Some(s) = sid {
                stmt.execute(rusqlite::params![s, created])?;
            }
        }
    }

    // 2. Tags
    let tags_path = curated_dir.join("tags.json");
    if tags_path.exists() {
        let rows = read_curated_rows(&tags_path)?;
        let mut stmt =
            conn.prepare("INSERT OR IGNORE INTO tags (session_id, tag) VALUES (?1, ?2)")?;
        for r in rows {
            let sid = r.get("session_id").and_then(|v| v.as_str());
            let tag = r.get("tag").and_then(|v| v.as_str());
            if let (Some(s), Some(t)) = (sid, tag) {
                stmt.execute(rusqlite::params![s, t])?;
            }
        }
    }

    // 3. Notes - one per session, so the newer timestamp wins.
    let notes_path = curated_dir.join("notes.json");
    if notes_path.exists() {
        let rows = read_curated_rows(&notes_path)?;
        let mut stmt = conn.prepare(
            "INSERT INTO notes (session_id, note, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET note = excluded.note, updated = excluded.updated
             WHERE COALESCE(excluded.updated, '') > COALESCE(notes.updated, '')",
        )?;
        for r in rows {
            let sid = r.get("session_id").and_then(|v| v.as_str());
            let note = r.get("note").and_then(|v| v.as_str());
            let updated = r.get("updated").and_then(|v| v.as_str());
            if let (Some(s), Some(n)) = (sid, note) {
                stmt.execute(rusqlite::params![s, n, updated])?;
            }
        }
    }

    // 4. Summaries
    let sum_path = curated_dir.join("summaries.json");
    if sum_path.exists() {
        let rows = read_curated_rows(&sum_path)?;
        let mut stmt = conn.prepare(
            "INSERT INTO summaries (session_id, summary, created, kind) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET summary = excluded.summary, created = excluded.created, kind = excluded.kind
             WHERE COALESCE(excluded.created, '') > COALESCE(summaries.created, '')",
        )?;
        for r in rows {
            let sid = r.get("session_id").and_then(|v| v.as_str());
            let sum = r.get("summary").and_then(|v| v.as_str());
            let created = r.get("created").and_then(|v| v.as_str());
            let kind = r.get("kind").and_then(|v| v.as_str());
            if let (Some(s), Some(u)) = (sid, sum) {
                stmt.execute(rusqlite::params![s, u, created, kind])?;
            }
        }
    }

    // 5. Snippets. `snippets.id` is an INTEGER PRIMARY KEY AUTOINCREMENT that
    // each machine numbers from 1, so «INSERT OR IGNORE by id» would silently
    // drop the other machine's snippet on any id collision - and the id used to
    // be read with `.as_str()`, which never matched a number, so no snippet was
    // ever restored at all. Match on the content key instead: it dedupes a
    // repeated restore without renumbering or discarding anything local.
    let snip_path = curated_dir.join("snippets.json");
    if snip_path.exists() {
        let rows = read_curated_rows(&snip_path)?;
        let mut stmt = conn.prepare(
            "INSERT INTO snippets (session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10
             WHERE NOT EXISTS (
               SELECT 1 FROM snippets
               WHERE session_id = ?1 AND message_idx = ?2 AND title = ?3)",
        )?;
        for r in rows {
            let sid = r.get("session_id").and_then(|v| v.as_str());
            let midx = r.get("message_idx").and_then(|v| v.as_i64()).unwrap_or(0);
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let lang = r.get("lang").and_then(|v| v.as_str());
            let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let tags = r.get("tags").and_then(|v| v.as_str());
            let created = r.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
            let tool = r.get("tool").and_then(|v| v.as_str());
            let role = r.get("role").and_then(|v| v.as_str());
            let mts = r.get("message_ts").and_then(|v| v.as_str());
            if let Some(s) = sid {
                stmt.execute(rusqlite::params![
                    s, midx, title, lang, content, tags, created, tool, role, mts
                ])?;
            }
        }
    }

    Ok(())
}

/// One curated export file, or the error that made it unusable. Silently
/// skipping a corrupt `notes.json` used to look exactly like "nothing to
/// restore".
fn read_curated_rows(path: &Path) -> Result<Vec<Value>> {
    let data = fs::read_to_string(path)
        .with_context(|| format!("failed to read backup file {}", path.display()))?;
    serde_json::from_str::<Vec<Value>>(&data)
        .with_context(|| format!("backup file {} is not valid JSON", path.display()))
}

pub fn restore_granular_sessions(
    conn: &mut Connection,
    sessions_dir: &Path,
) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();
    if !sessions_dir.exists() {
        return Ok(report);
    }

    let mut session_paths = Vec::new();
    for entry in walkdir::WalkDir::new(sessions_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file()
            && entry.path().extension().and_then(|s| s.to_str()) == Some("json")
        {
            session_paths.push(entry.path().to_path_buf());
        }
    }
    // Stable order, so a repeated restore skips the same sessions it restored
    // first and a failure report is reproducible.
    session_paths.sort();

    let tx = conn.transaction()?;
    {
        let mut have_stmt = tx.prepare("SELECT 1 FROM files WHERE session_id = ?1 LIMIT 1")?;
        // `archived_at` marks this row as restored rather than indexed from
        // disk; the sync sweep only prunes rows with `archived_at IS NULL`, so
        // without it the next sync would delete what we just restored.
        let mut file_stmt = tx.prepare(
            "INSERT INTO files (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id, archived_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        )?;
        let mut msg_stmt = tx.prepare(
            "INSERT INTO messages (session_id, role, text, full_text, is_rewound) VALUES (?1, ?2, ?3, ?4, ?5)"
        )?;
        // `msgs` is an external-content FTS5 table over `messages`: it has one
        // column, `text`, and its rowid IS the messages.id. Writing
        // `(session_id, text)` there - as this used to - threw an error every
        // time and the `.ok()` hid it, so restored sessions searched as empty.
        let mut fts_stmt = tx
            .prepare("INSERT INTO msgs (rowid, text) VALUES (?1, ?2)")
            .context("failed to prepare msgs FTS insert during restore")?;
        let mut touched_stmt =
            tx.prepare("INSERT OR IGNORE INTO touched (session_id, path) VALUES (?1, ?2)")?;
        let mut commits_stmt = tx.prepare(
            "INSERT OR IGNORE INTO session_commits (session_id, commit_sha) VALUES (?1, ?2)",
        )?;

        let archived_at = Utc::now().to_rfc3339();

        for path in &session_paths {
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("restore: skipping {} ({e})", path.display());
                    report.failed += 1;
                    continue;
                }
            };
            let s: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("restore: skipping unreadable {} ({e})", path.display());
                    report.failed += 1;
                    continue;
                }
            };
            let Some(sid) = s.get("id").and_then(|v| v.as_str()) else {
                eprintln!("restore: skipping {} (no session id)", path.display());
                report.failed += 1;
                continue;
            };

            let exists: Option<i64> = have_stmt
                .query_row(rusqlite::params![sid], |r| r.get(0))
                .unwrap_or(None);
            if exists.is_some() {
                // The local copy is fresher, and its messages are already in
                // the index. Restoring again would double every row.
                report.skipped += 1;
                continue;
            }

            let tool = s.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
            let project = s.get("project").and_then(|v| v.as_str()).unwrap_or("");
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let started = s.get("started").and_then(|v| v.as_str());
            let ended = s.get("ended").and_then(|v| v.as_str());
            let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("main");
            let parent_id = s.get("parent_id").and_then(|v| v.as_str());
            let mtime = s.get("mtime").and_then(|v| v.as_i64()).unwrap_or(0);
            let msgs = s.get("messages").and_then(|v| v.as_array());
            let msg_count = msgs.map(|m| m.len()).unwrap_or(0) as i64;
            let fake_path = format!("restored://{tool}/{sid}");

            file_stmt.execute(rusqlite::params![
                fake_path,
                mtime,
                msg_count,
                sid,
                tool,
                project,
                title,
                started,
                ended,
                msg_count,
                kind,
                parent_id,
                archived_at
            ])?;

            if let Some(msgs) = msgs {
                for m in msgs {
                    let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    // Normalize and redact exactly as the indexer does: the
                    // external-content FTS row must be byte-identical to
                    // messages.text or a later rebuild corrupts the index. An
                    // old archive can also still hold raw secrets.
                    let raw = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let text = crate::redact::redact(&crate::util::nfc(raw)).into_owned();
                    let full_text = m
                        .get("full_text")
                        .and_then(|v| v.as_str())
                        .map(|t| crate::redact::redact(&crate::util::nfc(t)).into_owned());
                    let is_rewound = if m
                        .get("is_rewound")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        1
                    } else {
                        0
                    };

                    msg_stmt.execute(rusqlite::params![sid, role, text, full_text, is_rewound])?;
                    fts_stmt.execute(rusqlite::params![tx.last_insert_rowid(), text])?;
                }
            }

            if let Some(touched) = s.get("touched_files").and_then(|v| v.as_array()) {
                for t in touched {
                    if let Some(p) = t.as_str() {
                        touched_stmt.execute(rusqlite::params![sid, p])?;
                    }
                }
            }

            if let Some(commits) = s.get("commit_shas").and_then(|v| v.as_array()) {
                for c in commits {
                    if let Some(sha) = c.as_str() {
                        commits_stmt.execute(rusqlite::params![sid, sha])?;
                    }
                }
            }

            report.restored += 1;
        }
    }
    tx.commit()?;
    Ok(report)
}

// ---------------------------------------------------------------------------
// Helpers: Manifest and README Generation
// ---------------------------------------------------------------------------

fn query_session_stats(conn: &Connection) -> Result<(i64, BTreeMap<String, i64>)> {
    let total: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;

    let mut stmt =
        conn.prepare("SELECT tool, count(*) FROM files GROUP BY tool ORDER BY count(*) DESC")?;
    let mut stats = BTreeMap::new();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;

    for row in rows.flatten() {
        stats.insert(row.0, row.1);
    }

    Ok((total, stats))
}

fn write_manifest_and_readme(
    dest_dir: &Path,
    repo_url: &str,
    total_sessions: i64,
    stats: &BTreeMap<String, i64>,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let manifest = json!({
        "wikiagents_version": env!("CARGO_PKG_VERSION"),
        "created_at": now,
        // Never the credential: this file is committed to the remote.
        "repo_url": strip_url_credentials(repo_url),
        "total_sessions": total_sessions,
        "sessions_by_tool": stats,
    });

    fs::write(
        dest_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    // Generate informative GitHub README
    let mut md = String::new();
    md.push_str("# 🛡️ WikiAgents & SessionWiki Backup Repository\n\n");
    md.push_str(&format!(
        "Автоматический бэкап всех сессий ИИ-агентов и веб-чатов.\n\n\
         - **Дата выгрузки:** `{}`\n\
         - **Всего сессий в бэкапе:** `{}`\n\
         - **Версия WikiAgents:** `{}`\n\n",
        now,
        total_sessions,
        env!("CARGO_PKG_VERSION")
    ));

    md.push_str(&format!(
        "## 📊 Статистика по инструментам ({} в этом бэкапе)\n\n",
        stats.len()
    ));
    md.push_str("| Инструмент (Tool) | Количество сессий |\n");
    md.push_str("|---|---:|\n");
    for (tool, count) in stats {
        md.push_str(&format!("| `{tool}` | {count} |\n"));
    }
    md.push_str(&format!("| **ИТОГО** | **{total_sessions}** |\n\n"));

    md.push_str("## 📁 Структура репозитория\n\n");
    md.push_str("- `sessions/` — **Все сессии всех IDE-агентов (OpenCode, Claude Code, Codex, Antigravity, Qoder, ZCode и др.) в открытом JSON**, разбитые по инструментам (`sessions/<tool>/<id>.json`). Содержат полные тексты диалогов, диффы, затронутые файлы и связанные коммиты. Можно читать прямо в интерфейсе GitHub!\n");
    md.push_str("- `curated/` — Человеческие знания в открытом JSON: избранные сессии ⭐, теги, заметки, KB-сниппеты 📌 и саммари.\n");
    md.push_str("- `web_session/` — Исходные JSON-выгрузки веб-чатов (Claude, DeepSeek, Gemini, Qwen, GLM).\n");
    md.push_str("- `manifest.json` — Машинночитаемая сводка бэкапа.\n");
    md.push_str("- `restore.py` / `restore.ps1` — Скрипты мгновенного восстановления рабочей базы `index.db`.\n\n");

    md.push_str("## 🔄 Как восстановить данные на новом компьютере\n\n");
    md.push_str("### Вариант 1 (Автоматически через приложение):\n");
    md.push_str("В приложении WikiAgents откройте окно «Бэкап», укажите URL этого репозитория и нажмите «Синхронизировать / Восстановить».\n\n");
    md.push_str("### Вариант 2 (Вручную из терминала):\n");
    md.push_str("```bash\npython restore.py\n```\n");
    md.push_str("После этого готовый `index.db` помещается в рабочую папку `data/` приложения WikiAgents.\n");

    fs::write(dest_dir.join("README.md"), md)?;

    Ok(())
}

fn count_files_in_dir(dir: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| e.file_name() != ".git")
        .flatten()
    {
        if entry.file_type().is_file() {
            count += 1;
        }
    }
    Ok(count)
}

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    let val: Result<String, _> =
        conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0));
    match val {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta(key, value) VALUES(?1, ?2)",
        [key, value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn test_backup_config_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT)", [])
            .unwrap();

        let cfg = get_backup_config(&conn).unwrap();
        assert_eq!(cfg.repo_url, "");
        assert_eq!(cfg.last_backup_at, None);

        set_backup_config(
            &conn,
            "https://github.com/AlexZander85/wikiagents-backup.git",
        )
        .unwrap();
        let cfg2 = get_backup_config(&conn).unwrap();
        assert_eq!(
            cfg2.repo_url,
            "https://github.com/AlexZander85/wikiagents-backup.git"
        );
    }

    #[test]
    fn test_test_connection_empty_url() {
        let res = test_connection("");
        assert!(!res.ok);
        assert!(res.message.contains("не указан"));
    }

    #[test]
    fn strip_url_credentials_keeps_the_address_drops_the_secret() {
        assert_eq!(
            strip_url_credentials("https://user:ghp_secret@github.com/me/repo.git"),
            "https://github.com/me/repo.git"
        );
        // token without a username, and a path that itself contains an '@'
        assert_eq!(
            strip_url_credentials("https://tok@github.com/me/repo.git"),
            "https://github.com/me/repo.git"
        );
        assert_eq!(
            strip_url_credentials("https://tok@github.com/me/repo@2.git"),
            "https://github.com/me/repo@2.git"
        );
        // Nothing to strip stays byte-identical, including ssh scp-style URLs
        // where the '@' is the account, not a credential we invented.
        assert_eq!(
            strip_url_credentials("https://github.com/me/repo.git"),
            "https://github.com/me/repo.git"
        );
        assert_eq!(
            strip_url_credentials("git@github.com:me/repo.git"),
            "git@github.com:me/repo.git"
        );
        assert_eq!(strip_url_credentials(""), "");
    }

    #[test]
    fn web_session_export_redacts_secrets_and_stays_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        let out_dir = dir.path().join("out");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&out_dir).unwrap();

        let secret = "sk-abcdefghijklmnopqrstuvwxyz012345";
        let src = src_dir.join("chat.json");
        fs::write(
            &src,
            format!(r#"{{"id":"c1","messages":[{{"role":"user","text":"key {secret} here"}}]}}"#),
        )
        .unwrap();

        assert!(export_one_web_session(&src, &out_dir, "chat.json").is_ok());
        let written = fs::read_to_string(out_dir.join("chat.json")).unwrap();
        assert!(!written.contains(secret), "secret leaked into the backup");
        assert!(
            written.contains("[redacted:openai]"),
            "marker missing: {written}"
        );
        // The markers hold no quotes, so the transcript must still parse.
        let parsed: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(parsed["id"], "c1");
    }

    #[test]
    fn derived_names_are_removed_before_a_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.json"), "old whole file").unwrap();
        fs::write(dir.path().join("big.json.part000"), "old head").unwrap();
        fs::write(dir.path().join("big.json.part001"), "old tail").unwrap();
        fs::write(dir.path().join("other.json.part000"), "keep").unwrap();

        remove_derived_names(dir.path(), "big.json").unwrap();

        assert!(!dir.path().join("big.json").exists());
        assert!(!dir.path().join("big.json.part000").exists());
        assert!(!dir.path().join("big.json.part001").exists());
        assert!(dir.path().join("other.json.part000").exists());
    }

    #[test]
    fn only_one_backup_run_may_hold_the_slot() {
        // Serial by construction: the second claim must fail while the guard is
        // alive, no matter which thread got here first.
        let guard_claimed = try_begin();
        assert!(
            guard_claimed,
            "slot should be free at the start of the test"
        );
        assert!(
            !try_begin(),
            "a second run must not start while one is active"
        );
        drop(RunningGuard);
        assert!(try_begin(), "the slot is released when the guard drops");
        drop(RunningGuard);
    }
}
