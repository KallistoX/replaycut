//! The HTTP API as specified in `docs/api.md`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{ACCEPT_RANGES, CACHE_CONTROL, CONTENT_TYPE};
use axum::http::{HeaderValue, StatusCode};
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
use crate::state::{AppState, StateError};

type App = Arc<AppState>;

pub fn router(state: App) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/index.html", get(ui))
        // pages since 2.1: the same file, the JS picks the page by path
        .route("/setup", get(ui))
        .route("/settings", get(ui))
        .route("/diagnostics", get(ui))
        .route("/login", get(ui))
        .route("/obs", get(ui))
        .route("/api/clips", get(clips))
        .route("/api/clips/{base}", axum::routing::delete(delete_clip))
        .route(
            "/api/clips/{base}/name",
            axum::routing::put(set_name).post(set_name),
        )
        .route("/api/history", get(history))
        .route("/api/jobs/{id}", get(job))
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
        .route("/api/addresses", get(admin::addresses))
        .route("/api/session", get(admin::session))
        .route("/api/login", post(admin::login))
        .route("/api/logout", post(admin::logout))
        .route("/api/restart", post(admin::restart))
        .route("/themes/{file}", get(admin::theme))
        .fallback(not_found)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::guard,
        ))
        .layer(axum::middleware::from_fn(auth::origin_check))
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
            StateError::UnknownClip(_) => StatusCode::NOT_FOUND,
            StateError::ClipBusy => StatusCode::CONFLICT,
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

async fn clips(State(app): State<App>) -> Json<Value> {
    Json(app.status())
}

async fn history(State(app): State<App>) -> Json<Value> {
    Json(app.history())
}

async fn job(State(app): State<App>, Path(id): Path<String>) -> Result<Json<Value>, ApiError> {
    match app.job(&id) {
        Some(job) => Ok(Json(serde_json::to_value(job).unwrap_or(Value::Null))),
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "unknown job")),
    }
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
    let remote = query.get("nextcloud").is_some_and(|v| v == "1");
    let clip = app.take_clip_for_delete(&base)?;

    // The MKV plus every share derived from it.
    let prefix = base.split_whitespace().collect::<Vec<_>>().join("_") + "_";
    let mut files = vec![std::path::PathBuf::from(&clip.path)];
    let paths = app.paths();
    if let Ok(entries) = std::fs::read_dir(&paths.shared_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.to_ascii_lowercase().ends_with(".mp4") {
                files.push(e.path());
            }
        }
    }
    let remote_deleted = if remote {
        let runtime = app.runtime();
        let Some(storage) = &runtime.integrations.storage else {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "no storage integration is enabled",
            ));
        };
        let month = share::month_of(&base);
        let mut paths: Vec<String> = files
            .iter()
            .skip(1)
            .filter_map(|f| {
                f.file_name()
                    .map(|n| storage.remote_path(&month, &n.to_string_lossy()))
            })
            .collect();
        paths.extend(app.history_paths_for(&base));
        paths.sort();
        paths.dedup();
        let n = storage.delete(&paths).await.map_err(ApiError::internal)?;
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
    let _ = std::fs::remove_file(paths.preview_of(&base));
    app.forget_clip(&base);
    app.scan_wake.notify_one();
    tracing::info!("deleted {base}: {recycled} file(s) to the recycle bin");
    Ok(Json(
        json!({ "ok": true, "recycled": recycled, "nextcloud": remote_deleted }),
    ))
}

fn number(v: &Value) -> f64 {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .unwrap_or(0.0)
}

async fn share(State(app): State<App>, body: Bytes) -> Response {
    let v = parse_body(&body);
    let req = ShareRequest {
        base: v["base"].as_str().unwrap_or("").to_string(),
        start: number(&v["start"]),
        end: number(&v["end"]),
        audio: v["audio"].as_str().unwrap_or("").to_string(),
    };
    match share::start(&app, req) {
        Ok(id) => {
            tokio::spawn(share::run(app.clone(), id.clone()));
            (StatusCode::ACCEPTED, Json(json!({ "ok": true, "job": id }))).into_response()
        }
        Err(ShareError::Busy(job)) => (
            StatusCode::CONFLICT,
            Json(json!({ "ok": false, "error": "a share is already running", "job": job })),
        )
            .into_response(),
        Err(ShareError::UnknownClip(base)) => {
            ApiError::new(StatusCode::NOT_FOUND, format!("unknown clip: {base}")).into_response()
        }
        Err(ShareError::Invalid(msg)) => {
            ApiError::new(StatusCode::BAD_REQUEST, msg).into_response()
        }
    }
}

async fn save(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    if app.dry_run {
        tracing::info!("dry run: replay hotkey not sent");
    } else {
        tokio::task::spawn_blocking(platform::press_f9)
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
        tracing::info!("replay hotkey sent to OBS");
    }
    Ok(Json(json!({ "ok": true })))
}

async fn media(State(app): State<App>, Path(file): Path<String>, req: Request) -> Response {
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
