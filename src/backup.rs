//! Backup and sync wikiagents data to a private GitHub repository.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
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
            message: "Подключение успешно! Доступ к приватному репозиторию подтвержден."
                .to_string(),
            git_version,
            git_user,
        },
        Ok(output) => {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
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

            // Push to origin main with retries and force fallback if history diverged
            let mut push_res = run_git_cmd(&backup_dir, &["push", "-u", "origin", "main"]);
            if push_res.is_err() {
                push_res = run_git_cmd(&backup_dir, &["push", "-u", "origin", "main", "--force"]);
                if push_res.is_err() {
                    for attempt in 1..=3 {
                        std::thread::sleep(std::time::Duration::from_secs(2 * attempt));
                        push_res =
                            run_git_cmd(&backup_dir, &["push", "-u", "origin", "main", "--force"]);
                        if push_res.is_ok() {
                            break;
                        }
                    }
                }
            }
            push_res?;
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
pub fn run_restore(conn: &mut Connection, repo_url: &str) -> Result<usize> {
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

    let res = (|| -> Result<usize> {
        let backup_dir = resolve_backup_dir()?;
        ensure_git_repo(&backup_dir, repo_url)?;

        // 1. Fetch and pull latest from origin main
        let _ = run_git_cmd(&backup_dir, &["fetch", "origin", "main"]);
        let _ = run_git_cmd(&backup_dir, &["checkout", "-B", "main", "origin/main"]);
        let _ = run_git_cmd(&backup_dir, &["pull", "origin", "main"]);
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
        let total = restore_granular_sessions(conn, &backup_dir.join("sessions"))?;
        log_step(
            3,
            "Сессии восстановлены",
            &format!("[3/4] Восстановление сессий... OK (всего: {})", total),
        );

        // 4. Update local meta
        set_meta(conn, REPO_URL_KEY, repo_url)?;
        set_meta(conn, LAST_STATUS_KEY, "ok")?;
        log_step(
            4,
            "Готово",
            &format!(
                "[4/4] УСПЕШНО! Восстановлено {} сессий из GitHub бэкапа.",
                total
            ),
        );

        Ok(total)
    })();

    match res {
        Ok(total) => {
            set_progress(|p| {
                p.is_running = false;
                p.stage = "Восстановление завершено".to_string();
            });
            Ok(total)
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Helpers: Curated Data Export
// ---------------------------------------------------------------------------

fn export_curated_tables(conn: &Connection, dest_dir: &Path) -> Result<usize> {
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
        .filter_map(Result::ok)
        .collect();
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
        .filter_map(Result::ok)
        .collect();
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
        .filter_map(Result::ok)
        .collect();
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
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "session_id": r.get::<_, String>(1)?,
                "message_idx": r.get::<_, i64>(2)?,
                "title": r.get::<_, String>(3)?,
                "lang": r.get::<_, String>(4)?,
                "content": r.get::<_, String>(5)?,
                "tags": r.get::<_, String>(6)?,
                "created_at": r.get::<_, String>(7)?,
                "tool": r.get::<_, Option<String>>(8)?,
                "role": r.get::<_, Option<String>>(9)?,
                "message_ts": r.get::<_, Option<i64>>(10)?,
            }))
        })?
        .filter_map(Result::ok)
        .collect();
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
        .filter_map(Result::ok)
        .collect();
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
            let size = fs::metadata(&path)?.len();

            if size > MAX_CHUNK_BYTES {
                // Split into chunks <= MAX_CHUNK_BYTES so GitHub will never reject it
                split_file(&path, dest_dir, &file_name)?;
            } else {
                fs::copy(&path, dest_dir.join(&file_name))?;
            }
            copied_files.push(file_name);
        }
    }

    // Write recombine helper scripts
    write_recombine_helpers(dest_dir)?;

    Ok(copied_files.len())
}

fn split_file(src_path: &Path, dest_dir: &Path, base_name: &str) -> Result<usize> {
    let mut src = File::open(src_path)?;
    let mut buf = vec![0u8; MAX_CHUNK_BYTES as usize];
    let mut chunk_idx = 0;

    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk_name = format!("{}.part{:02}", base_name, chunk_idx);
        let mut out = File::create(dest_dir.join(&chunk_name))?;
        out.write_all(&buf[..n])?;
        chunk_idx += 1;
    }

    Ok(chunk_idx)
}

