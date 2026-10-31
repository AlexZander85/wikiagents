use crate::model::Role;
use crate::{commands, index, resume};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::json;
use std::path::Path;
use tiny_http::{Header, Response, Server};

/// Local read-only viewer over the index. By default it does not sync, so it
/// stays snappy and can run while a CLI index pass is in progress (WAL allows
/// concurrent readers) - which means sessions created after the last
/// `list`/`search` won't show until the index is refreshed. Pass `sync` to
/// bring the index up to date once before serving.
pub fn serve(port: u16, no_open: bool, sync: bool) -> Result<()> {
    let mut conn = index::open()?;
    if sync {
        index::sync(&mut conn, None)?;
    }
    let addr = format!("127.0.0.1:{port}");
    let server = Server::http(&addr).map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?;
    let url = format!("http://{addr}");
    println!("agentwiki web: {url}");
    if !sync {
        println!("(read-only view; run `agentwiki list` or `web --sync` to refresh the index)");
    }
    let wsl = is_wsl();
    if should_open_browser(no_open, wsl) {
        open_browser(&url);
    } else if wsl && !no_open {
        println!("(WSL detected - open the URL above in your browser; auto-open is off here to avoid stray windows)");
    }

    // Several handlers, each with its own connection: WAL lets them read side
    // by side, so a sync or a slow search no longer stalls every other request
    // (the desktop app's whole UI) until it finishes.
    let server = std::sync::Arc::new(server);
    let sync_lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    {
        let sync_lock = sync_lock.clone();
        std::thread::spawn(move || auto_compact_loop(&sync_lock));
    }
    for _ in 1..WEB_WORKERS {
        let server = server.clone();
        let sync_lock = sync_lock.clone();
        std::thread::spawn(move || {
            let mut conn = match index::open() {
                Ok(conn) => conn,
                Err(e) => {
                    eprintln!("web: handler could not open the index: {e:#}");
                    return;
                }
            };
            for request in server.incoming_requests() {
                handle_request(&mut conn, request, port, &sync_lock);
            }
        });
    }
    for request in server.incoming_requests() {
        handle_request(&mut conn, request, port, &sync_lock);
    }
    Ok(())
}

/// Request handler threads `serve` runs, the calling thread included.
const WEB_WORKERS: usize = 4;

/// When the last request arrived (Unix seconds), for idle detection.
static LAST_REQUEST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn seconds_since_last_request() -> i64 {
    chrono::Utc::now().timestamp() - LAST_REQUEST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Full compaction without anyone running `sessionwiki vacuum`: the app (or
/// `web`) checks now and then whether [`index::compact_due`] says it is worth
/// it, and runs it only while nobody is using the UI - no request for a couple
/// of minutes and no sync in flight. It never waits on another process's
/// write lock; a busy index is tried again at the next check.
fn auto_compact_loop(sync_lock: &std::sync::Mutex<()>) {
    use std::time::{Duration, Instant};
    const FIRST_CHECK: Duration = Duration::from_secs(3 * 60);
    const CHECK_EVERY: Duration = Duration::from_secs(10 * 60);
    const IDLE_SECS: i64 = 2 * 60;
    const AFTER_FAILURE: Duration = Duration::from_secs(6 * 60 * 60);

    std::thread::sleep(FIRST_CHECK);
    let mut not_before = Instant::now();
    loop {
        if Instant::now() >= not_before && seconds_since_last_request() >= IDLE_SECS {
            if let Ok(_guard) = sync_lock.try_lock() {
                match try_compact() {
                    Ok(Some((reason, report))) => eprintln!(
                        "web: compacted the index ({reason}): {} -> {}",
                        crate::util::human_size(report.before),
                        crate::util::human_size(report.after),
                    ),
                    Ok(None) => {}
                    Err(e) if is_busy(&e) => {}
                    Err(e) => {
                        eprintln!("web: automatic compaction failed: {e:#}");
                        not_before = Instant::now() + AFTER_FAILURE;
                    }
                }
            }
        }
        std::thread::sleep(CHECK_EVERY);
    }
}

fn try_compact() -> Result<Option<(&'static str, index::CompactReport)>> {
    let conn = index::open()?;
    let path = index::db_path()?;
    let Some(reason) = index::compact_due(&conn, &path)? else {
        return Ok(None);
    };
    conn.busy_timeout(std::time::Duration::ZERO)?;
    Ok(Some((reason, index::compact(&conn, &path)?)))
}

fn is_busy(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    })
}

