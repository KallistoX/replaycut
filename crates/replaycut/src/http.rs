//! The HTTP API as specified in `docs/api.md`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::admin;
use crate::auth;
use crate::platform;
use crate::share::{self, ShareError, ShareRequest};
use crate::state::{AppState, StateError, MAX_HISTORY};

type App = Arc<AppState>;

pub fn router(state: App) -> Router {
    Router::new()
        .route("/", get(entry))
        .route("/index.html", get(entry))
        // pages since 2.1: the same file, the JS picks the page by path
        .route("/setup", get(ui))
        .route("/settings", get(ui))
        .route("/diagnostics", get(ui))
        .route("/login", get(ui))
        .route("/obs", get(ui))
        // since 3.0: the queue and everything that was ever sent somewhere
        .route("/activity", get(ui))
        // since 2.8: the device login
        .route("/approve", get(ui))
        .route("/approve/{id}", get(ui))
        .route("/api/clips", get(clips))
        .route("/api/events", get(events))
        .route("/api/clips/{base}", axum::routing::delete(delete_clip))
        .route(
            "/api/clips/{base}/name",
            axum::routing::put(set_name).post(set_name),
        )
        // since 3.0
        .route(
            "/api/clips/{base}/state",
            axum::routing::put(set_clip_state).post(set_clip_state),
        )
        .route("/api/history", get(history))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/open-folder", post(job_open_folder))
        .route("/api/tls/show", post(tls_show))
        .route("/api/jobs/{id}/copy-file", post(job_copy_file))
        .route("/api/jobs/{id}/cancel", post(job_cancel))
        .route("/api/jobs/{id}/publish", post(job_publish))
        // since 2.6
        .route("/api/jobs/{id}/file", get(job_file))
        // since 2.7
        .route("/api/jobs/{id}/post", post(job_post))
        .route("/api/clips/{base}/preview", post(clip_preview))
        // since 3.0: the cut between the recording and every rendering
        .route("/api/cuts", post(cuts_create))
        .route("/api/cuts/{id}", get(cut).delete(delete_cut))
        .route("/api/cuts/{id}/render", post(cut_render))
        .route("/api/share", post(share))
        .route("/api/save", post(save))
        .route("/media/{file}", get(media))
        // since 2.1
        .route(
            "/api/settings",
            get(admin::get_settings).put(admin::put_settings),
        )
        .route("/api/test/nextcloud", post(admin::test_nextcloud))
        .route("/api/test/discord", post(admin::test_discord))
        .route("/api/test/s3", post(admin::test_s3))
        .route("/api/test/webdav", post(admin::test_webdav))
        .route("/api/test/telegram", post(admin::test_telegram))
        .route("/api/test/webhook", post(admin::test_webhook))
        .route("/api/addresses", get(admin::addresses))
        .route("/api/setup/obs", get(admin::setup_obs))
        .route("/api/diagnostics", get(admin::diagnostics))
        .route("/api/obs", get(admin::obs_status))
        .route("/api/obs/reconnect", post(admin::obs_reconnect))
        .route("/api/obs/refresh", post(admin::obs_refresh))
        .route("/api/update", get(admin::update_status))
        .route("/api/update/check", post(admin::update_check))
        .route("/api/update/download", post(admin::update_download))
        .route("/api/update/install", post(admin::update_install))
        .route("/api/update/seen", post(admin::update_seen))
        .route("/api/scanning", post(admin::scanning))
        .route("/api/oauth/{provider}", get(admin::oauth_status))
        .route("/api/oauth/{provider}/start", post(admin::oauth_start))
        .route(
            "/api/oauth/{provider}/disconnect",
            post(admin::oauth_disconnect),
        )
        // since 2.6: the loopback flow
        .route(
            "/api/oauth/{provider}/loopback",
            post(admin::oauth_loopback),
        )
        .route("/oauth/{provider}/callback", get(admin::oauth_callback))
        .route(
            "/api/obs/replay-buffer/start",
            post(admin::obs_start_replay),
        )
        .route("/api/obs/adopt-folder", post(admin::obs_adopt_folder))
        .route("/api/session", get(admin::session))
        .route("/api/login", post(admin::login))
        // since 2.8
        .route("/api/password/suggest", get(admin::password_suggest))
        .route("/api/pair/request", post(admin::pair_request))
        .route("/api/pair/pending", get(admin::pair_pending))
        .route("/api/pair/{id}", get(admin::pair_poll))
        .route("/api/pair/{id}/approve", post(admin::pair_approve))
        .route("/api/pair/{id}/deny", post(admin::pair_deny))
        .route("/api/network/enable", post(admin::network_enable))
        .route("/api/network/disable", post(admin::network_disable))
        .route("/api/sessions", get(admin::sessions))
        .route("/api/sessions/clear", post(admin::sessions_clear))
        .route(
            "/api/sessions/{id}",
            axum::routing::delete(admin::session_revoke),
        )
        .route("/api/logout", post(admin::logout))
        .route("/api/restart", post(admin::restart))
        .route("/themes/{file}", get(admin::theme))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .layer(axum::middleware::from_fn(auth::origin_check))
        // outermost since 2.8: a request for another host never reaches a handler
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::host_check,
        ))
        .with_state(state)
}