fn write_recombine_helpers(dest_dir: &Path) -> Result<()> {
    // Python script to recombine any .partXX files in the directory
    let py_script = r#"#!/usr/bin/env python3
"""Recombines split .partXX files into their original full-size files."""
import os, glob, re

def recombine():
    parts = glob.glob("*.part00")
    for first_part in parts:
        base_name = first_part[:-7]
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
    let ps_script = r#"# Recombines split .partXX files into their original full-size files
Get-ChildItem -Filter "*.part00" | ForEach-Object {
    $baseName = $_.Name.Substring(0, $_.Name.Length - 7)
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

fn export_granular_sessions(conn: &Connection, dest_dir: &Path) -> Result<(usize, usize)> {
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
        .filter_map(Result::ok)
        .collect();
    drop(stmt);

    let total = rows.len();
    let mut newly_written = 0;

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
            .filter_map(Result::ok)
            .collect();

        // Fetch touched files
        let touched: Vec<String> = touched_stmt
            .query_map([&row.session_id], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();

        // Fetch commits
        let commits: Vec<String> = commits_stmt
            .query_map([&row.session_id], |r| r.get(0))?
            .filter_map(Result::ok)
            .collect();

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

    Ok((total, newly_written))
}

fn write_restore_scripts(dest_dir: &Path) -> Result<()> {
    let py_restore = r#"#!/usr/bin/env python3
"""
WikiAgents Backup Restorer.
Restores all sessions, curated data, and web chats from this repository into a SQLite index.db.
Usage:
    python restore.py [dest_index.db]
"""
import os, sys, glob, json, sqlite3

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

    try:
        cur.execute("""CREATE VIRTUAL TABLE IF NOT EXISTS msgs USING fts5(session_id UNINDEXED, text)""")
    except Exception:
        pass

    # 1. Restore Curated
    curated_dir = "curated"
    if os.path.exists(curated_dir):
        print("[*] Restoring curated tables...")
        for name, tbl, cols in [
            ("favorites.json", "favorites", ["session_id", "created_at"]),
            ("tags.json", "tags", ["session_id", "tag"]),
            ("notes.json", "notes", ["session_id", "note", "updated"]),
            ("summaries.json", "summaries", ["session_id", "summary", "created", "kind"]),
            ("snippets.json", "snippets", ["id", "session_id", "message_idx", "title", "lang", "content", "tags", "created_at", "tool", "role", "message_ts"]),
        ]:
            p = os.path.join(curated_dir, name)
            if os.path.exists(p):
                with open(p, "r", encoding="utf-8") as f:
                    rows = json.load(f)
                q = f"INSERT OR REPLACE INTO {tbl} ({','.join(cols)}) VALUES ({','.join(['?']*len(cols))})"
                cur.executemany(q, [[r.get(c) for c in cols] for r in rows])
                print(f"  [OK] {tbl}: {len(rows)} rows restored.")

    # 2. Restore Sessions
    session_files = glob.glob(os.path.join("sessions", "*", "*.json"))
    print(f"[*] Restoring {len(session_files)} granular sessions...")
    restored = 0
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

            fake_path = f"restored://{tool}/{sid}"
            cur.execute("""INSERT OR REPLACE INTO files
                (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (fake_path, mtime, len(msgs), sid, tool, proj, title, started, ended, len(msgs), kind, parent_id)
            )

            for m in msgs:
                role = m.get("role", "user")
                text = m.get("text", "")
                full_text = m.get("full_text")
                is_rewound = 1 if m.get("is_rewound") else 0
                cur.execute("""INSERT INTO messages (session_id, role, text, full_text, is_rewound)
                    VALUES (?, ?, ?, ?, ?)""", (sid, role, text, full_text, is_rewound))
                try:
                    cur.execute("INSERT INTO msgs(session_id, text) VALUES (?, ?)", (sid, text))
                except Exception:
                    pass

            for t in touched:
                cur.execute("INSERT OR IGNORE INTO touched (session_id, path) VALUES (?, ?)", (sid, t))

            for c in commits:
                cur.execute("INSERT OR IGNORE INTO session_commits (session_id, commit_sha) VALUES (?, ?)", (sid, c))

            restored += 1
        except Exception:
            continue

    conn.commit()
    conn.close()
    print(f"[OK] SUCCESS: {restored} sessions successfully restored to {dest_db}!")

if __name__ == "__main__":
    dest = sys.argv[1] if len(sys.argv) > 1 else "index.db"
    restore(dest)
"#;
    fs::write(dest_dir.join("restore.py"), py_restore)?;

    let ps_restore = r#"# WikiAgents Backup Restorer (PowerShell)
param([string]$dest = "index.db")

if (Get-Command python -ErrorAction SilentlyContinue) {
    Write-Host "Restoring WikiAgents database via python restore.py $dest..."
    python restore.py $dest
} else {
    Write-Host "Python not found. Please install Python 3 or use WikiAgents to restore sessions."
}
"#;
    fs::write(dest_dir.join("restore.ps1"), ps_restore)?;

    Ok(())
}

fn restore_curated_tables(conn: &Connection, curated_dir: &Path) -> Result<()> {
    if !curated_dir.exists() {
        return Ok(());
    }

    // 1. Favorites
    let fav_path = curated_dir.join("favorites.json");
    if fav_path.exists() {
        if let Ok(data) = fs::read_to_string(&fav_path) {
            if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&data) {
                let mut stmt = conn.prepare(
                    "INSERT OR REPLACE INTO favorites (session_id, created_at) VALUES (?1, ?2)",
                )?;
                for r in rows {
                    let sid = r.get("session_id").and_then(|v| v.as_str());
                    let created = r.get("created_at").and_then(|v| v.as_str());
                    if let Some(s) = sid {
                        let _ = stmt.execute(rusqlite::params![s, created]);
                    }
                }
            }
        }
    }

    // 2. Tags
    let tags_path = curated_dir.join("tags.json");
    if tags_path.exists() {
        if let Ok(data) = fs::read_to_string(&tags_path) {
            if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&data) {
                let mut stmt =
                    conn.prepare("INSERT OR REPLACE INTO tags (session_id, tag) VALUES (?1, ?2)")?;
                for r in rows {
                    let sid = r.get("session_id").and_then(|v| v.as_str());
                    let tag = r.get("tag").and_then(|v| v.as_str());
                    if let (Some(s), Some(t)) = (sid, tag) {
                        let _ = stmt.execute(rusqlite::params![s, t]);
                    }
                }
            }
        }
    }

    // 3. Notes
    let notes_path = curated_dir.join("notes.json");
    if notes_path.exists() {
        if let Ok(data) = fs::read_to_string(&notes_path) {
            if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&data) {
                let mut stmt = conn.prepare(
                    "INSERT OR REPLACE INTO notes (session_id, note, updated) VALUES (?1, ?2, ?3)",
                )?;
                for r in rows {
                    let sid = r.get("session_id").and_then(|v| v.as_str());
                    let note = r.get("note").and_then(|v| v.as_str());
                    let updated = r.get("updated").and_then(|v| v.as_str());
                    if let (Some(s), Some(n)) = (sid, note) {
                        let _ = stmt.execute(rusqlite::params![s, n, updated]);
                    }
                }
            }
        }
    }

    // 4. Summaries
    let sum_path = curated_dir.join("summaries.json");
    if sum_path.exists() {
        if let Ok(data) = fs::read_to_string(&sum_path) {
            if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&data) {
                let mut stmt = conn.prepare(
                    "INSERT OR REPLACE INTO summaries (session_id, summary, created, kind) VALUES (?1, ?2, ?3, ?4)",
                )?;
                for r in rows {
                    let sid = r.get("session_id").and_then(|v| v.as_str());
                    let sum = r.get("summary").and_then(|v| v.as_str());
                    let created = r.get("created").and_then(|v| v.as_str());
                    let kind = r.get("kind").and_then(|v| v.as_str());
                    if let (Some(s), Some(u)) = (sid, sum) {
                        let _ = stmt.execute(rusqlite::params![s, u, created, kind]);
                    }
                }
            }
        }
    }

    // 5. Snippets
    let snip_path = curated_dir.join("snippets.json");
    if snip_path.exists() {
        if let Ok(data) = fs::read_to_string(&snip_path) {
            if let Ok(rows) = serde_json::from_str::<Vec<Value>>(&data) {
                let mut stmt = conn.prepare(
                    "INSERT OR REPLACE INTO snippets (id, session_id, message_idx, title, lang, content, tags, created_at, tool, role, message_ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )?;
                for r in rows {
                    let id = r.get("id").and_then(|v| v.as_str());
                    let sid = r.get("session_id").and_then(|v| v.as_str());
                    let midx = r.get("message_idx").and_then(|v| v.as_i64()).unwrap_or(0);
                    let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let lang = r.get("lang").and_then(|v| v.as_str()).unwrap_or("");
                    let content = r.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    let tags = r.get("tags").and_then(|v| v.as_str()).unwrap_or("");
                    let created = r.get("created_at").and_then(|v| v.as_str()).unwrap_or("");
                    let tool = r.get("tool").and_then(|v| v.as_str());
                    let role = r.get("role").and_then(|v| v.as_str());
                    let mts = r.get("message_ts").and_then(|v| v.as_i64());
                    if let (Some(i), Some(s)) = (id, sid) {
                        let _ = stmt.execute(rusqlite::params![
                            i, s, midx, title, lang, content, tags, created, tool, role, mts
                        ]);
                    }
                }
            }
        }
    }

    Ok(())
}

fn restore_granular_sessions(conn: &mut Connection, sessions_dir: &Path) -> Result<usize> {
    if !sessions_dir.exists() {
        return Ok(0);
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

    let total = session_paths.len();
    if total == 0 {
        return Ok(0);
    }

    let tx = conn.transaction()?;
    {
        let mut file_stmt = tx.prepare(
            "INSERT OR REPLACE INTO files (path, mtime, size, session_id, tool, project, title, started, ended, msg_count, kind, parent_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)"
        )?;
        let mut msg_stmt = tx.prepare(
            "INSERT INTO messages (session_id, role, text, full_text, is_rewound) VALUES (?1, ?2, ?3, ?4, ?5)"
        )?;
        let mut fts_stmt = tx
            .prepare("INSERT INTO msgs (session_id, text) VALUES (?1, ?2)")
            .ok();
        let mut touched_stmt =
            tx.prepare("INSERT OR IGNORE INTO touched (session_id, path) VALUES (?1, ?2)")?;
        let mut commits_stmt = tx.prepare(
            "INSERT OR IGNORE INTO session_commits (session_id, commit_sha) VALUES (?1, ?2)",
        )?;

        for path in session_paths {
            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let s: Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let sid = match s.get("id").and_then(|v| v.as_str()) {
                Some(id) => id,
                None => continue,
            };
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

            let _ = file_stmt.execute(rusqlite::params![
                fake_path, mtime, msg_count, sid, tool, project, title, started, ended, msg_count,
                kind, parent_id
            ]);

            if let Some(msgs) = msgs {
                for m in msgs {
                    let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    let text = m.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let full_text = m.get("full_text").and_then(|v| v.as_str());
                    let is_rewound = if m
                        .get("is_rewound")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        1
                    } else {
                        0
                    };

                    let _ =
                        msg_stmt.execute(rusqlite::params![sid, role, text, full_text, is_rewound]);
                    if let Some(ref mut fts) = fts_stmt {
                        let _ = fts.execute(rusqlite::params![sid, text]);
                    }
                }
            }

            if let Some(touched) = s.get("touched_files").and_then(|v| v.as_array()) {
                for t in touched {
                    if let Some(p) = t.as_str() {
                        let _ = touched_stmt.execute(rusqlite::params![sid, p]);
                    }
                }
            }

            if let Some(commits) = s.get("commit_shas").and_then(|v| v.as_array()) {
                for c in commits {
                    if let Some(sha) = c.as_str() {
                        let _ = commits_stmt.execute(rusqlite::params![sid, sha]);
                    }
                }
            }
        }
    }
    tx.commit()?;
    Ok(total)
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
        "repo_url": repo_url,
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

    md.push_str("## 📊 Статистика по инструментам (все 18 провайдеров)\n\n");
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
}