fn handle_request(
    conn: &mut Connection,
    mut request: tiny_http::Request,
    port: u16,
    sync_lock: &std::sync::Mutex<()>,
) {
    // Defend against DNS rebinding: a page on a malicious site can rebind its
    // own hostname to 127.0.0.1 and have the browser fetch this server, then
    // read your sessions same-origin. Such a request carries the attacker's
    // hostname in the Host header, so only serve requests addressed to the
    // loopback name we actually bound.
    let host = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .map(|h| h.value.as_str().to_string());
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string());
    // Host check defeats DNS rebinding (an attacker hostname rebound to
    // 127.0.0.1 carries its name in Host). The Origin check is a second layer
    // against a plain cross-origin fetch from a malicious site, which the
    // browser stamps with its Origin - we don't rely on the same-origin
    // policy alone.
    if !host_matches(host.as_deref(), port) || !origin_ok(origin.as_deref(), port) {
        let _ = request.respond(Response::from_string("forbidden").with_status_code(403));
        return;
    }
    LAST_REQUEST.store(
        chrono::Utc::now().timestamp(),
        std::sync::atomic::Ordering::Relaxed,
    );
    let gzip = request.headers().iter().any(|h| {
        h.field.equiv("Accept-Encoding")
            && h.value
                .as_str()
                .split(',')
                .any(|e| e.trim().starts_with("gzip"))
    });
    ACCEPT_GZIP.with(|g| g.set(gzip));
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    // CSRF: no browser request that a hostile page can make on its own -
    // `<img src>`, `<form>` or a navigation - is able to carry a custom header,
    // and a cross-origin `fetch` that adds one is held back by a CORS preflight
    // this server never answers. Only the UI's own `jget`/`jpost` send it.
    if path.starts_with("/api/") {
        let from_our_ui = request
            .headers()
            .iter()
            .any(|h| h.field.equiv("X-WikiAgents") && h.value.as_str().trim() == "1");
        if !from_our_ui {
            let _ = request.respond(Response::from_string("forbidden").with_status_code(403));
            return;
        }
        // Reading the index over the API is harmless; anything that changes
        // state or exports the database must be an explicit POST.
        if POST_ONLY_PATHS.contains(&path) && request.method().as_str() != "POST" {
            let _ =
                request.respond(Response::from_string("method not allowed").with_status_code(405));
            return;
        }
    }
    let result = match path {
        "/" => html(INDEX_HTML),
        "/api/stats" => api_stats(conn),
        "/api/sessions" => api_sessions(conn, query),
        "/api/search" => api_search(conn, query),
        "/api/trace" => api_trace(conn, query),
        "/api/file" => api_file(conn, query),
        "/api/projects" => api_projects(conn),
        "/api/tag" => api_tag(conn, query),
        "/api/sync" => api_sync(conn, sync_lock),
        "/api/favorites" => api_favorites(conn),
        "/api/favorite" => api_favorite(conn, query),
        "/api/forget" => api_forget(conn, &mut request, query),
        "/api/brief" => api_brief(conn, query),
        "/api/snippets" => api_snippets(conn, query),
        "/api/snippet/save" => api_snippet_save(conn, &mut request, query),
        "/api/snippet/delete" => api_snippet_delete(conn, &mut request, query),
        "/api/snippet/rename" => api_snippet_rename(conn, &mut request, query),
        "/api/subagents" => api_subagents(conn, query),
        "/api/analytics" => api_analytics(conn),
        "/api/analytics/files" => api_analytics_files(conn, query),
        "/api/analytics/langs" => api_analytics_langs(conn, query),
        "/api/backup/status" => api_backup_status(conn),
        "/api/backup/config" => api_backup_config(conn, &mut request, query),
        "/api/backup/test" => api_backup_test(&mut request, query),
        "/api/backup/run" => api_backup_run(conn, &mut request, query),
        "/api/backup/restore" => api_backup_restore(conn, &mut request, query),
        p if p.starts_with("/api/commit/") => {
            api_commit(conn, p.trim_start_matches("/api/commit/"))
        }
        "/api/commit" => {
            let sha = param(query, "sha").unwrap_or_default();
            api_commit(conn, &sha)
        }
        p if p.starts_with("/api/related/") => {
            api_related(conn, p.trim_start_matches("/api/related/"))
        }
        p if p.starts_with("/api/session/") => {
            api_session(conn, p.trim_start_matches("/api/session/"), query)
        }
        _ => Ok(Response::from_string("not found")
            .with_status_code(404)
            .boxed()),
    };
    let response = match result {
        Ok(r) => r,
        Err(e) => Response::from_string(json!({ "error": e.to_string() }).to_string())
            .with_status_code(500)
            .boxed(),
    };
    let _ = request.respond(response);
}

type Boxed = Response<Box<dyn std::io::Read + Send>>;

/// Endpoints that mutate the index or move data out of it. They accept POST
/// only, so a crafted URL can never be triggered by a plain GET.
const POST_ONLY_PATHS: &[&str] = &[
    "/api/backup/config",
    "/api/backup/restore",
    "/api/backup/run",
    "/api/forget",
    "/api/snippet/delete",
    "/api/snippet/rename",
    "/api/snippet/save",
];

/// True only for the loopback host:port we bound. Anything else (notably an
/// attacker hostname rebound to 127.0.0.1) is rejected, defeating DNS rebinding.
fn host_matches(host: Option<&str>, port: u16) -> bool {
    match host {
        Some(h) => {
            h == format!("127.0.0.1:{port}")
                || h == format!("localhost:{port}")
                || h == format!("[::1]:{port}")
        }
        None => false,
    }
}

/// A present Origin that is not our own loopback origin means a cross-origin
/// request (e.g. a `fetch` from a malicious site) - refuse it. A missing Origin
/// (top-level navigation, same-origin GET) is allowed.
fn origin_ok(origin: Option<&str>, port: u16) -> bool {
    match origin {
        None => true,
        Some(o) => {
            o == format!("http://127.0.0.1:{port}")
                || o == format!("http://localhost:{port}")
                || o == format!("http://[::1]:{port}")
        }
    }
}

