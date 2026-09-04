//! The 2.1 endpoints behind the settings page, the setup wizard and the
//! login: settings, integration tests, addresses with a QR code, themes,
//! sessions and the restart. See docs/api.md "Since 2.1".

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::auth;
use crate::credentials;
use crate::http::ApiError;
use crate::integrations::{Discord, Nextcloud};
use crate::platform;
use crate::settings::{is_theme_name, Settings};
use crate::state::{AppState, VERSION};

type App = Arc<AppState>;

const BUILT_IN_THEME: &str = "wardogs";

fn parse_json(headers: &HeaderMap, body: &Bytes) -> Result<Value, ApiError> {
    auth::require_json(headers)?;
    serde_json::from_slice(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

fn theme_names(app: &AppState) -> Vec<String> {
    let mut names = vec![BUILT_IN_THEME.to_string()];
    if let Ok(entries) = std::fs::read_dir(app.data_dir.join("themes")) {
        for e in entries.flatten() {
            let file = e.file_name().to_string_lossy().to_string();
            if let Some(name) = file.strip_suffix(".css") {
                if is_theme_name(name) && name != BUILT_IN_THEME {
                    names.push(name.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

#[cfg(windows)]
fn autostart_enabled() -> bool {
    crate::winshell::autostart_entry().is_some()
}
#[cfg(not(windows))]
fn autostart_enabled() -> bool {
    false
}

#[cfg(windows)]
fn set_autostart(on: bool) -> anyhow::Result<()> {
    if on {
        let exe = std::env::current_exe()?;
        crate::winshell::set_autostart(&exe)
    } else {
        crate::winshell::clear_autostart().map(|_| ())
    }
}
#[cfg(not(windows))]
fn set_autostart(_on: bool) -> anyhow::Result<()> {
    anyhow::bail!("autostart is only available on Windows")
}

/// `GET /api/settings`
fn settings_document(app: &AppState) -> Value {
    let settings = app.settings();
    let mut doc = settings.public_json();
    doc["secrets"] = json!({
        "nextcloud": credentials::read(credentials::NEXTCLOUD).ok().flatten().is_some(),
        "discord": credentials::read(credentials::DISCORD_WEBHOOK).ok().flatten().is_some(),
        "obs": credentials::read(credentials::OBS_WEBSOCKET).ok().flatten().is_some(),
    });
    doc["passwordSet"] = json!(settings.password_hash.is_some());
    doc["autostart"] = json!(autostart_enabled());
    doc["themes"] = json!(theme_names(app));
    doc["restartNeeded"] = json!(*app.pending_restart.lock());
    doc["version"] = json!(VERSION);
    doc["overrides"] = json!({
        "clipDir": app.overrides.clip_dir.is_some(),
        "port": app.overrides.port.is_some(),
        "bind": app.overrides.bind.is_some(),
    });
    doc
}

pub async fn get_settings(State(app): State<App>) -> Json<Value> {
    Json(settings_document(&app))
}

/// `PUT /api/settings`: a partial object. Secrets (`nextcloud.user`,
/// `nextcloud.password`, `discord.webhook`, `password`) and `autostart`
/// are taken out first; the rest is merged into the file and applied.
pub async fn put_settings(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let mut patch = parse_json(&headers, &body)?;
    let Some(obj) = patch.as_object_mut() else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "body must be a JSON object",
        ));
    };
    let password = obj.remove("password");
    let autostart = obj.remove("autostart");
    let nextcloud_user = obj.remove("nextcloudUser");
    let nextcloud_password = obj.remove("nextcloudPassword");
    let discord_webhook = obj.remove("discordWebhook");
    let obs_password = obj.remove("obsPassword");

    // Fields a running job depends on.
    if app.inner.lock().current_job.is_some() {
        let touches_job = obj.contains_key("clipDir") || obj.contains_key("encoder");
        if touches_job {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "clipDir and encoder cannot change while a share is running",
            ));
        }
    }

    // Effective settings (with overrides) for the runtime, file settings for the disk.
    let current = app.settings();
    let mut next = current
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let disk = Settings::load_or_create(&app.settings_path).map_err(ApiError::internal)?;
    let mut disk_next = disk
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;

    if let Some(pw) = password {
        let pw = pw.as_str().unwrap_or("");
        if pw.is_empty() {
            next.password_hash = None;
            disk_next.password_hash = None;
            app.sessions.clear();
            tracing::info!("password removed");
        } else {
            if pw.chars().count() < 6 {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "password: use at least 6 characters",
                ));
            }
            let hash = auth::hash_password(pw).map_err(ApiError::internal)?;
            next.password_hash = Some(hash.clone());
            disk_next.password_hash = Some(hash);
            tracing::info!("password set");
        }
    }

    let mut credentials_changed = false;
    if let (Some(user), Some(pw)) = (
        nextcloud_user.as_ref().and_then(Value::as_str),
        nextcloud_password.as_ref().and_then(Value::as_str),
    ) {
        if user.is_empty() || pw.is_empty() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "nextcloudUser and nextcloudPassword must not be empty",
            ));
        }
        credentials::write(credentials::NEXTCLOUD, user, pw).map_err(ApiError::internal)?;
        credentials_changed = true;
    } else if nextcloud_user.is_some() || nextcloud_password.is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "send nextcloudUser and nextcloudPassword together",
        ));
    }
    if let Some(webhook) = discord_webhook.as_ref().and_then(Value::as_str) {
        if !crate::integrations::is_webhook_url(webhook) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "discordWebhook: this does not look like a Discord webhook URL",
            ));
        }
        credentials::write(credentials::DISCORD_WEBHOOK, "webhook", webhook)
            .map_err(ApiError::internal)?;
        credentials_changed = true;
    }

    let mut obs_changed = false;
    if let Some(pw) = obs_password.as_ref().and_then(Value::as_str) {
        if pw.is_empty() {
            credentials::delete(credentials::OBS_WEBSOCKET).map_err(ApiError::internal)?;
        } else {
            credentials::write(credentials::OBS_WEBSOCKET, "obs-websocket", pw)
                .map_err(ApiError::internal)?;
        }
        obs_changed = true;
        tracing::info!(
            "OBS password {}",
            if pw.is_empty() { "removed" } else { "stored" }
        );
    }

    disk_next
        .save(&app.settings_path)
        .map_err(ApiError::internal)?;
    let restart = app
        .apply_settings(next)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    if credentials_changed {
        app.rebuild_runtime()
            .await
            .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    }
    if obs_changed {
        app.obs
            .reconfigure(crate::obs_link::config_from(&app.settings()));
    }
    if let Some(on) = autostart.as_ref().and_then(Value::as_bool) {
        set_autostart(on).map_err(ApiError::internal)?;
        tracing::info!("autostart {}", if on { "on" } else { "off" });
    }
    tracing::info!(
        "settings saved ({} field(s))",
        patch.as_object().map_or(0, |o| o.len())
    );
    Ok(Json(json!({
        "ok": true,
        "restartNeeded": restart,
        "settings": settings_document(&app),
    })))
}