/// Error responses of the contract: `{ ok: false, error }` with a status code.
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn internal(e: impl std::fmt::Display) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

impl From<StateError> for ApiError {
    fn from(e: StateError) -> Self {
        let status = match e {
            StateError::UnknownClip(_) | StateError::UnknownJob => StatusCode::NOT_FOUND,
            StateError::TooLate(_) => StatusCode::CONFLICT,
            StateError::ClipBusy => StatusCode::CONFLICT,
            StateError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError::new(status, e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "ok": false, "error": self.message })),
        )
            .into_response()
    }
}

fn parse_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, "text/plain")],
        "not found",
    )
        .into_response()
}

/// `GET /` and `/index.html`. Since 2.8 a `?pair=<token>` from a scanned
/// QR code signs this device in and takes the token out of the address
/// bar; a token that is used up or too old lands on the login page.
async fn entry(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let Some(token) = query.get("pair") else {
        return ui(State(app)).await;
    };
    if !app.pairing.redeem_qr(token) {
        tracing::warn!("a QR sign-in from {} was too old or used up", addr.ip());
        return Ok(redirect("/login?pair=expired", &[]));
    }
    let agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Since 3.5 the device cookie travels with this navigation - it is
    // `SameSite=Lax` for exactly this case - so a phone that scanned the
    // code keeps the row it already has instead of opening a second one.
    let (device, fresh) = auth::device_of(&headers, auth::Client::Browser);
    let session = auth::NewSession::from_agent(agent, addr.ip(), auth::Via::Qr).on_device(&device);
    tracing::info!("QR sign-in from {} ({})", addr.ip(), session.name);
    let mut cookies = vec![auth::set_cookie_value(
        &app.sessions.create(session),
        app.tls.active,
    )];
    if fresh {
        cookies.push(auth::set_device_cookie_value(&device, app.tls.active));
    }
    Ok(redirect("/", &cookies))
}

fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut res = (StatusCode::SEE_OTHER, [("Location", location)]).into_response();
    for cookie in cookies {
        if let Ok(value) = HeaderValue::from_str(cookie) {
            res.headers_mut()
                .append(axum::http::header::SET_COOKIE, value);
        }
    }
    res
}