thread_local! {
    /// Whether the request this handler thread is serving accepts gzip. Each
    /// handler serves one request at a time, so the flag is set per request.
    static ACCEPT_GZIP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A response body, gzip-compressed when the client accepts it and the body is
/// big enough to be worth it (session transcripts run to tens of MB of JSON).
fn encoded_body(body: Vec<u8>) -> Response<std::io::Cursor<Vec<u8>>> {
    use std::io::Write;
    if body.len() >= 1024 && ACCEPT_GZIP.with(|g| g.get()) {
        let mut enc = flate2::write::GzEncoder::new(
            Vec::with_capacity(body.len() / 4),
            flate2::Compression::fast(),
        );
        if enc.write_all(&body).is_ok() {
            if let Ok(gz) = enc.finish() {
                return Response::from_data(gz)
                    .with_header(
                        Header::from_bytes(&b"Content-Encoding"[..], &b"gzip"[..]).unwrap(),
                    )
                    .with_header(
                        Header::from_bytes(&b"Vary"[..], &b"Accept-Encoding"[..]).unwrap(),
                    );
            }
        }
    }
    Response::from_data(body)
}

fn html(body: &str) -> Result<Boxed> {
    Ok(encoded_body(body.as_bytes().to_vec())
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        )
        .with_header(
            Header::from_bytes(
                &b"Cache-Control"[..],
                &b"no-cache, no-store, must-revalidate"[..],
            )
            .unwrap(),
        )
        .with_header(Header::from_bytes(&b"Pragma"[..], &b"no-cache"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Expires"[..], &b"0"[..]).unwrap())
        .boxed())
}

fn json_response(v: serde_json::Value) -> Result<Boxed> {
    json_response_with_status(v, 200)
}

fn json_response_with_status(v: serde_json::Value, status: u16) -> Result<Boxed> {
    Ok(encoded_body(v.to_string().into_bytes())
        .with_status_code(status)
        .with_header(
            Header::from_bytes(
                &b"Content-Type"[..],
                &b"application/json; charset=utf-8"[..],
            )
            .unwrap(),
        )
        .boxed())
}

fn api_stats(conn: &Connection) -> Result<Boxed> {
    let mut stmt = conn.prepare(
        "SELECT tool, count(*), sum(size), sum(msg_count) FROM files GROUP BY tool ORDER BY 2 DESC",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "tool": r.get::<_, String>(0)?,
                "sessions": r.get::<_, i64>(1)?,
                "bytes": r.get::<_, i64>(2)?,
                "messages": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let tag_counts = index::tag_counts(conn).unwrap_or_default();
    let tags: Vec<serde_json::Value> = tag_counts
        .into_iter()
        .map(|(t, c)| json!({ "tag": t, "count": c }))
        .collect();

    json_response(json!({ "tools": rows, "tags": tags, "version": env!("CARGO_PKG_VERSION") }))
}

fn api_sessions(conn: &Connection, query: &str) -> Result<Boxed> {
    let tool = param(query, "tool");
    let project = param(query, "project");
    let tag = param(query, "tag");
    let since = param(query, "since");
    let until = param(query, "until");
    let limit = param(query, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let include_subagents = param(query, "subagents").is_some_and(|v| v == "1" || v == "true")
        || param(query, "all").is_some_and(|v| v == "1" || v == "true");
    let rows = index::recent_range(
        conn,
        limit,
        &index::ListQuery {
            tool: tool.as_deref(),
            project: project.as_deref(),
            tag: tag.as_deref(),
            since: since.as_deref(),
            until: until.as_deref(),
            include_subagents,
        },
        index::SessionSort::parse(param(query, "sort").as_deref().unwrap_or("")),
    )?;
    json_response(json!(rows.iter().map(row_json).collect::<Vec<_>>()))
}

fn api_projects(conn: &Connection) -> Result<Boxed> {
    let rows = index::projects(conn)?;
    json_response(json!(rows
        .iter()
        .map(|p| json!({
            "project": p.project,
            "sessions": p.sessions,
            "messages": p.messages,
            "newest": p.newest,
        }))
        .collect::<Vec<_>>()))
}

fn api_related(conn: &Connection, id: &str) -> Result<Boxed> {
    let rel = index::related(conn, id, 8)?;
    json_response(json!(rel.iter().map(row_json).collect::<Vec<_>>()))
}

fn api_search(conn: &Connection, query: &str) -> Result<Boxed> {
    let q = param(query, "q").unwrap_or_default();
    let qt = q.trim();
    let session_terms_raw = param(query, "session_terms");
    let session_terms: Vec<&str> = session_terms_raw
        .as_deref()
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|w| !w.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if qt.is_empty() && session_terms.is_empty() {
        return json_response(json!([]));
    }

    let tool = param(query, "tool");
    let roles = match param(query, "roles") {
        Some(value) => {
            let parsed = value
                .split(',')
                .map(str::parse::<Role>)
                .collect::<std::result::Result<Vec<_>, _>>();
            match parsed {
                Ok(roles) if !roles.is_empty() => Some(roles),
                _ => {
                    return json_response_with_status(
                        json!({"error": "roles must be a comma-separated list of user, assistant, or tool"}),
                        400,
                    )
                }
            }
        }
        None => None,
    };
    let limit = param(query, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);

    // Date and tag limits apply in SQL before `limit`, in both modes.
    let since = param(query, "since");
    let until = param(query, "until");
    let tag = param(query, "tag");
    let scope = index::SearchScope {
        roles: roles.as_deref(),
        tag: tag.as_deref(),
        since: since.as_deref(),
        until: until.as_deref(),
    };

    // If session_terms are provided via '+' chips, run cross-message session intersection
    if !session_terms.is_empty() {
        let mut all_terms = session_terms;
        if !qt.is_empty() {
            all_terms.push(qt);
        }
        let rows =
            index::search_sessions_multi_terms(conn, &all_terms, limit, tool.as_deref(), scope)?;
        return json_response(json!(rows
            .iter()
            .map(|r| {
                let mut v = row_json(r);
                v["snippet"] = json!("");
                v["snippet_marked"] = json!("");
                v["role"] = json!("session");
                v
            })
            .collect::<Vec<_>>()));
    }

    // Default search mode: strictly within one message (FTS5 rank & snippet)
    let project = param(query, "project");
    let hits = index::search_scoped(conn, qt, limit, tool.as_deref(), project.as_deref(), scope)?;
    json_response(json!(hits
        .iter()
        .map(|h| {
            let mut v = row_json(&h.row);
            let (plain, marked) = crate::commands::clean_snippet(&h.snippet);
            v["snippet"] = json!(plain);
            v["snippet_marked"] = json!(marked);
            v["role"] = json!(h.role);
            v
        })
        .collect::<Vec<_>>()))
}

/// Reverse provenance lookup: sessions that touched a file. Each result
/// carries the matched stored path, so the UI can show what it resolved to.
fn api_trace(conn: &Connection, query: &str) -> Result<Boxed> {
    let path = param(query, "path").unwrap_or_default();
    if path.is_empty() {
        return json_response(json!([]));
    }
    let limit = param(query, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let hits = index::sessions_for_file(conn, &path, limit)?;
    json_response(json!(hits
        .iter()
        .map(|(r, matched)| {
            let mut v = row_json(r);
            v["matched"] = json!(matched);
            v
        })
        .collect::<Vec<_>>()))
}

/// A file's evidence chain for the file-history page: the sessions that edited
/// it (newest first), each session row carrying its own `edits` (kind + snippet
/// + ts) - "why does this file look like this".
fn api_file(conn: &Connection, query: &str) -> Result<Boxed> {
    let path = param(query, "path").unwrap_or_default();
    if path.is_empty() {
        return json_response(json!({ "path": "", "sessions": [] }));
    }
    let hist = index::evidence_for(conn, &path, 100)?;
    let sessions: Vec<_> = hist
        .sessions
        .iter()
        .map(|se| {
            let mut v = row_json(&se.session);
            v["edits"] = json!(se.edits);
            v
        })
        .collect();
    json_response(json!({ "path": hist.path, "sessions": sessions }))
}

fn api_tag(conn: &Connection, query: &str) -> Result<Boxed> {
    let id = param(query, "id").context("missing session id")?;
    let matches = index::resolve(conn, &id)?;
    let row = matches.first().context("session not found")?;

    if let Some(add) = param(query, "add") {
        for t in add
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let _ = index::add_tag(conn, &row.session_id, t);
        }
    }
    if let Some(rm) = param(query, "rm") {
        for t in rm
            .split([',', ' '])
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let _ = index::remove_tag(conn, &row.session_id, t);
        }
    }

    let updated = index::resolve(conn, &row.session_id)?;
    let tags = updated
        .first()
        .and_then(|r| r.tags.as_ref())
        .map(|t| t.split(',').map(String::from).collect::<Vec<_>>())
        .unwrap_or_default();

    let all_tags: Vec<serde_json::Value> = index::tag_counts(conn)
        .unwrap_or_default()
        .into_iter()
        .map(|(t, c)| json!({ "tag": t, "count": c }))
        .collect();

    json_response(
        json!({ "ok": true, "session_id": row.session_id, "tags": tags, "all_tags": all_tags }),
    )
}

fn api_sync(conn: &mut Connection, sync_lock: &std::sync::Mutex<()>) -> Result<Boxed> {
    // Auto-sync, the Sync button and other windows can overlap. A request
    // that finds a sync running waits for it and reports its result rather
    // than walking every session store again right after it.
    use std::sync::TryLockError;
    let _guard = match sync_lock.try_lock() {
        Ok(guard) => {
            index::sync(conn, None)?;
            guard
        }
        Err(TryLockError::Poisoned(e)) => {
            index::sync(conn, None)?;
            e.into_inner()
        }
        Err(TryLockError::WouldBlock) => sync_lock.lock().unwrap_or_else(|e| e.into_inner()),
    };
    let count: i64 = conn
        .query_row("SELECT count(*) FROM files WHERE kind = 'main'", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    json_response(json!({ "ok": true, "sessions": count }))
}

/// A parsed session kept between page requests: everything but the messages
/// already as JSON, and the messages to slice pages from.
struct CachedSession {
    key: String,
    head: serde_json::Value,
    messages: std::sync::Arc<Vec<crate::model::Message>>,
}

/// The last session opened. Paging through a transcript asks for it again and
/// again; re-parsing the source file for every page would cost more than the
/// paging saves. One entry is enough - the UI shows one session at a time.
static SESSION_CACHE: std::sync::Mutex<Option<CachedSession>> = std::sync::Mutex::new(None);

/// What identifies one parse of a session: its row, plus the source file's
/// size and mtime so a session still being written is re-read.
fn session_cache_key(row: &index::SessionRow) -> String {
    let meta = std::fs::metadata(&row.path).ok();
    let len = meta.as_ref().map(|m| m.len());
    let mtime = meta.and_then(|m| m.modified().ok());
    format!(
        "{}|{}|{len:?}|{mtime:?}|{}",
        row.session_id, row.path, row.archived
    )
}

fn load_session_cached(
    conn: &Connection,
    row: &index::SessionRow,
) -> Result<(
    serde_json::Value,
    std::sync::Arc<Vec<crate::model::Message>>,
)> {
    let key = session_cache_key(row);
    {
        let cache = SESSION_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cache.as_ref().filter(|c| c.key == key) {
            return Ok((c.head.clone(), c.messages.clone()));
        }
    }
    let mut session = commands::load_session(conn, row)?;
    let messages = std::sync::Arc::new(std::mem::take(&mut session.messages));
    let mut head = serde_json::to_value(&session)?;
    if let Some(obj) = head.as_object_mut() {
        obj.remove("messages");
    }
    *SESSION_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedSession {
        key,
        head: head.clone(),
        messages: messages.clone(),
    });
    Ok((head, messages))
}

/// The user turns a reader navigates by, for the outline of a paged session.
fn session_outline(messages: &[crate::model::Message]) -> serde_json::Value {
    let turns: Vec<serde_json::Value> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            let t = m.text.trim_start();
            m.role == Role::User && !t.starts_with('<') && !t.starts_with("[Request interrupted")
        })
        .map(|(i, m)| {
            let text: String = m.text.split_whitespace().collect::<Vec<_>>().join(" ");
            json!({ "i": i, "text": text.chars().take(110).collect::<String>() })
        })
        .collect();
    json!(turns)
}

/// `/api/session/<id>`: the whole session, or with `offset`/`limit` one page
/// of its messages. Every response carries `total_messages`; the first page
/// also carries the outline and per-role counts, so the UI can show them
/// before the rest of a long transcript has arrived.
fn api_session(conn: &Connection, id: &str, query: &str) -> Result<Boxed> {
    let matches = index::resolve(conn, id)?;
    let row = matches.first().context("session not found")?;
    let path = std::path::Path::new(&row.path);
    let (mut v, messages) = load_session_cached(conn, row)?;
    let total = messages.len();
    let offset = param(query, "offset")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(total);
    let end = param(query, "limit")
        .and_then(|s| s.parse::<usize>().ok())
        .map_or(total, |limit| offset.saturating_add(limit).min(total));
    v["messages"] = serde_json::to_value(&messages[offset..end])?;
    v["offset"] = json!(offset);
    v["total_messages"] = json!(total);
    if offset > 0 {
        // A follow-up page: the UI already has everything else.
        return json_response(v);
    }
    let mut counts = std::collections::HashMap::new();
    for m in messages.iter() {
        // Rewound turns are hidden by default, so they are counted apart.
        let key = if m.is_rewound {
            format!("{}_rewound", m.role.label())
        } else {
            m.role.label().to_string()
        };
        *counts.entry(key).or_insert(0usize) += 1;
    }
    v["role_counts"] = json!(counts);
    v["outline"] = session_outline(&messages);
    let session_parent_id = v
        .get("parent_id")
        .and_then(|p| p.as_str())
        .map(str::to_owned);
    if row.archived {
        v["archived"] = json!(true);
    }
    if let Some(info) = resume::for_session(&row.tool, path, &row.project) {
        v["resume"] = json!(info.command_line());
    }
    if let Some(s) = &row.summary {
        v["summary"] = json!(s);
    }
    if let Some(t) = &row.tags {
        v["tags"] = json!(t.split(',').collect::<Vec<_>>());
    }
    if let Some(note) = index::note_for(conn, &row.session_id)? {
        v["note"] = json!(note);
    }
    if index::is_favorite(conn, &row.session_id)? {
        v["favorite"] = json!(true);
    }
    let pinned = index::snippet_indices_for_session(conn, &row.session_id).unwrap_or_default();
    v["pinned_indices"] = json!(pinned);
    if let Some(pid) = row.parent_id.as_ref().or(session_parent_id.as_ref()) {
        v["parent_id"] = json!(pid);
        if let Ok(parents) = index::resolve(conn, pid) {
            if let Some(prow) = parents.first() {
                v["parent"] = row_json(prow);
            }
        }
    }
    let subagents = index::subagents_for_parent(conn, &row.session_id).unwrap_or_default();
    if !subagents.is_empty() {
        v["subagents"] = json!(subagents.iter().map(row_json).collect::<Vec<_>>());
    }
    let clean_proj = row.project.trim().trim_matches(['"', '\'']).trim();
    let (git_remote, git_branch) = detect_git_remote_and_branch(Path::new(clean_proj));
    if let Some(r) = git_remote {
        v["git_remote"] = json!(r);
    }
    if let Some(b) = git_branch {
        v["git_branch"] = json!(b);
    }
    v["project_root"] = json!(clean_proj);
    let commits = index::get_session_commits(conn, &row.session_id).unwrap_or_default();
    if !commits.is_empty() {
        v["commits"] = json!(commits
            .into_iter()
            .map(|(sha, msg, ts)| json!({
                "sha": sha,
                "message": msg,
                "timestamp": ts,
            }))
            .collect::<Vec<_>>());
    }
    json_response(v)
}

fn api_commit(conn: &Connection, sha: &str) -> Result<Boxed> {
    let clean = sha.trim();
    if clean.is_empty() {
        return Ok(
            Response::from_string(json!({ "error": "missing commit sha" }).to_string())
                .with_status_code(400)
                .boxed(),
        );
    }
    let rows = index::find_session_by_commit(conn, clean)?;
    if let Some(row) = rows.first() {
        let mut val = row_json(row);
        let commits = index::get_session_commits(conn, &row.session_id).unwrap_or_default();
        val["commits"] = json!(commits
            .into_iter()
            .map(|(s, m, t)| json!({
                "sha": s, "message": m, "timestamp": t
            }))
            .collect::<Vec<_>>());
        // Sessions that merely quote a SHA are not its author; when more than
        // one still claims it, the UI is told so instead of one being picked.
        if rows.len() > 1 {
            val["candidates"] = json!(rows.iter().map(row_json).collect::<Vec<_>>());
        }
        json_response(val)
    } else {
        Ok(
            Response::from_string(json!({ "error": "not found", "commit": clean }).to_string())
                .with_status_code(404)
                .boxed(),
        )
    }
}

/// Detect git remote web URL (GitHub, GitLab, Bitbucket) and branch from a repository directory
pub fn detect_git_remote_and_branch(project_path: &Path) -> (Option<String>, Option<String>) {
    let raw_str = project_path.to_string_lossy();
    let clean_str = raw_str.trim().trim_matches(['"', '\'']).trim();
    if clean_str.is_empty() {
        return (None, None);
    }
    let clean_path = Path::new(clean_str);
    let mut current = if clean_path.is_file() {
        clean_path.parent().unwrap_or(clean_path).to_path_buf()
    } else {
        clean_path.to_path_buf()
    };
    if !current.exists() {
        return (None, None);
    }
    let mut git_dir = None;
    for _ in 0..10 {
        let candidate = current.join(".git");
        if candidate.is_dir() {
            git_dir = Some(candidate);
            break;
        } else if candidate.is_file() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                    let pointer = rest.trim();
                    let resolved = current.join(pointer);
                    if resolved.exists() {
                        git_dir = Some(resolved);
                        break;
                    }
                }
            }
        }
        if let Some(parent) = current.parent() {
            if parent == current || parent.as_os_str().is_empty() {
                break;
            }
            current = parent.to_path_buf();
        } else {
            break;
        }
    }

    let git_dir = match git_dir {
        Some(d) => d,
        None => return (None, None),
    };

    let mut branch = None;
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD")) {
        let head = head.trim();
        if let Some(rest) = head.strip_prefix("ref: refs/heads/") {
            branch = Some(rest.trim().to_string());
        }
    }

    let mut config_dir = git_dir.clone();
    if let Ok(commondir) = std::fs::read_to_string(git_dir.join("commondir")) {
        let c = commondir.trim();
        if !c.is_empty() {
            let resolved = git_dir.join(c);
            if resolved.exists() {
                config_dir = resolved;
            }
        }
    }

    let mut remote_url = None;
    if let Ok(config) = std::fs::read_to_string(config_dir.join("config")) {
        let mut in_origin = false;
        for line in config.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('[') {
                in_origin = trimmed.eq_ignore_ascii_case("[remote \"origin\"]");
                continue;
            }
            if in_origin && trimmed.starts_with("url") {
                if let Some((_, u)) = trimmed.split_once('=') {
                    remote_url = Some(u.trim().to_string());
                    break;
                }
            }
        }
    }

    let web_url = remote_url.and_then(|u| normalize_git_web_url(&u));
    (web_url, branch)
}