/// `POST /api/test/nextcloud`
pub async fn test_nextcloud(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let current = app.settings();
    let url = v["url"]
        .as_str()
        .unwrap_or(&current.integrations.nextcloud.url)
        .to_string();
    let folder = v["folder"]
        .as_str()
        .unwrap_or(&current.integrations.nextcloud.folder)
        .to_string();
    let (user, password) = match (v["user"].as_str(), v["password"].as_str()) {
        (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => (u.to_string(), p.to_string()),
        _ => match credentials::read(credentials::NEXTCLOUD) {
            Ok(Some(c)) => (c.user, c.secret),
            _ => {
                return Ok(Json(json!({
                    "ok": false,
                    "error": "no Nextcloud credentials stored - enter user and app password"
                })))
            }
        },
    };
    let started = std::time::Instant::now();
    let result = async {
        let nc = Nextcloud::with_values(&url, &folder, 0, user.clone(), password)?;
        nc.user_info().await
    }
    .await;
    Ok(Json(match result {
        Ok(info) => json!({
            "ok": true,
            "user": user,
            "displayName": info.display_name,
            "freeBytes": info.free,
            "totalBytes": info.total,
            "ms": started.elapsed().as_millis() as u64,
        }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

/// `POST /api/test/discord`
pub async fn test_discord(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let current = app.settings();
    let name = v["displayName"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&current.display_name)
        .to_string();
    let webhook = match v["webhook"].as_str().filter(|s| !s.is_empty()) {
        Some(w) => w.to_string(),
        None => match credentials::read(credentials::DISCORD_WEBHOOK) {
            Ok(Some(c)) => c.secret,
            _ => {
                return Ok(Json(json!({
                    "ok": false,
                    "error": "no webhook stored - paste the webhook URL"
                })))
            }
        },
    };
    if app.dry_run {
        return Ok(Json(json!({ "ok": true, "dryRun": true })));
    }
    let result = async {
        let d = Discord::new(webhook, name)?;
        d.post("replaycut test message - if you can read this, the webhook works")
            .await
    }
    .await;
    Ok(Json(match result {
        Ok(status) if status.starts_with("Link posted") => json!({ "ok": true }),
        Ok(status) => json!({ "ok": false, "error": status }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

/// `GET /api/addresses`: how other devices reach this service, with a QR
/// code (SVG) for the first address.
pub async fn addresses(State(app): State<App>) -> Json<Value> {
    let settings = app.settings();
    let port = settings.port;
    let mut urls = Vec::new();
    if settings.bind != "127.0.0.1" && settings.bind != "::1" {
        urls.push(format!("http://{}:{port}/", platform::hostname()));
        if let Some(ip) = platform::primary_ipv4() {
            urls.push(format!("http://{ip}:{port}/"));
        }
    }
    urls.push(format!("http://localhost:{port}/"));
    let qr_svg = qrcode::QrCode::new(urls[0].as_bytes())
        .map(|code| {
            code.render::<qrcode::render::svg::Color>()
                .min_dimensions(160, 160)
                .quiet_zone(true)
                .build()
        })
        .unwrap_or_default();
    Json(json!({
        "hostname": platform::hostname(),
        "port": port,
        "bind": settings.bind,
        "urls": urls,
        "qrSvg": qr_svg,
    }))
}

/// `GET /themes/<name>.css`
pub async fn theme(State(app): State<App>, Path(file): Path<String>) -> Response {
    let Some(name) = file.strip_suffix(".css") else {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !is_theme_name(name) {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = app.data_dir.join("themes").join(&file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (CONTENT_TYPE, "text/css; charset=utf-8"),
                (CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => ApiError::new(StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// `GET /api/session`
pub async fn session(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    let loopback = auth::is_loopback(&addr);
    let password_set = app.password_set();
    let has_session = auth::cookie_token(&headers).is_some_and(|t| app.sessions.is_valid(&t));
    Json(json!({
        "authenticated": !password_set || loopback || has_session,
        "loopback": loopback,
        "passwordSet": password_set,
    }))
}

/// `POST /api/login`
pub async fn login(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let v = parse_json(&headers, &body)?;
    let settings = app.settings();
    let Some(hash) = settings.password_hash.as_deref() else {
        return Ok(Json(json!({ "ok": true, "note": "no password is set" })).into_response());
    };
    if let Err(seconds) = app.sessions.check_lockout(addr.ip()) {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many attempts - wait {seconds} s"),
        ));
    }
    let password = v["password"].as_str().unwrap_or("").to_string();
    let ok = {
        let hash = hash.to_string();
        tokio::task::spawn_blocking(move || auth::verify_password(&hash, &password))
            .await
            .unwrap_or(false)
    };
    if !ok {
        app.sessions.record_failure(addr.ip());
        tracing::warn!("login failed from {}", addr.ip());
        tokio::time::sleep(auth::Sessions::failure_delay()).await;
        return Err(ApiError::new(StatusCode::UNAUTHORIZED, "wrong password"));
    }
    app.sessions.record_success(addr.ip());
    let token = app.sessions.create();
    tracing::info!("login from {}", addr.ip());
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = auth::set_cookie_value(&token).parse() {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    Ok(res)
}

/// `POST /api/logout`
pub async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::cookie_token(&headers) {
        app.sessions.remove(&token);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = auth::clear_cookie_value().parse() {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    res
}

/// `POST /api/restart`: start a fresh process that waits for this one to
/// release the single-instance mutex, then shut down.
pub async fn restart(State(app): State<App>) -> Result<Response, ApiError> {
    let Some(shutdown) = app.shutdown.get() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "restart not available",
        ));
    };
    if app.inner.lock().current_job.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "a share is running - restart afterwards",
        ));
    }
    platform::spawn_self_for_restart().map_err(|e| ApiError::internal(format!("{e:#}")))?;
    tracing::info!("restart requested from the settings page");
    shutdown.request("restart");
    app.pending_restart.lock().clear();
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response())
}

/// `GET /api/setup/obs`: what the wizard's OBS step shows - the OBS
/// profiles on this machine (read only), the folder being watched and the
/// newest clip with its video facts.
pub async fn setup_obs(State(app): State<App>) -> Json<Value> {
    let profiles = tokio::task::spawn_blocking(crate::obs::profiles)
        .await
        .unwrap_or_default();
    let paths = app.paths();
    let newest = {
        let inner = app.inner.lock();
        inner
            .clips
            .values()
            .max_by(|a, b| a.created.cmp(&b.created))
            .map(|c| {
                json!({
                    "name": c.name,
                    "base": c.base,
                    "created": c.created,
                    "duration": c.duration,
                    "tracks": c.tracks,
                    "codec": c.codec,
                    "width": c.width,
                    "height": c.height,
                    "fps": c.fps,
                    "container": "mkv",
                })
            })
    };
    // MP4 and other containers in the folder are not clips; the wizard names them.
    let mut others: Vec<String> = std::fs::read_dir(&paths.clip_dir)
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                    (e.path().is_file()
                        && ["mp4", "mov", "flv", "ts", "m4v"].contains(&ext.as_str()))
                    .then_some(name)
                })
                .collect()
        })
        .unwrap_or_default();
    others.sort();
    others.reverse();
    others.truncate(3);
    Json(json!({
        "profiles": profiles,
        "watching": paths.clip_dir.to_string_lossy(),
        "newest": newest,
        "otherFiles": others,
        "encoder": app.runtime().encoder.name,
    }))
}

/// `GET /api/diagnostics`: every check with its status plus the text copy.
pub async fn diagnostics(State(app): State<App>) -> Json<Value> {
    Json(crate::diagnostics::run(&app).await.json())
}

/// `POST /api/obs/reconnect` (since 2.2): connect now instead of waiting
/// out the backoff, e.g. after the WebSocket server was switched on in OBS.
pub async fn obs_reconnect(State(app): State<App>) -> Json<Value> {
    app.obs.reconnect_now();
    Json(json!({ "ok": true }))
}