async fn ui(State(app): State<App>) -> Result<Response, ApiError> {
    let paths = app.paths();
    let bytes = tokio::fs::read(&paths.ui_file).await.map_err(|e| {
        ApiError::internal(format!(
            "UI file {} unreadable: {e}",
            paths.ui_file.display()
        ))
    })?;
    Ok((
        [
            (CONTENT_TYPE, "text/html; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        bytes,
    )
        .into_response())
}

/// The state document. Since 2.8 it carries `pending` - the devices
/// waiting for an answer - but only for this PC and signed-in clients:
/// the code in it is for the person who decides.
fn state_document(app: &AppState, addr: &SocketAddr, headers: &HeaderMap) -> Value {
    let mut doc = app.status();
    if auth::is_authenticated(app, addr, headers) {
        doc["pending"] = Value::Array(app.pairing.pending());
    }
    doc
}

async fn clips(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Json<Value> {
    // since 3.0: `?done=1` lists the clips that were marked done as well
    if query.get("done").is_some_and(|v| v == "1") {
        let mut doc = app.status_for(true);
        if auth::is_authenticated(&app, &addr, &headers) {
            doc["pending"] = Value::Array(app.pairing.pending());
        }
        return Json(doc);
    }
    Json(state_document(&app, &addr, &headers))
}

/// Counts an open event stream; the count drops with the stream.
struct SseGuard(App);

impl Drop for SseGuard {
    fn drop(&mut self) {
        self.0
            .sse_clients
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

const MAX_SSE_CLIENTS: usize = 8;

/// `GET /api/events` (since 2.4): the `/api/clips` document as an
/// `event: state` whenever something changed (bursts coalesced), a ping
/// every 25 s, closed on shutdown so a restart does not wait for it.
async fn events(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::sync::atomic::Ordering;
    if app.sse_clients.fetch_add(1, Ordering::Relaxed) >= MAX_SSE_CLIENTS {
        app.sse_clients.fetch_sub(1, Ordering::Relaxed);
        return ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many event streams open - the page falls back to polling",
        )
        .into_response();
    }
    let guard = SseGuard(app.clone());
    let rx = app.events.subscribe();
    let shutdown = app.shutdown.get().cloned();
    // who may see the sign-in requests is decided once, for this stream
    let trusted = auth::is_authenticated(&app, &addr, &headers);
    let stream = futures_util::stream::unfold(
        (rx, app, guard, shutdown, true),
        move |(mut rx, app, guard, shutdown, first)| async move {
            if !first {
                let changed = rx.changed();
                let stop = async {
                    match &shutdown {
                        Some(s) => {
                            s.wait().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    r = changed => r.ok()?,
                    _ = stop => return None,
                }
                // a burst of changes (job progress, scan) becomes one event
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                rx.borrow_and_update();
            }
            let mut doc = app.status();
            if trusted {
                doc["pending"] = Value::Array(app.pairing.pending());
            }
            let event: Result<Event, std::convert::Infallible> =
                Ok(Event::default().event("state").data(doc.to_string()));
            Some((event, (rx, app, guard, shutdown, false)))
        },
    );
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(25))
                .text("ping"),
        )
        .into_response()
}

/// `GET /api/history[?limit=&before=]`: the outputs, newest first. Since 3.0
/// the store keeps every one of them, so a page is what the client asks for.
async fn history(
    State(app): State<App>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    // `limit` is a maximum, so 0 is a page of nothing and not a page of one;
    // a number that is no number at all falls back to the default
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(MAX_HISTORY)
        .min(5000);
    let before = query
        .get("before")
        .map(String::as_str)
        .filter(|b| !b.is_empty());
    Json(app.history_page(limit, before))
}

/// `PUT /api/clips/<base>/state { state }` (since 3.0): `done` takes a clip
/// out of the list, `active` brings it back. A running job is not touched.
async fn set_clip_state(
    State(app): State<App>,
    Path(base): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_body(&body);
    let state = v["state"].as_str().unwrap_or("").trim().to_string();
    if ![crate::db::CLIP_DONE, crate::db::CLIP_ACTIVE].contains(&state.as_str()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "state must be done or active",
        ));
    }
    Ok(Json(app.set_clip_state(&base, &state)?))
}

async fn job(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    match app.job(&id) {
        Some(job) => Ok(Json(serde_json::to_value(job).unwrap_or(Value::Null))),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "unknown job")),
    }
}