fn normalize_git_web_url(url: &str) -> Option<String> {
    let clean = url.trim().trim_end_matches(".git");
    if let Some(rest) = clean.strip_prefix("git@") {
        if let Some((host, repo)) = rest.split_once(':') {
            return Some(format!("https://{host}/{repo}"));
        }
    }
    if let Some(rest) = clean.strip_prefix("ssh://git@") {
        if let Some((host, repo)) = rest.split_once('/') {
            return Some(format!("https://{host}/{repo}"));
        }
    }
    if clean.starts_with("https://") || clean.starts_with("http://") {
        if let Some(pos) = clean.find("://") {
            let proto = &clean[..pos];
            let rest = &clean[pos + 3..];
            if let Some(at) = rest.find('@') {
                return Some(format!("{proto}://{}", &rest[at + 1..]));
            }
        }
        return Some(clean.to_string());
    }
    None
}

fn api_favorites(conn: &Connection) -> Result<Boxed> {
    let favs = index::list_favorites(conn)?;
    json_response(json!(favs))
}

fn api_favorite(conn: &Connection, query: &str) -> Result<Boxed> {
    let id = param(query, "id").unwrap_or_default();
    if id.is_empty() {
        return json_response(json!({ "ok": false, "error": "missing id" }));
    }
    let fav = param(query, "fav")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(true);
    index::set_favorite(conn, &id, fav)?;
    json_response(json!({ "ok": true, "id": id, "favorite": fav }))
}

