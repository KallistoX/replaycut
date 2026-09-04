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

use crate::platform;
use crate::state::{AppState, StateError};

type App = Arc<AppState>;

pub fn router(state: App) -> Router {
    Router::new()
        .route("/", get(ui))
        .route("/index.html", get(ui))
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
        .fallback(not_found)
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
    let bytes = tokio::fs::read(&app.paths.ui_file).await.map_err(|e| {
        ApiError::internal(format!(
            "UI file {} unreadable: {e}",
            app.paths.ui_file.display()
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
    if let Ok(entries) = std::fs::read_dir(&app.paths.shared_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix) && name.to_ascii_lowercase().ends_with(".mp4") {
                files.push(e.path());
            }
        }
    }
    if remote {
        tracing::debug!(
            "remote deletion requested - storage integrations arrive with the share pipeline"
        );
    }
    let remote_deleted = 0;
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
    let _ = std::fs::remove_file(app.paths.preview_of(&base));
    app.forget_clip(&base);
    app.scan_wake.notify_one();
    tracing::info!("deleted {base}: {recycled} file(s) to the recycle bin");
    Ok(Json(
        json!({ "ok": true, "recycled": recycled, "nextcloud": remote_deleted }),
    ))
}

async fn share(State(_app): State<App>, _body: Bytes) -> Result<Json<Value>, ApiError> {
    Err(ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "sharing is not implemented yet",
    ))
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
    let path = app.paths.preview_of(base);
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