/// The shared file of a finished job, for the local-mode actions (since 2.1).
fn shared_file_of(app: &AppState, id: &str) -> Result<std::path::PathBuf, ApiError> {
    // The jobs map holds this run only; the store holds every output there
    // ever was. Without the fallback every row of the clips and activity
    // pages lost both actions on the first restart, while the download beside
    // them kept working.
    let job = app
        .job(id)
        .or_else(|| app.history_job(id))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "unknown job"))?;
    let Some(file) = job.file.filter(|_| job.ok == Some(true)) else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this job has no finished file",
        ));
    };
    let path = app.paths().shared_dir.join(file);
    if !path.is_file() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "the shared file is gone",
        ));
    }
    Ok(path)
}

/// `POST /api/jobs/<id>/open-folder` (since 2.1): Explorer with the file selected.
async fn job_open_folder(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = shared_file_of(&app, &id)?;
    if app.dry_run {
        tracing::info!("dry run: would open the folder of {}", path.display());
    } else {
        tokio::task::spawn_blocking(move || platform::open_folder_select(&path))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/tls/show` (since 3.4): the certificate folder with `ca.crt`
/// selected, so the file can be dragged into a browser's certificate store.
/// Only for a certificate we made - an own PEM pair needs no importing, and
/// its files are wherever the user put them.
async fn tls_show(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let path = app.tls.ca_file.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "there is no certificate authority to show: HTTPS is off, or you brought your own certificate",
        )
    })?;
    if app.dry_run {
        tracing::info!("dry run: would open the folder of {}", path.display());
    } else {
        tokio::task::spawn_blocking(move || platform::open_folder_select(&path))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/jobs/<id>/copy-file` (since 2.1): the file as a clipboard object.
async fn job_copy_file(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let path = shared_file_of(&app, &id)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if app.dry_run {
        tracing::info!("dry run: would copy {} to the clipboard", path.display());
    } else {
        tokio::task::spawn_blocking(move || platform::copy_file(&path))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
        tracing::info!("copied {name} to the clipboard as a file");
    }
    Ok(Json(json!({ "ok": true, "file": name })))
}

async fn set_name(
    State(app): State<App>,
    Path(base): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let name = parse_body(&body)["name"].as_str().unwrap_or("").to_string();
    let title = app.set_title(&base, &name)?;
    Ok(Json(json!({ "ok": true, "base": base, "title": title })))
}

async fn delete_clip(
    State(app): State<App>,
    Path(base): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    // `nextcloud=1` is what 1.4 called it; `remote=1` is the name since 3.0
    let remote = query
        .get("nextcloud")
        .or_else(|| query.get("remote"))
        .is_some_and(|v| v == "1");
    // since 3.0: `scope=clip` recycles the recording and leaves the cuts,
    // `all` (the default, and what 1.4 did) takes everything with it
    let scope = query.get("scope").map(String::as_str).unwrap_or(SCOPE_ALL);
    if ![SCOPE_ALL, SCOPE_CLIP].contains(&scope) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "scope must be clip or all",
        ));
    }
    let clip = app.take_clip_for_delete(&base)?;
    // a clip without cuts has nothing left to keep: `clip` is then `all`
    let scope = if scope == SCOPE_CLIP && clip.cuts.is_empty() {
        SCOPE_ALL
    } else {
        scope
    };
    let keep_cuts = scope == SCOPE_CLIP;
    let remote = remote && !keep_cuts;

    // The MKV plus, unless the cuts stay, every share derived from it and
    // every cut file.
    let paths = app.paths();
    let shared = if keep_cuts {
        Vec::new()
    } else {
        shared_files_of(&paths.shared_dir, &base)
    };
    let mut files: Vec<std::path::PathBuf> = clip.path.into_iter().collect();
    files.extend(shared.iter().cloned());
    if !keep_cuts {
        files.extend(
            clip.cuts
                .iter()
                .map(|c| paths.cut_of(&c.id))
                .filter(|f| f.is_file()),
        );
    }
    let remote_deleted = if remote {
        let runtime = app.runtime();
        if runtime.integrations.storages.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "no storage integration is enabled",
            ));
        }
        // since 2.5 every configured storage gets the paths its jobs recorded;
        // Nextcloud also gets the pre-2.5 ones derived from the shared file names
        let month = share::month_of(&base);
        let mut n = 0usize;
        for entry in &runtime.integrations.storages {
            let mut paths = app.history_paths_for_target(&base, entry.id);
            if entry.id == "nextcloud" {
                paths.extend(shared.iter().filter_map(|f| {
                    f.file_name()
                        .map(|n| entry.storage.remote_path(&month, &n.to_string_lossy()))
                }));
                paths.extend(app.history_paths_for(&base));
            }
            paths.sort();
            paths.dedup();
            if !paths.is_empty() {
                n += entry
                    .storage
                    .delete(&paths)
                    .await
                    .map_err(ApiError::internal)?;
            }
        }
        app.remove_history_for(&base);
        n
    } else {
        0
    };
    let recycled = tokio::task::spawn_blocking(move || -> Result<usize, anyhow::Error> {
        let mut n = 0;
        for f in files {
            if f.exists() {
                platform::recycle(&f)?;
                n += 1;
            }
        }
        Ok(n)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    // The playable copies go with the recording either way; the thumbnail
    // stays as long as the clip is still listed for its cuts.
    let _ = std::fs::remove_file(paths.preview_of(&base));
    let _ = std::fs::remove_file(paths.preview_h264_of(&base));
    if keep_cuts {
        app.recording_recycled(&base);
    } else {
        let _ = std::fs::remove_file(paths.thumb_of(&base));
        app.forget_clip(&base);
    }
    app.scan_wake.notify_one();
    tracing::info!(
        "deleted {base} (scope {scope}): {recycled} file(s) to the recycle bin{}",
        if keep_cuts {
            format!(", {} cut(s) kept", clip.cuts.len())
        } else {
            String::new()
        }
    );
    Ok(Json(json!({
        "ok": true, "recycled": recycled, "nextcloud": remote_deleted,
        // since 3.0
        "scope": scope, "cuts": if keep_cuts { clip.cuts.len() } else { 0 },
    })))
}

pub const SCOPE_ALL: &str = "all";
pub const SCOPE_CLIP: &str = "clip";

/// The finished files of a clip in `shared\`: `<base with whitespace as _>_*.mp4`.
fn shared_files_of(shared_dir: &std::path::Path, base: &str) -> Vec<std::path::PathBuf> {
    let prefix = base.split_whitespace().collect::<Vec<_>>().join("_") + "_";
    let Ok(entries) = std::fs::read_dir(shared_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with(&prefix) && name.to_ascii_lowercase().ends_with(".mp4")
        })
        .map(|e| e.path())
        .collect()
}