fn api_forget(conn: &mut Connection, req: &mut tiny_http::Request, query: &str) -> Result<Boxed> {
    // The UI posts JSON; the body is read even when the id comes from the query,
    // so a keep-alive socket is left with nothing unconsumed.
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let id = json_body
        .as_ref()
        .and_then(|v| v.get("id").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "id"))
        .unwrap_or_default();
    if id.is_empty() {
        return json_response(json!({ "ok": false, "error": "missing id" }));
    }
    index::forget(conn, &id)?;
    json_response(json!({ "ok": true, "id": id }))
}

fn api_brief(conn: &Connection, query: &str) -> Result<Boxed> {
    let id = param(query, "id").unwrap_or_default();
    if id.is_empty() {
        return json_response(json!({ "ok": false, "error": "missing id" }));
    }
    let max_chars: usize = param(query, "max_chars")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24000);
    let include_tools = param(query, "tools")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let matches = index::resolve(conn, &id)?;
    let row = matches.first().context("session not found")?;
    let mut session = commands::load_session(conn, row)?;
    commands::redact_session_for_export(&mut session);
    let markdown = crate::brief::smart_brief(&session, max_chars, include_tools, false);
    let chars = markdown.len();
    let est_tokens = chars / 4;
    json_response(json!({
        "ok": true,
        "id": id,
        "markdown": markdown,
        "chars": chars,
        "est_tokens": est_tokens
    }))
}

fn api_subagents(conn: &Connection, query: &str) -> Result<Boxed> {
    let parent_id = param(query, "parent_id").unwrap_or_default();
    if parent_id.is_empty() {
        return json_response(json!([]));
    }
    let rows = index::subagents_for_parent(conn, &parent_id)?;
    json_response(json!(rows.iter().map(row_json).collect::<Vec<_>>()))
}