/// `DELETE /api/cuts/<id>[?remote=1]` (since 3.0): one cut with the outputs
/// that came from it. A clip that loses its last cut is a fresh clip again.
async fn delete_cut(
    State(app): State<App>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let remote = query
        .get("remote")
        .or_else(|| query.get("nextcloud"))
        .is_some_and(|v| v == "1");
    let Some(cut) = app.db.cut(&id).map_err(ApiError::internal)? else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("unknown cut: {id}"),
        ));
    };
    if app.cut_busy(&id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "this cut is being worked on right now - please wait",
        ));
    }
    let outputs = app.db.jobs_of_cut(&id).map_err(ApiError::internal)?;
    let paths = app.paths();
    let mut files: Vec<std::path::PathBuf> = outputs
        .iter()
        .filter_map(|o| o["file"].as_str())
        .map(|f| paths.shared_dir.join(f))
        .filter(|f| f.is_file())
        .collect();
    files.sort();
    files.dedup();
    let cut_file = paths.cut_of(&cut.id);
    if cut_file.is_file() {
        files.push(cut_file);
    }
    let remote_deleted = if remote {
        let runtime = app.runtime();
        if runtime.integrations.storages.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "no storage integration is enabled",
            ));
        }
        let mut n = 0usize;
        for entry in &runtime.integrations.storages {
            let mut paths = app.cut_paths_for_target(&cut.id, entry.id);
            paths.sort();
            paths.dedup();
            if !paths.is_empty() {
                n += entry
                    .storage
                    .delete(&paths)
                    .await
                    .map_err(ApiError::internal)?;
            }
        }
        n
    } else {
        0
    };
    let recycled = tokio::task::spawn_blocking(move || -> Result<usize, anyhow::Error> {
        let mut n = 0;
        for f in files {
            platform::recycle(&f)?;
            n += 1;
        }
        Ok(n)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    app.remove_history_of_cut(&cut.id);
    app.db.delete_cut(&cut.id).map_err(ApiError::internal)?;
    app.reset_state_without_cuts(&cut.base);
    app.tray_changed();
    tracing::info!(
        "deleted cut {} of {}: {recycled} file(s) to the recycle bin",
        cut.id,
        cut.base
    );
    Ok(Json(
        json!({ "ok": true, "recycled": recycled, "nextcloud": remote_deleted, "base": cut.base }),
    ))
}

fn number(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0.0)
}

/// The `202` of a queued job, with what the endpoint wants to add. Since 3.0
/// every answer that has a cut names it.
fn accepted(app: &App, started: share::Started, extra: Value) -> Response {
    if started.position == 0 {
        tokio::spawn(share::run(app.clone(), started.job.clone()));
    }
    let mut doc = json!({ "ok": true, "job": started.job, "position": started.position });
    if let Some(cut) = started.cut {
        doc["cut"] = json!(cut);
    }
    if let Some(map) = extra.as_object() {
        for (k, v) in map {
            doc[k.as_str()] = v.clone();
        }
    }
    (StatusCode::ACCEPTED, Json(doc)).into_response()
}

/// A job that could not be queued, as the contract answers it. `busy` is what
/// "the same thing is already running" means for this endpoint.
fn share_error(e: ShareError, busy: &str) -> Response {
    match e {
        ShareError::Busy(job) => (
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": busy, "job": job })),
        )
            .into_response(),
        ShareError::CutExists(cut) => (
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "this range is already a cut of this clip", "cut": cut })),
        )
            .into_response(),
        ShareError::QueueFull => (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", "30")],
            Json(json!({ "ok": false, "error": "too many shares are waiting - try again in a moment" })),
        )
            .into_response(),
        ShareError::UnknownClip(base) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown clip: {base}")).into_response()
        }
        ShareError::UnknownJob(id) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown job: {id}")).into_response()
        }
        ShareError::UnknownCut(id) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown cut: {id}")).into_response()
        }
        ShareError::Invalid(msg) => ApiError::new(StatusCode::BAD_REQUEST, msg).into_response(),
    }
}

/// The range of a request body, shared by `/api/share` and `/api/cuts`.
fn share_request(v: &Value) -> ShareRequest {
    ShareRequest {
        base: v["base"].as_str().unwrap_or("").to_string(),
        start: number(&v["start"]),
        end: number(&v["end"]),
        audio: v["audio"].as_str().unwrap_or("").to_string(),
        mode: v["mode"].as_str().unwrap_or("").to_string(),
        target: v["target"].as_str().unwrap_or("").to_string(),
        vertical: v["vertical"].as_bool().unwrap_or(false),
        vertical_pos: v["verticalPos"].as_f64().unwrap_or(0.5),
        after: v["after"].as_str().unwrap_or("").to_string(),
    }
}