/// Raw material for the Activity tab: one compact row per main session
/// (subagents would count the same piece of work several times) with Unix
/// start/end, tool and project as indexes into the two name lists, message
/// and edit counts. The browser buckets these into its own local days and
/// hours and any period it is asked for, which the server - not knowing the
/// viewer's time zone - could not do correctly.
fn api_analytics(conn: &Connection) -> Result<Boxed> {
    let mut tools: Vec<String> = Vec::new();
    let mut projects: Vec<String> = Vec::new();
    let mut tool_idx = std::collections::HashMap::<String, usize>::new();
    let mut project_idx = std::collections::HashMap::<String, usize>::new();
    let mut stmt = conn.prepare(
        "SELECT CAST(strftime('%s', COALESCE(f.started, datetime(f.mtime, 'unixepoch'))) AS INTEGER),
                CAST(strftime('%s', COALESCE(f.ended, f.started, datetime(f.mtime, 'unixepoch'))) AS INTEGER),
                f.tool, f.project, f.msg_count, COALESCE(e.n, 0)
         FROM files f
         LEFT JOIN (SELECT session_id, COUNT(*) AS n FROM edits GROUP BY session_id) e
           ON e.session_id = f.session_id
         WHERE f.kind = 'main'
         ORDER BY 1",
    )?;
    let mut sessions = Vec::new();
    let mut rows = stmt.query([])?;
    while let Some(r) = rows.next()? {
        let Some(start) = r.get::<_, Option<i64>>(0)? else {
            continue;
        };
        let end = r.get::<_, Option<i64>>(1)?.unwrap_or(start).max(start);
        let tool: String = r.get(2)?;
        let project: String = r.get(3)?;
        let t = *tool_idx.entry(tool.clone()).or_insert_with(|| {
            tools.push(tool);
            tools.len() - 1
        });
        let p = *project_idx.entry(project.clone()).or_insert_with(|| {
            projects.push(project);
            projects.len() - 1
        });
        sessions.push(json!([
            start,
            end,
            t,
            p,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?
        ]));
    }

    let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
    json_response(json!({
        "tools": tools,
        "projects": projects,
        "sessions": sessions,
        "totals": {
            "touched_files": count("SELECT COUNT(DISTINCT path) FROM touched"),
            "snippets": count("SELECT COUNT(*) FROM snippets"),
            "favorites": count("SELECT COUNT(*) FROM favorites"),
        },
        "storage": index::db_path()
            .ok()
            .and_then(|path| index::storage_status(conn, &path).ok()),
    }))
}

/// Files touched by the main sessions active in `since`..`until` (RFC 3339
/// instants from the browser; both optional), most sessions first.
fn api_analytics_files(conn: &Connection, query: &str) -> Result<Boxed> {
    let since = param(query, "since");
    let until = param(query, "until");
    let limit: i64 = param(query, "limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(12)
        .clamp(1, 100);
    let mut stmt = conn.prepare(
        "WITH s AS (
           SELECT session_id FROM files
           WHERE kind = 'main'
             AND (?1 IS NULL OR julianday(COALESCE(ended, started)) >= julianday(?1))
             AND (?2 IS NULL OR julianday(started) <= julianday(?2))
         )
         SELECT t.path, COUNT(DISTINCT t.session_id),
                (SELECT COUNT(*) FROM edits e WHERE e.path = t.path
                   AND e.session_id IN (SELECT session_id FROM s))
         FROM touched t
         WHERE t.session_id IN (SELECT session_id FROM s)
         GROUP BY t.path
         ORDER BY 2 DESC, 3 DESC
         LIMIT ?3",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map(rusqlite::params![since, until, limit], |r| {
            Ok(json!({
                "path": r.get::<_, String>(0)?,
                "sessions": r.get::<_, i64>(1)?,
                "edits": r.get::<_, i64>(2)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    json_response(json!(rows))
}

fn api_snippets(conn: &Connection, query: &str) -> Result<Boxed> {
    let q = param(query, "q");
    let lang = param(query, "lang");
    let session_id = param(query, "session_id");
    let tool = param(query, "tool");
    let role = param(query, "role");
    let limit = param(query, "limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let snippets = index::list_snippets(
        conn,
        q.as_deref(),
        lang.as_deref(),
        session_id.as_deref(),
        tool.as_deref(),
        role.as_deref(),
        limit,
    )?;
    json_response(json!(snippets))
}

fn api_snippet_delete(
    conn: &Connection,
    req: &mut tiny_http::Request,
    query: &str,
) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let id = json_body
        .as_ref()
        .and_then(|v| {
            v.get("id").and_then(|n| {
                n.as_i64()
                    .or_else(|| n.as_str().and_then(|s| s.parse().ok()))
            })
        })
        .or_else(|| param(query, "id").and_then(|s| s.parse().ok()))
        .context("missing id")?;
    let ok = index::delete_snippet(conn, id)?;
    json_response(json!({ "ok": ok, "id": id }))
}

fn api_snippet_rename(
    conn: &Connection,
    req: &mut tiny_http::Request,
    query: &str,
) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();

    let id: i64 = json_body
        .as_ref()
        .and_then(|v| v.get("id").and_then(|n| n.as_i64()))
        .or_else(|| param(query, "id").and_then(|s| s.parse().ok()))
        .context("missing id")?;

    let title = json_body
        .as_ref()
        .and_then(|v| v.get("title").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "title"))
        .context("missing title")?;

    let ok = index::rename_snippet(conn, id, &title)?;
    json_response(json!({ "ok": ok, "id": id, "title": title }))
}

fn api_snippet_save(conn: &Connection, req: &mut tiny_http::Request, query: &str) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();

    let session_id = json_body
        .as_ref()
        .and_then(|v| {
            v.get("session_id")
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .or_else(|| param(query, "session_id"))
        .context("missing session_id")?;

    let message_idx: usize = json_body
        .as_ref()
        .and_then(|v| {
            v.get("message_idx")
                .and_then(|n| n.as_u64())
                .map(|n| n as usize)
        })
        .or_else(|| param(query, "message_idx").and_then(|s| s.parse().ok()))
        .unwrap_or(0);

    let title = json_body
        .as_ref()
        .and_then(|v| v.get("title").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "title"))
        .unwrap_or_else(|| "Saved snippet".to_string());

    let lang = json_body
        .as_ref()
        .and_then(|v| v.get("lang").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "lang"));

    let content = json_body
        .as_ref()
        .and_then(|v| v.get("content").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "content"))
        .context("missing content")?;

    let tags = json_body
        .as_ref()
        .and_then(|v| v.get("tags").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "tags"));

    let tool = json_body
        .as_ref()
        .and_then(|v| v.get("tool").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "tool"));

    let role = json_body
        .as_ref()
        .and_then(|v| v.get("role").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "role"));

    let message_ts = json_body
        .as_ref()
        .and_then(|v| {
            v.get("message_ts")
                .and_then(|s| s.as_str())
                .map(String::from)
        })
        .or_else(|| param(query, "message_ts"));

    let id = index::save_snippet(
        conn,
        &index::NewSnippet {
            session_id: &session_id,
            message_idx,
            title: &title,
            lang: lang.as_deref(),
            content: &content,
            tags: tags.as_deref(),
            tool: tool.as_deref(),
            role: role.as_deref(),
            message_ts: message_ts.as_deref(),
        },
    )?;

    json_response(json!({ "ok": true, "id": id }))
}