async fn share(State(app): State<App>, body: Bytes) -> Response {
    let req = share_request(&parse_body(&body));
    match share::start(&app, req) {
        Ok(started) => accepted(&app, started, Value::Null),
        Err(e) => share_error(e, "this share is already running or waiting"),
    }
}

/// `POST /api/cuts` (since 3.0): save a range as a cut, render nothing.
async fn cuts_create(State(app): State<App>, body: Bytes) -> Response {
    let req = share_request(&parse_body(&body));
    match share::start_cut(&app, req) {
        Ok(started) => accepted(&app, started, Value::Null),
        Err(e) => share_error(e, "this cut is already being made"),
    }
}

/// `GET /api/cuts/<id>` (since 3.0): one cut with its outputs.
async fn cut(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    app.cut_document(&id)
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("unknown cut: {id}")))
}

/// `POST /api/cuts/<id>/render` (since 3.0): encode a cut that exists and
/// send it to a target. What the body leaves out comes from the cut.
async fn cut_render(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let v = parse_body(&body);
    let req = share::RenderRequest {
        target: v["target"].as_str().unwrap_or("").to_string(),
        mode: v["mode"].as_str().unwrap_or("").to_string(),
        audio: v["audio"].as_str().map(str::to_string),
        vertical: v["vertical"].as_bool(),
        vertical_pos: v["verticalPos"].as_f64(),
        after: v["after"].as_str().unwrap_or("").to_string(),
    };
    match share::start_render(&app, &id, req) {
        Ok(started) => accepted(&app, started, Value::Null),
        Err(e) => share_error(e, "this render is already running or waiting"),
    }
}

/// `GET /api/jobs/<id>/file` (since 2.6): the finished MP4 as a download,
/// so the phone puts it in its gallery and the PC in its downloads folder.
async fn job_file(State(app): State<App>, Path(id): Path<String>, req: Request) -> Response {
    let Some(file) = app.job_file(&id) else {
        return ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no finished file for job {id}"),
        )
        .into_response();
    };
    let path = app.paths().shared_dir.join(&file);
    if !path.is_file() {
        return ApiError::new(StatusCode::NOT_FOUND, "the shared file is gone").into_response();
    }
    match ServeFile::new(&path).oneshot(req).await {
        Ok(res) => {
            let mut res = res.map(Body::new);
            res.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            let safe: String = file
                .chars()
                .map(|c| {
                    if c == '"' || c == '\\' || c.is_control() {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            if let Ok(v) = HeaderValue::from_str(&format!(
                "attachment; filename=\"{safe}\"; filename*=UTF-8''{}",
                crate::util::encode_path_segment(&file)
            )) {
                res.headers_mut().insert(CONTENT_DISPOSITION, v);
            }
            res.headers_mut()
                .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
            res
        }
        Err(e) => ApiError::internal(e).into_response(),
    }
}

/// `POST /api/jobs/<id>/post { target }` (since 2.7): post the link of a
/// finished job to one notify integration now; the automatic post only
/// happens for the quick share.
async fn job_post(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let v = parse_body(&body);
    let target = v["target"].as_str().unwrap_or("");
    match share::post_now(&app, &id, target).await {
        Ok(status) => Json(json!({ "ok": true, "status": status })).into_response(),
        Err(ShareError::UnknownJob(id)) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown job: {id}")).into_response()
        }
        Err(ShareError::Invalid(msg)) => {
            ApiError::new(StatusCode::BAD_REQUEST, msg).into_response()
        }
        Err(e) => ApiError::new(StatusCode::BAD_REQUEST, format!("{e:?}")).into_response(),
    }
}

/// `POST /api/clips/<base>/preview` (since 2.6): queue the playable H.264
/// copy for browsers that cannot decode the recording.
async fn clip_preview(State(app): State<App>, Path(base): Path<String>) -> Response {
    match share::start_preview(&app, &base, false) {
        Ok(started) => accepted(&app, started, Value::Null),
        // "the playable preview exists already" is a conflict, not a bad request
        Err(ShareError::Invalid(msg)) => ApiError::new(StatusCode::CONFLICT, msg).into_response(),
        Err(e) => share_error(e, "this preview is already being made"),
    }
}

/// `POST /api/jobs/<id>/publish { target }` (since 2.5): the finished file
/// of a job goes to another storage, without cutting again.
async fn job_publish(State(app): State<App>, Path(id): Path<String>, body: Bytes) -> Response {
    let v = parse_body(&body);
    let target = v["target"].as_str().unwrap_or("");
    match share::publish(&app, &id, target) {
        Ok(started) => accepted(&app, started, json!({ "source": id })),
        Err(e) => share_error(e, "this publish is already running or waiting"),
    }
}

/// `POST /api/jobs/<id>/cancel` (since 2.4): a waiting job leaves the queue,
/// a running one is stopped; past the upload it is too late.
async fn job_cancel(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match app.cancel_job(&id) {
        Ok(at_once) => Ok(Json(json!({ "ok": true, "stopped": at_once }))),
        Err(StateError::UnknownJob) => Err(ApiError::new(StatusCode::NOT_FOUND, "unknown job")),
        Err(e) => Err(ApiError::new(StatusCode::CONFLICT, e.to_string())),
    }
}

/// `POST /api/save`: through obs-websocket when connected (since 2.2),
/// else the simulated key press of 1.x.
async fn save(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let obs = app.obs.status();
    if obs.connected {
        if !obs.replay_active {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "the replay buffer is not running - start it in OBS (or on the OBS page)",
            ));
        }
        if app.dry_run {
            tracing::info!("dry run: SaveReplayBuffer not sent");
        } else {
            app.obs
                .request("SaveReplayBuffer", json!({}))
                .await
                .map_err(ApiError::internal)?;
            tracing::info!("SaveReplayBuffer sent to OBS");
        }
        return Ok(Json(json!({ "ok": true, "via": "obs-websocket" })));
    }
    if app.dry_run {
        tracing::info!("dry run: replay hotkey not sent");
    } else {
        tokio::task::spawn_blocking(platform::press_f9)
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
        tracing::info!("replay hotkey sent to OBS as a key press");
    }
    Ok(Json(json!({ "ok": true, "via": "hotkey" })))
}

async fn media(State(app): State<App>, Path(file): Path<String>, req: Request) -> Response {
    // since 2.4: `<base>.jpg` is the thumbnail, cacheable because it never changes
    if let Some(base) = file.strip_suffix(".jpg") {
        if base.contains(['/', '\\']) {
            return ApiError::new(StatusCode::BAD_REQUEST, "bad path").into_response();
        }
        let path = app.paths().thumb_of(base);
        if !path.is_file() {
            return not_found().await;
        }
        return match ServeFile::new(&path).oneshot(req).await {
            Ok(res) => {
                let mut res = res.map(Body::new);
                res.headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
                res.headers_mut()
                    .insert(CACHE_CONTROL, HeaderValue::from_static("max-age=86400"));
                res
            }
            Err(e) => ApiError::internal(e).into_response(),
        };
    }
    let Some(base) = file.strip_suffix(".mp4") else {
        return not_found().await;
    };
    if base.contains(['/', '\\']) {
        return ApiError::new(StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let path = app.paths().preview_of(base);
    if !path.is_file() {
        return not_found().await;
    }
    match ServeFile::new(&path).oneshot(req).await {
        Ok(res) => {
            let mut res = res.map(Body::new);
            res.headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("video/mp4"));
            res.headers_mut()
                .entry(ACCEPT_RANGES)
                .or_insert(HeaderValue::from_static("bytes"));
            res
        }
        Err(e) => ApiError::internal(e).into_response(),
    }
}