/// Delegate to SessionRow's Serialize derive so the web JSON and the CLI
/// `--json` contract are the exact same field set (id/msgs/tags-array/...),
/// and can never drift apart.
fn row_json(r: &index::SessionRow) -> serde_json::Value {
    serde_json::to_value(r).unwrap_or_else(|_| json!({}))
}

fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key && !v.is_empty()).then(|| url_decode(v))
    })
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => match (hex(bytes.get(i + 1)), hex(bytes.get(i + 2))) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: Option<&u8>) -> Option<u8> {
    (*b? as char).to_digit(16).map(|d| d as u8)
}

/// True when running under WSL, where `xdg-open` pops a WSLg/Windows browser
/// window on EVERY `web` start (a stray blank window during repeated testing)
/// and often can't even reach the loopback server. Detected via the kernel
/// string, which carries "microsoft" on WSL1/WSL2.
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| v.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// Whether to auto-open the browser: never when `--no-open`, and never on WSL
/// (print the URL there instead - see `is_wsl`).
fn should_open_browser(no_open: bool, wsl: bool) -> bool {
    !no_open && !wsl
}

#[cfg(target_os = "windows")]
pub(crate) fn open_browser(url: &str) {
    use std::os::windows::ffi::OsStrExt;
    let url_wide: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let op_wide: Vec<u16> = std::ffi::OsStr::new("open")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: *mut std::ffi::c_void,
            lpOperation: *const u16,
            lpFile: *const u16,
            lpParameters: *const u16,
            lpDirectory: *const u16,
            nShowCmd: i32,
        ) -> isize;
    }

    unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            op_wide.as_ptr(),
            url_wide.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "macos")]
    cmd.arg(url);

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    cmd.arg(url);

    let _ = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

const INDEX_HTML: &str = include_str!("webui.html");

/// Languages of the files touched by the main sessions active in
/// `since`..`until`, by extension: distinct files and sessions per language.
fn api_analytics_langs(conn: &Connection, query: &str) -> Result<Boxed> {
    let since = param(query, "since");
    let until = param(query, "until");
    let mut stmt = conn.prepare(
        "SELECT t.path, t.session_id FROM touched t
         WHERE t.session_id IN (
           SELECT session_id FROM files
           WHERE kind = 'main'
             AND (?1 IS NULL OR julianday(COALESCE(ended, started)) >= julianday(?1))
             AND (?2 IS NULL OR julianday(started) <= julianday(?2))
         )",
    )?;
    let mut langs: std::collections::BTreeMap<
        &'static str,
        (
            std::collections::HashSet<String>,
            std::collections::HashSet<String>,
        ),
    > = Default::default();
    let mut rows = stmt.query(rusqlite::params![since, until])?;
    while let Some(r) = rows.next()? {
        let path: String = r.get(0)?;
        let session: String = r.get(1)?;
        let entry = langs.entry(language_of(&path)).or_default();
        entry.0.insert(path);
        entry.1.insert(session);
    }
    let mut out: Vec<_> = langs
        .into_iter()
        .map(|(lang, (files, sessions))| (lang, files.len(), sessions.len()))
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    json_response(json!(out
        .iter()
        .map(
            |(lang, files, sessions)| json!({ "lang": lang, "files": files, "sessions": sessions })
        )
        .collect::<Vec<_>>()))
}

/// A display language for a path, by extension (or well-known file name).
fn language_of(path: &str) -> &'static str {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .to_ascii_lowercase();
    match name.as_str() {
        "dockerfile" => return "Docker",
        "makefile" => return "Make",
        "cargo.toml" | "cargo.lock" => return "Rust",
        _ => {}
    }
    let ext = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => ext,
        _ => return "Other",
    };
    match ext {
        "rs" => "Rust",
        "ts" | "tsx" | "mts" | "cts" => "TypeScript",
        "js" | "jsx" | "mjs" | "cjs" => "JavaScript",
        "py" | "pyi" | "ipynb" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "swift" => "Swift",
        "c" | "h" => "C",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "C++",
        "cs" => "C#",
        "rb" => "Ruby",
        "php" => "PHP",
        "lua" => "Lua",
        "dart" => "Dart",
        "sol" => "Solidity",
        "sql" => "SQL",
        "sh" | "bash" | "zsh" => "Shell",
        "ps1" | "psm1" | "bat" | "cmd" => "PowerShell/Batch",
        "html" | "htm" => "HTML",
        "css" | "scss" | "sass" | "less" => "CSS",
        "vue" => "Vue",
        "svelte" => "Svelte",
        "md" | "mdx" | "markdown" => "Markdown",
        "json" | "jsonc" | "json5" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "xml" => "XML",
        "txt" | "log" => "Text",
        _ => "Other",
    }
}

fn api_backup_status(conn: &Connection) -> Result<Boxed> {
    let cfg = crate::backup::get_backup_config(conn)?;
    let total: i64 = conn
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .unwrap_or(0);
    let progress = crate::backup::get_progress();
    json_response(json!({
        "ok": true,
        "repo_url": cfg.repo_url,
        "last_backup_at": cfg.last_backup_at,
        "last_commit": cfg.last_commit,
        "last_status": cfg.last_status,
        "total_sessions": total,
        "progress": progress,
    }))
}

fn api_backup_config(
    conn: &Connection,
    req: &mut tiny_http::Request,
    query: &str,
) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let repo_url = json_body
        .as_ref()
        .and_then(|v| v.get("repo_url").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "repo_url"))
        .context("missing repo_url")?;

    crate::backup::set_backup_config(conn, &repo_url)?;
    json_response(json!({ "ok": true, "repo_url": repo_url }))
}

fn api_backup_test(req: &mut tiny_http::Request, query: &str) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let repo_url = json_body
        .as_ref()
        .and_then(|v| v.get("repo_url").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "repo_url"))
        .unwrap_or_default();

    let res = crate::backup::test_connection(&repo_url);
    json_response(json!({
        "ok": res.ok,
        "message": res.message,
        "git_version": res.git_version,
        "git_user": res.git_user,
    }))
}

fn api_backup_run(conn: &Connection, req: &mut tiny_http::Request, query: &str) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let explicit_url = json_body
        .as_ref()
        .and_then(|v| v.get("repo_url").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "repo_url"));

    let repo_url = match explicit_url {
        Some(u) if !u.trim().is_empty() => u,
        _ => {
            let cfg = crate::backup::get_backup_config(conn)?;
            if cfg.repo_url.trim().is_empty() {
                anyhow::bail!("Репозиторий не настроен. Укажите URL GitHub репозитория.");
            }
            cfg.repo_url
        }
    };

    // Claim the slot here, not in the worker: the progress flag is only set
    // inside the thread, so an is_running() check let two clicks both start.
    if !crate::backup::try_begin() {
        return json_response(json!({
            "ok": true,
            "started": true,
            "already_running": true,
            "progress": crate::backup::get_progress(),
        }));
    }
    let guard = crate::backup::RunningGuard;

    let repo_url_clone = repo_url.clone();
    std::thread::spawn(move || {
        // Releasing happens on any exit path, including an early return below.
        let _guard = guard;
        let db_p = match crate::index::db_path() {
            Ok(p) => p,
            Err(e) => {
                crate::backup::set_error(&format!("Database path error: {e}"));
                return;
            }
        };
        let conn = match rusqlite::Connection::open(&db_p) {
            Ok(c) => c,
            Err(e) => {
                crate::backup::set_error(&format!("Database open error: {e}"));
                return;
            }
        };
        let _ = crate::backup::run_backup(&conn, &repo_url_clone);
    });

    json_response(json!({
        "ok": true,
        "started": true,
        "progress": crate::backup::get_progress(),
    }))
}

fn api_backup_restore(
    conn: &Connection,
    req: &mut tiny_http::Request,
    query: &str,
) -> Result<Boxed> {
    let mut body = String::new();
    let _ = req.as_reader().read_to_string(&mut body);
    let json_body: Option<serde_json::Value> = serde_json::from_str(&body).ok();
    let explicit_url = json_body
        .as_ref()
        .and_then(|v| v.get("repo_url").and_then(|s| s.as_str()).map(String::from))
        .or_else(|| param(query, "repo_url"));

    let repo_url = match explicit_url {
        Some(u) if !u.trim().is_empty() => u,
        _ => {
            let cfg = crate::backup::get_backup_config(conn)?;
            if cfg.repo_url.trim().is_empty() {
                anyhow::bail!("Репозиторий не настроен. Укажите URL GitHub репозитория.");
            }
            cfg.repo_url
        }
    };

    // Claim the slot before spawning: the twin of the run-handler bug, where
    // the flag was only set inside the thread and both clicks got through.
    if !crate::backup::try_begin() {
        return json_response(json!({
            "ok": true,
            "started": true,
            "already_running": true,
            "progress": crate::backup::get_progress(),
        }));
    }

    let guard = crate::backup::RunningGuard;
    let repo_url_clone = repo_url.clone();
    std::thread::spawn(move || {
        // Dropped on every exit path, including the two early returns below.
        let _guard = guard;
        let db_p = match crate::index::db_path() {
            Ok(p) => p,
            Err(e) => {
                crate::backup::set_error(&format!("Database path error: {e}"));
                return;
            }
        };
        let mut conn = match rusqlite::Connection::open(&db_p) {
            Ok(c) => c,
            Err(e) => {
                crate::backup::set_error(&format!("Database open error: {e}"));
                return;
            }
        };
        let _ = crate::backup::run_restore(&mut conn, &repo_url_clone);
    });

    json_response(json!({
        "ok": true,
        "started": true,
        "progress": crate::backup::get_progress(),
    }))
}

#[cfg(test)]
mod tests {
    use super::{host_matches, origin_ok, should_open_browser};

    #[test]
    fn wsl_never_auto_opens_a_browser() {
        // Native (not WSL): open unless the user said --no-open.
        assert!(should_open_browser(false, false));
        assert!(!should_open_browser(true, false));
        // WSL: never auto-open, so `web` stops popping a stray Chrome/WSLg
        // window on every start - the URL is printed instead.
        assert!(!should_open_browser(false, true));
        assert!(!should_open_browser(true, true));
    }

    #[test]
    fn host_matches_only_loopback() {
        assert!(host_matches(Some("127.0.0.1:7575"), 7575));
        assert!(host_matches(Some("localhost:7575"), 7575));
        assert!(host_matches(Some("[::1]:7575"), 7575));
        // DNS-rebinding and cross-origin attempts:
        assert!(!host_matches(Some("evil.com:7575"), 7575));
        assert!(!host_matches(Some("127.0.0.1:7575"), 7576)); // wrong port
        assert!(!host_matches(Some("127.0.0.1"), 7575)); // no port
        assert!(!host_matches(None, 7575)); // missing Host
    }

    #[test]
    fn origin_ok_rejects_cross_origin() {
        assert!(origin_ok(None, 7575)); // same-origin GET / navigation
        assert!(origin_ok(Some("http://127.0.0.1:7575"), 7575));
        assert!(origin_ok(Some("http://localhost:7575"), 7575));
        assert!(!origin_ok(Some("http://evil.com"), 7575)); // cross-origin fetch
        assert!(!origin_ok(Some("https://127.0.0.1:7575"), 7575)); // wrong scheme
        assert!(!origin_ok(Some("http://127.0.0.1:7576"), 7575)); // wrong port
    }

    #[test]
    fn test_normalize_git_web_urls() {
        use super::normalize_git_web_url;
        assert_eq!(
            normalize_git_web_url("git@github.com:AlexZander85/wikiagents.git"),
            Some("https://github.com/AlexZander85/wikiagents".into())
        );
        assert_eq!(
            normalize_git_web_url("https://github.com/AlexZander85/wikiagents.git"),
            Some("https://github.com/AlexZander85/wikiagents".into())
        );
        assert_eq!(
            normalize_git_web_url("https://token@gitlab.com/org/project.git"),
            Some("https://gitlab.com/org/project".into())
        );
        assert_eq!(
            normalize_git_web_url("ssh://git@github.com/org/repo.git"),
            Some("https://github.com/org/repo".into())
        );
    }

    #[test]
    fn test_detect_git_remote_and_branch_self() {
        use super::detect_git_remote_and_branch;
        use std::path::Path;
        let (remote, branch) = detect_git_remote_and_branch(Path::new("."));
        assert!(remote.is_some(), "detects git remote for current repo");
        let r = remote.unwrap();
        assert!(
            r.contains("wikiagents"),
            "remote URL contains wikiagents: {}",
            r
        );
        assert!(branch.is_some(), "detects git branch");
    }
}
