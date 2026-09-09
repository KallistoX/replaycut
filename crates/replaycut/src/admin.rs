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
use crate::themes::{self, BUILT_IN_THEME};

type App = Arc<AppState>;

fn parse_json(headers: &HeaderMap, body: &Bytes) -> Result<Value, ApiError> {
    auth::require_json(headers)?;
    serde_json::from_slice(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")))
}

/// The built-in themes plus every readable file in `<data-dir>/themes/`.
fn theme_names(app: &AppState) -> Vec<String> {
    let mut names = vec![BUILT_IN_THEME.to_string()];
    names.extend(themes::BUILT_IN_THEMES.iter().map(|(n, _)| n.to_string()));
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
#[cfg(target_os = "linux")]
fn autostart_enabled() -> bool {
    crate::linuxshell::autostart_enabled()
}
#[cfg(not(any(windows, target_os = "linux")))]
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
#[cfg(target_os = "linux")]
fn set_autostart(on: bool) -> anyhow::Result<()> {
    if on {
        let exe = std::env::current_exe()?;
        crate::linuxshell::set_autostart(&exe)
    } else {
        crate::linuxshell::clear_autostart().map(|_| ())
    }
}
#[cfg(not(any(windows, target_os = "linux")))]
fn set_autostart(_on: bool) -> anyhow::Result<()> {
    anyhow::bail!("autostart is only available on Windows and Linux")
}

/// `GET /api/settings`
fn settings_document(app: &AppState) -> Value {
    let settings = app.settings();
    let mut doc = settings.public_json();
    doc["secrets"] = json!({
        "nextcloud": credentials::read(credentials::NEXTCLOUD).ok().flatten().is_some(),
        "discord": credentials::read(credentials::DISCORD_WEBHOOK).ok().flatten().is_some(),
        "obs": credentials::read(credentials::OBS_WEBSOCKET).ok().flatten().is_some(),
        "onedrive": credentials::read(credentials::ONEDRIVE).ok().flatten().is_some(),
        "s3": credentials::read(credentials::S3).ok().flatten().is_some(),
        "webdav": credentials::read(credentials::WEBDAV).ok().flatten().is_some(),
        "youtube": credentials::read(credentials::YOUTUBE).ok().flatten().is_some(),
        "youtubeClient": credentials::read(credentials::YOUTUBE_CLIENT).ok().flatten().is_some(),
        "telegram": credentials::read(credentials::TELEGRAM).ok().flatten().is_some(),
        "webhookSecret": credentials::read(credentials::WEBHOOK_SECRET).ok().flatten().is_some(),
    });
    doc["passwordSet"] = json!(settings.password_hash.is_some());
    doc["autostart"] = json!(autostart_enabled());
    doc["themes"] = json!(theme_names(app));
    doc["restartNeeded"] = json!(*app.pending_restart.lock());
    // since 3.4: what the HTTPS card shows about the certificate in use.
    // `caFile` is the file to import into a browser by hand, `fingerprint`
    // the value to compare it against; both are empty with an own PEM pair.
    doc["tls"] = json!({
        "active": app.tls.active,
        "own": app.tls.own,
        "caFile": app.tls.ca_file.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
        "fingerprint": app.tls.fingerprint,
        "expires": app.tls.not_after.map(|t| t.format("%Y-%m-%d").to_string()),
    });
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
    let s3_access_key = obj.remove("s3AccessKey");
    let s3_secret_key = obj.remove("s3SecretKey");
    let webdav_user = obj.remove("webdavUser");
    let webdav_password = obj.remove("webdavPassword");
    let youtube_client_id = obj.remove("youtubeClientId");
    let youtube_client_secret = obj.remove("youtubeClientSecret");
    let telegram_token = obj.remove("telegramToken");
    let webhook_secret = obj.remove("webhookSecret");

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
            let len = pw.chars().count();
            if !(auth::PASSWORD_MIN..=auth::PASSWORD_MAX).contains(&len) {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "password: use between {} and {} characters",
                        auth::PASSWORD_MIN,
                        auth::PASSWORD_MAX
                    ),
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
    // S3 keys, WebDAV login (since 2.5) and the Google client for YouTube
    // (since 2.6): both halves together, empty removes
    let mut youtube_client_changed = false;
    for (a, b, target, name) in [
        (
            &s3_access_key,
            &s3_secret_key,
            credentials::S3,
            "s3AccessKey and s3SecretKey",
        ),
        (
            &webdav_user,
            &webdav_password,
            credentials::WEBDAV,
            "webdavUser and webdavPassword",
        ),
        (
            &youtube_client_id,
            &youtube_client_secret,
            credentials::YOUTUBE_CLIENT,
            "youtubeClientId and youtubeClientSecret",
        ),
    ] {
        if target == credentials::YOUTUBE_CLIENT && (a.is_some() || b.is_some()) {
            youtube_client_changed = true;
        }
        match (
            a.as_ref().and_then(Value::as_str),
            b.as_ref().and_then(Value::as_str),
        ) {
            (Some(""), Some("")) => {
                credentials::delete(target).map_err(ApiError::internal)?;
                credentials_changed = true;
            }
            (Some(u), Some(p)) if !u.is_empty() && !p.is_empty() => {
                credentials::write(target, u, p).map_err(ApiError::internal)?;
                credentials_changed = true;
            }
            (None, None) => {}
            _ => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("send {name} together (both empty removes them)"),
                ))
            }
        }
    }
    // single write-only secrets of 2.6: empty removes
    for (value, target, user, what) in [
        (
            &telegram_token,
            credentials::TELEGRAM,
            "bot",
            "Telegram bot token",
        ),
        (
            &webhook_secret,
            credentials::WEBHOOK_SECRET,
            "secret",
            "webhook secret",
        ),
    ] {
        if let Some(v) = value.as_ref().and_then(Value::as_str) {
            if v.trim().is_empty() {
                credentials::delete(target).map_err(ApiError::internal)?;
                tracing::info!("{what} removed");
            } else {
                if target == credentials::TELEGRAM && !v.contains(':') {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "telegramToken: this does not look like a bot token (123456:ABC...)",
                    ));
                }
                credentials::write(target, user, v.trim()).map_err(ApiError::internal)?;
                tracing::info!("{what} stored");
            }
            credentials_changed = true;
        }
    }
    // A refresh token belongs to the client that issued it, so switching
    // between the built-in client and one's own, or between the TV and the
    // desktop client, ends the connection just as storing a new client does
    // (since 3.3).
    let yt_before = &current.integrations.youtube;
    let yt_after = &next.integrations.youtube;
    if yt_before.client != yt_after.client || yt_before.client_type != yt_after.client_type {
        youtube_client_changed = true;
    }
    // a new Google client invalidates the channel connected with the old one.
    // Best effort: a settings change must not fail because the credential
    // store is out of reach - the connection is stale either way.
    if youtube_client_changed {
        match credentials::delete(credentials::YOUTUBE) {
            Ok(true) => {
                tracing::info!("YouTube: the channel was disconnected because the client changed")
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("YouTube: cannot remove the channel token: {e:#}"),
        }
        app.oauth.lock().remove("youtube");
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
/// code (SVG) for the first address. Since 2.8 that code signs the phone
/// in: its URL carries a one-time token, which only this PC and signed-in
/// devices get - whoever sees the code is at the PC or already in.
pub async fn addresses(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    let settings = app.settings();
    let port = settings.port;
    let scheme = app.scheme();
    let mut urls = Vec::new();
    let local = settings.bind == "127.0.0.1" || settings.bind == "::1";
    if !local {
        urls.push(format!("{scheme}://{}:{port}/", platform::lan_host()));
        if let Some(ip) = platform::primary_ipv4() {
            urls.push(format!("{scheme}://{ip}:{port}/"));
        }
    }
    urls.push(format!("{scheme}://localhost:{port}/"));
    let signs_in = !local && auth::is_authenticated(&app, &addr, &headers);
    // A QR code for localhost would only lead a phone to itself.
    let qr_svg = if local {
        String::new()
    } else {
        let target = if signs_in {
            // The fingerprint rides along as a fragment (since 3.4): a
            // browser never sends it, so it stays out of every log, while a
            // client of ours reads the code itself and can pin the authority
            // before it trusts anything - the first contact included.
            format!(
                "{}?pair={}{}",
                urls[0],
                app.pairing.qr_token(),
                fingerprint_fragment(&app)
            )
        } else {
            format!("{}{}", urls[0], fingerprint_fragment(&app))
        };
        qrcode::QrCode::new(target.as_bytes())
            .map(|code| {
                code.render::<qrcode::render::svg::Color>()
                    .min_dimensions(160, 160)
                    .quiet_zone(true)
                    .build()
            })
            .unwrap_or_default()
    };
    Json(json!({
        "hostname": platform::hostname(),
        "port": port,
        "bind": settings.bind,
        "local": local,
        "urls": urls,
        "qrSvg": qr_svg,
        // since 2.8: whether scanning the code signs the phone in
        "qrSignsIn": signs_in,
        // since 3.4
        "scheme": scheme,
        "fingerprint": app.tls.fingerprint,
    }))
}

/// `#fp=<fingerprint>` for the QR code, empty when there is nothing to pin:
/// without HTTPS, and with an own PEM pair, where the certificate is trusted
/// by everyone anyway and a client validates it the ordinary way.
fn fingerprint_fragment(app: &AppState) -> String {
    match app.tls.fingerprint.as_str() {
        "" => String::new(),
        fp => format!("#fp={fp}"),
    }
}

/// `GET /themes/<name>.css`: the file in `<data-dir>/themes/` if there is
/// one, else the built-in theme of that name, so a copy in the folder wins.
pub async fn theme(State(app): State<App>, Path(file): Path<String>) -> Response {
    let Some(name) = file.strip_suffix(".css") else {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !is_theme_name(name) {
        return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response();
    }
    let path = app.data_dir.join("themes").join(&file);
    let css = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(_) => match themes::built_in(name) {
            Some(css) => css.as_bytes().to_vec(),
            None => return ApiError::new(StatusCode::NOT_FOUND, "not found").into_response(),
        },
    };
    (
        [
            (CONTENT_TYPE, "text/css; charset=utf-8"),
            (CACHE_CONTROL, "no-store"),
        ],
        css,
    )
        .into_response()
}

/// `GET /api/session`
pub async fn session(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Json<Value> {
    let loopback = auth::is_loopback(&addr);
    let password_set = app.password_set();
    Json(json!({
        "authenticated": auth::is_authenticated(&app, &addr, &headers),
        "loopback": loopback,
        "passwordSet": password_set,
        // since 2.8: the pages say the name of this PC instead of "this PC",
        // which is wrong on a phone
        "host": platform::hostname(),
        "network": app.network_mode(),
        // false while the device login is paused after a flood
        "pairing": app.pairing.paused().is_none(),
    }))
}

// ---------------------------------------------------------- device login

fn user_agent(headers: &HeaderMap) -> &str {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Only this PC and signed-in devices see and answer sign-in requests.
fn require_trust(app: &AppState, addr: &SocketAddr, headers: &HeaderMap) -> Result<(), ApiError> {
    if auth::is_authenticated(app, addr, headers) {
        Ok(())
    } else {
        Err(ApiError::new(StatusCode::UNAUTHORIZED, "login required"))
    }
}

/// `POST /api/pair/request { name }` (since 2.8): a device asks for access.
/// The PC learns about it through a toast, the open UI and the tray.
pub async fn pair_request(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let v = parse_json(&headers, &body)?;
    let agent = user_agent(&headers);
    let name = match v["name"].as_str().map(str::trim).unwrap_or("") {
        "" => auth::device_name(agent),
        n => n.to_string(),
    };
    // since 3.4: a client of ours asks for the token as a value, a browser
    // for a cookie. The answer to the poll follows this.
    let client = auth::Client::parse(v["client"].as_str())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    match app.pairing.ask(&name, agent, addr.ip(), client) {
        Ok((id, code, expires)) => {
            crate::pairing::watch(&app);
            // `localhost`, not `127.0.0.1`: with HTTPS on, the certificate
            // carries the name, and browsers are happier with a name than
            // with an address SAN. Loopback either way.
            let url = format!("{}approve/{id}", app.ui_url());
            crate::toast::show(
                &app,
                crate::toast::Toast::sign_in_request(&name, &addr.ip().to_string(), &code, &url),
            );
            app.tray_changed();
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({ "ok": true, "id": id, "code": code, "expires": expires })),
            )
                .into_response())
        }
        Err(crate::pairing::Refused::Paused(seconds)) => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!(
                "too many sign-in requests - the device login pauses for {seconds} s, the password still works"
            ),
        )),
        Err(crate::pairing::Refused::TooMany) => Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too many sign-in requests at once - try again in a minute",
        )),
    }
}

/// `GET /api/pair/<id>` (since 2.8): the device asks how it is going. The
/// cookie comes with the first answer after an approval.
pub async fn pair_poll(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> Response {
    let answer = app.pairing.poll(&id, addr.ip());
    let mut doc = json!({ "ok": true, "status": answer.status });
    // A client of ours gets the token in the body and no cookie; a browser
    // gets the cookie and never sees the value. Mixing the two would give
    // up HttpOnly for every browser (since 3.4).
    let cookie = match (&answer.token, answer.client.is_native()) {
        (Some(token), true) => {
            doc["token"] = json!(token);
            None
        }
        (Some(token), false) => Some(token.clone()),
        (None, _) => None,
    };
    let mut res = Json(doc).into_response();
    if let Some(token) = cookie {
        if let Ok(v) = auth::set_cookie_value(&token, app.tls.active).parse() {
            res.headers_mut().insert(SET_COOKIE, v);
        }
    }
    res
}

/// `GET /api/pair/pending` (since 2.8): what the approve page shows.
pub async fn pair_pending(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_trust(&app, &addr, &headers)?;
    Ok(Json(
        json!({ "ok": true, "pending": app.pairing.pending() }),
    ))
}

/// `POST /api/pair/<id>/approve` (since 2.8): the device gets a session
/// with the name it asked under.
pub async fn pair_approve(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_trust(&app, &addr, &headers)?;
    let sessions = &app.sessions;
    let request = app
        .pairing
        .approve(&id, |r| {
            sessions.create(auth::NewSession {
                name: r.name.clone(),
                agent: r.agent.clone(),
                ip: r.ip.to_string(),
                via: auth::Via::Approve,
                client: r.client,
            })
        })
        .map_err(decided_error)?;
    app.tray_changed();
    Ok(Json(json!({ "ok": true, "name": request.name })))
}

/// `POST /api/pair/<id>/deny` (since 2.8).
pub async fn pair_deny(
    State(app): State<App>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_trust(&app, &addr, &headers)?;
    let request = app.pairing.deny(&id).map_err(decided_error)?;
    app.tray_changed();
    Ok(Json(json!({ "ok": true, "name": request.name })))
}

fn decided_error(e: crate::pairing::NotDecided) -> ApiError {
    match e {
        crate::pairing::NotDecided::Unknown => {
            ApiError::new(StatusCode::NOT_FOUND, "unknown sign-in request")
        }
        crate::pairing::NotDecided::Settled => ApiError::new(
            StatusCode::CONFLICT,
            "this sign-in request is already answered or has run out",
        ),
    }
}

/// `GET /api/password/suggest` (since 2.8): four words from the EFF short
/// word list, for "Generate one for me". Nothing is stored; the password
/// only exists once the caller sends it back with `PUT /api/settings`.
pub async fn password_suggest() -> Json<Value> {
    Json(json!({ "password": crate::wordlist::passphrase(4) }))
}

/// Write one field of the settings to disk and make it effective; returns
/// the restart-only fields that changed.
async fn patch_settings(app: &AppState, patch: Value) -> Result<Vec<&'static str>, ApiError> {
    let next = app
        .settings()
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let disk_next = Settings::load_or_create(&app.settings_path)
        .map_err(ApiError::internal)?
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    disk_next
        .save(&app.settings_path)
        .map_err(ApiError::internal)?;
    app.apply_settings(next)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))
}

/// The elevated step for the firewall rule. It is the one thing here that
/// needs administrator rights, and it may be declined - then the switch
/// still flips and the diagnostics say the rule is missing.
async fn firewall(app: &AppState, add: bool) -> &'static str {
    if app.dry_run {
        tracing::info!(
            "dry run: would {} the firewall rule",
            if add { "add" } else { "remove" }
        );
        return "dryRun";
    }
    #[cfg(windows)]
    {
        let port = app.settings().port;
        let result =
            tokio::task::spawn_blocking(move || crate::install::firewall_rule(port, add)).await;
        match result {
            Ok(Ok(crate::winshell::Elevated::Exit(0))) => {
                tracing::info!("firewall rule {}", if add { "added" } else { "removed" });
                if add {
                    "added"
                } else {
                    "removed"
                }
            }
            Ok(Ok(crate::winshell::Elevated::Cancelled)) => {
                tracing::warn!("the firewall step was declined");
                "declined"
            }
            Ok(Ok(crate::winshell::Elevated::Exit(code))) => {
                tracing::warn!("the firewall step exited with {code}");
                "failed"
            }
            Ok(Err(e)) => {
                tracing::warn!("firewall step: {e:#}");
                "failed"
            }
            Err(e) => {
                tracing::warn!("firewall step: {e}");
                "failed"
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = add;
        "unavailable"
    }
}

/// `POST /api/network/enable` (since 2.8): make the service reachable from
/// the network. Without a password there is nothing to protect it, so the
/// answer is a 409 and the caller sets one first.
pub async fn network_enable(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    if !app.password_set() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "set a password first - without one anybody in the network could use this replaycut",
        ));
    }
    let restart = patch_settings(&app, json!({ "bind": "0.0.0.0" })).await?;
    let firewall = firewall(&app, true).await;
    tracing::info!("network access on (firewall: {firewall})");
    Ok(Json(json!({
        "ok": true,
        "network": "lan",
        "firewall": firewall,
        "restartNeeded": !restart.is_empty(),
    })))
}

/// `POST /api/network/disable` (since 2.8): back to this PC only.
pub async fn network_disable(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let restart = patch_settings(&app, json!({ "bind": "127.0.0.1" })).await?;
    let firewall = firewall(&app, false).await;
    tracing::info!("network access off (firewall: {firewall})");
    Ok(Json(json!({
        "ok": true,
        "network": "loopback",
        "firewall": firewall,
        "restartNeeded": !restart.is_empty(),
    })))
}

/// `GET /api/sessions` (since 2.8): the devices that are signed in.
pub async fn sessions(State(app): State<App>, headers: HeaderMap) -> Json<Value> {
    let current = auth::session_token(&headers);
    Json(json!({ "ok": true, "sessions": app.sessions.list(current.as_deref()) }))
}

/// `DELETE /api/sessions/<id>` (since 2.8): that device has to sign in again.
pub async fn session_revoke(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match app.sessions.revoke(&id) {
        Some(name) => {
            tracing::info!("session of {name} revoked");
            Ok(Json(json!({ "ok": true, "name": name })))
        }
        None => Err(ApiError::new(StatusCode::NOT_FOUND, "unknown session")),
    }
}

/// `POST /api/sessions/clear` (since 2.8): everyone but the caller.
pub async fn sessions_clear(State(app): State<App>, headers: HeaderMap) -> Json<Value> {
    let removed = app
        .sessions
        .clear_except(auth::session_token(&headers).as_deref());
    tracing::info!("{removed} session(s) signed out");
    Json(json!({ "ok": true, "removed": removed }))
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
    // since 2.8: the same for all addresses together
    if let Err(seconds) = app.sessions.check_global() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many failed logins - the password login pauses for {seconds} s"),
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
    let agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // since 3.4, as for the device login: `native` takes the token in the
    // body, a browser gets the cookie and never sees the value.
    let client = auth::Client::parse(v["client"].as_str())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let new = auth::NewSession::from_agent(agent, addr.ip(), auth::Via::Password).held_by(client);
    tracing::info!("login from {} ({})", addr.ip(), new.name);
    let token = app.sessions.create(new);
    if client.is_native() {
        return Ok(Json(json!({ "ok": true, "token": token })).into_response());
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = auth::set_cookie_value(&token, app.tls.active).parse() {
        res.headers_mut().insert(SET_COOKIE, v);
    }
    Ok(res)
}

/// `POST /api/logout`
pub async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    if let Some(token) = auth::session_token(&headers) {
        app.sessions.remove(&token);
    }
    let mut res = Json(json!({ "ok": true })).into_response();
    if let Ok(v) = auth::clear_cookie_value(app.tls.active).parse() {
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

/// `GET /api/obs` (since 2.2): the connection, the facts and the checks.
pub async fn obs_status(State(app): State<App>) -> Json<Value> {
    let status = app.obs.status();
    let settings = app.settings();
    let checks = status
        .facts
        .as_ref()
        .map(|f| crate::obs_status::checks(f, status.replay_active, &settings))
        .unwrap_or_default();
    let mut doc = serde_json::to_value(&status).unwrap_or(Value::Null);
    doc["checks"] = json!(checks);
    doc["settings"] = json!({
        "host": settings.obs.host,
        "port": settings.obs.port,
        "enabled": settings.obs.enabled,
        "passwordSet": credentials::read(credentials::OBS_WEBSOCKET).ok().flatten().is_some(),
    });
    Json(doc)
}

/// `POST /api/obs/replay-buffer/start` (since 2.2)
pub async fn obs_start_replay(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let status = app.obs.status();
    if !status.connected {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            status
                .reason
                .map(|r| format!("OBS is not connected: {r}"))
                .unwrap_or_else(|| "OBS is not connected".into()),
        ));
    }
    if status.replay_active {
        return Ok(Json(json!({ "ok": true, "note": "already running" })));
    }
    if app.dry_run {
        tracing::info!("dry run: StartReplayBuffer not sent");
    } else {
        app.obs
            .request("StartReplayBuffer", json!({}))
            .await
            .map_err(ApiError::internal)?;
        tracing::info!("StartReplayBuffer sent to OBS");
    }
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/obs/adopt-folder` (since 2.2): make the OBS recording folder
/// the clip folder of replaycut. Changes replaycut only.
pub async fn obs_adopt_folder(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    let status = app.obs.status();
    let Some(path) = status
        .facts
        .as_ref()
        .and_then(|f| f.profile.rec_path.clone())
    else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "OBS is not connected or did not report a recording folder",
        ));
    };
    if app.inner.lock().current_job.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the clip folder cannot change while a share is running",
        ));
    }
    let patch = json!({ "clipDir": path });
    let next = app
        .settings()
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    let disk = Settings::load_or_create(&app.settings_path).map_err(ApiError::internal)?;
    let disk_next = disk
        .with_patch(&patch)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, e))?;
    disk_next
        .save(&app.settings_path)
        .map_err(ApiError::internal)?;
    app.apply_settings(next)
        .await
        .map_err(|e| ApiError::internal(format!("{e:#}")))?;
    tracing::info!("clip folder adopted from OBS: {path}");
    Ok(Json(json!({ "ok": true, "clipDir": path })))
}

/// `POST /api/obs/refresh` (since 2.2): read the facts again now.
pub async fn obs_refresh(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    if !app.obs.status().connected {
        return Err(ApiError::new(StatusCode::CONFLICT, "OBS is not connected"));
    }
    crate::obs_ws::refresh_basics(&app.obs).await;
    let facts = crate::obs_status::read_facts(&app.obs).await;
    app.obs.set_facts(facts);
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------ updates (since 2.3)

fn update_document(app: &AppState) -> Value {
    let mut doc = serde_json::to_value(&*app.update.lock()).unwrap_or(Value::Null);
    doc["current"] = json!(VERSION);
    doc["installed"] = json!(crate::update::is_installed_copy());
    doc["fromPackage"] = json!(crate::update::is_package_copy());
    doc["checkUpdates"] = json!(app.settings().check_updates);
    doc
}

/// `GET /api/update`
pub async fn update_status(State(app): State<App>) -> Json<Value> {
    Json(update_document(&app))
}

/// `POST /api/update/check`: ask GitHub now.
pub async fn update_check(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    crate::update::check(&app).await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("update check failed: {e:#}"),
        )
    })?;
    let mut doc = update_document(&app);
    doc["ok"] = json!(true);
    Ok(Json(doc))
}

/// `POST /api/update/download`: fetch and verify the latest release.
pub async fn update_download(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    {
        let u = app.update.lock();
        if u.latest.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "no update is available",
            ));
        }
        if matches!(
            u.phase,
            crate::update::Phase::Downloading | crate::update::Phase::Installing
        ) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "an update is already in progress",
            ));
        }
    }
    let state = app.clone();
    tokio::spawn(async move {
        let _ = crate::update::download(state, true).await;
    });
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/update/install`: apply the verified package and restart.
pub async fn update_install(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    crate::update::install(&app)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, format!("{e:#}")))?;
    Ok(Json(json!({ "ok": true })))
}

/// `POST /api/update/seen`: the UI showed "What's new".
pub async fn update_seen(State(app): State<App>) -> Json<Value> {
    let mut u = app.update.lock();
    u.just_updated = false;
    u.updated_notes = None;
    u.updated_url = None;
    Json(json!({ "ok": true }))
}

/// `POST /api/scanning { paused }`: pause or resume the folder scan
/// (RAM only; the tray has the same switch).
pub async fn scanning(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let Some(paused) = v["paused"].as_bool() else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "paused must be true or false",
        ));
    };
    app.set_scanning_paused(paused);
    Ok(Json(json!({ "ok": true, "paused": paused })))
}

// ------------------------------------------------------------ OAuth (since 2.5)

fn oauth_document(app: &AppState, p: &crate::oauth::Provider) -> Value {
    let cred = credentials::read(p.credential).ok().flatten();
    let flow = app.oauth.lock().get(p.id).cloned();
    json!({
        "provider": p.id,
        "label": p.label,
        "configured": !p.client_id.is_empty(),
        "connected": cred.is_some(),
        "account": cred.map(|c| c.user),
        // since 2.6: the provider connects through the browser on this PC
        "loopback": p.loopback,
        // since 3.3: this build carries a client of its own for the provider,
        // whether or not it is the one in use
        "builtIn": p.built_in,
        "flow": flow.map(|f| {
            let mut v = serde_json::to_value(&f).unwrap_or(Value::Null);
            v["expiresIn"] = json!(f.expires_at.saturating_duration_since(std::time::Instant::now()).as_secs());
            v
        }),
    })
}

fn oauth_provider(app: &AppState, id: &str) -> Result<crate::oauth::Provider, ApiError> {
    crate::oauth::provider(id, &app.settings())
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("unknown provider: {id}")))
}

/// Who the account is, once the tokens are there: per provider.
fn account_lookup(p: &crate::oauth::Provider) -> crate::oauth::AccountLookup {
    let id = p.id;
    let api = match id {
        "youtube" => crate::youtube::api_base(),
        _ => crate::onedrive::graph_base(),
    };
    Box::new(move |token| {
        let api = api.clone();
        Box::pin(async move {
            match id {
                "youtube" => crate::youtube::YouTube::channel_title(&api, &token).await,
                _ => crate::onedrive::OneDrive::me(&api, &token).await,
            }
        })
    })
}

/// `GET /api/oauth/<provider>`
pub async fn oauth_status(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = oauth_provider(&app, &id)?;
    Ok(Json(oauth_document(&app, &p)))
}

fn flow_running(app: &AppState, id: &str) -> bool {
    app.oauth.lock().get(id).is_some_and(|f| {
        matches!(f.status, crate::oauth::FlowStatus::Pending)
            && f.expires_at > std::time::Instant::now()
    })
}

/// `POST /api/oauth/<provider>/start`: begin a device flow (or report the
/// one that is running); the page polls `GET` for the code and the outcome.
pub async fn oauth_start(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = oauth_provider(&app, &id)?;
    if p.client_id.is_empty() {
        return Err(ApiError::new(StatusCode::CONFLICT, p.missing_client));
    }
    if p.loopback {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "{} connects through the browser on this PC - use /loopback",
                p.label
            ),
        ));
    }
    if !flow_running(&app, p.id) {
        // the code comes from the provider first; wait for it so the answer carries it
        let start = crate::oauth::device_start(&p)
            .await
            .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
        let state = app.clone();
        let provider = p.clone();
        let lookup = account_lookup(&p);
        tokio::spawn(async move {
            let _ = crate::oauth::run_started_flow(state, provider, start, lookup).await;
        });
        // give the task a moment to register the flow
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let mut doc = oauth_document(&app, &p);
    doc["ok"] = json!(true);
    Ok(Json(doc))
}

/// `POST /api/oauth/<provider>/loopback` (since 2.6): begin a browser login.
/// Answers the document plus `url`; the page opens it on this PC, the
/// provider sends the browser back to `/oauth/<provider>/callback`.
pub async fn oauth_loopback(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = oauth_provider(&app, &id)?;
    if p.client_id.is_empty() {
        return Err(ApiError::new(StatusCode::CONFLICT, p.missing_client));
    }
    if !p.loopback {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("{} connects with a code - use /start", p.label),
        ));
    }
    let base = crate::oauth::callback_base(&app, p.id)
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
    let redirect_uri = format!("{base}/oauth/{}/callback", p.id);
    let url = crate::oauth::start_loopback_flow(&app, &p, &redirect_uri)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, format!("{e:#}")))?;
    let mut doc = oauth_document(&app, &p);
    doc["ok"] = json!(true);
    doc["url"] = json!(url);
    Ok(Json(doc))
}

/// `GET /oauth/<provider>/callback?code&state` (since 2.6): the provider
/// sends the browser back here. Answers a small page for that tab; the
/// settings card learns the outcome through `GET /api/oauth/<provider>`.
pub async fn oauth_callback(
    State(app): State<App>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let page = |status: StatusCode, title: &str, text: &str| {
        let body = format!(
            "<!doctype html><meta charset=\"utf-8\"><title>replaycut - {title}</title>\
             <body style=\"font-family:system-ui,sans-serif;background:#141519;color:#e8e8ec;\
             display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
             <div style=\"max-width:32rem;padding:2rem\"><h1 style=\"font-size:1.4rem\">{title}</h1>\
             <p>{}</p><p style=\"color:#9a9aa6\">You can close this tab and go back to replaycut.</p></div>",
            html_escape(text)
        );
        (
            status,
            [
                (CONTENT_TYPE, "text/html; charset=utf-8"),
                (CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response()
    };
    let p = match oauth_provider(&app, &id) {
        Ok(p) => p,
        Err(e) => return page(StatusCode::NOT_FOUND, "Unknown provider", &e.message),
    };
    if let Some(err) = q.get("error") {
        let text = format!(
            "{err}: {}",
            q.get("error_description").map(String::as_str).unwrap_or("")
        );
        if let Some(f) = app.oauth.lock().get_mut(p.id) {
            f.status = crate::oauth::FlowStatus::Failed {
                error: text.clone(),
            };
            f.loopback = None;
        }
        tracing::warn!("{}: browser login refused: {text}", p.label);
        return page(StatusCode::BAD_REQUEST, "Not connected", &text);
    }
    let (code, state) = (
        q.get("code").map(String::as_str).unwrap_or(""),
        q.get("state").map(String::as_str).unwrap_or(""),
    );
    if code.is_empty() || state.is_empty() {
        return page(
            StatusCode::BAD_REQUEST,
            "Not connected",
            "the answer carries no code and state",
        );
    }
    let lookup = account_lookup(&p);
    match crate::oauth::finish_loopback_flow(&app, &p, code, state, lookup).await {
        Ok(account) => page(
            StatusCode::OK,
            &format!("{} connected", p.label),
            &format!("Connected as {account}."),
        ),
        Err(e) => page(StatusCode::BAD_REQUEST, "Not connected", &format!("{e:#}")),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `POST /api/oauth/<provider>/disconnect`: forget the account.
pub async fn oauth_disconnect(
    State(app): State<App>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let p = oauth_provider(&app, &id)?;
    credentials::delete(p.credential).map_err(ApiError::internal)?;
    app.oauth.lock().remove(p.id);
    if let Err(e) = app.rebuild_runtime().await {
        tracing::warn!("rebuild after {} disconnect: {e:#}", p.label);
    }
    app.tray_changed();
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------ Telegram and webhook tests (since 2.6)

/// `POST /api/test/telegram`: `getMe` plus a test message. `token` and
/// `chatId` from the body override the stored ones.
pub async fn test_telegram(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let s = app.settings().integrations.telegram.clone();
    let chat_id = v["chatId"]
        .as_str()
        .filter(|c| !c.trim().is_empty())
        .unwrap_or(&s.chat_id)
        .to_string();
    let token = match v["token"].as_str().filter(|t| !t.trim().is_empty()) {
        Some(t) => t.to_string(),
        None => match credentials::read(credentials::TELEGRAM) {
            Ok(Some(c)) => c.secret,
            _ => {
                return Ok(Json(
                    json!({ "ok": false, "error": "no bot token stored - paste the token from @BotFather" }),
                ))
            }
        },
    };
    let started = std::time::Instant::now();
    let result = async {
        let t = crate::notify::Telegram::new(&token, &chat_id)?;
        let bot = t.me().await?;
        let posted = t
            .send("<b>replaycut</b> test message - if you can read this, the bot and the chat id work")
            .await?;
        Ok::<(String, String), anyhow::Error>((bot, posted))
    }
    .await;
    Ok(Json(match result {
        Ok((bot, posted)) => {
            json!({ "ok": true, "bot": bot, "posted": posted, "ms": started.elapsed().as_millis() as u64 })
        }
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

/// `POST /api/test/webhook`: a signed `test` event. `url` and `secret`
/// from the body override the stored ones.
pub async fn test_webhook(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let s = app.settings().integrations.webhook.clone();
    let url = v["url"]
        .as_str()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or(&s.url)
        .to_string();
    let secret = match v["secret"].as_str() {
        Some(sec) if !sec.is_empty() => Some(sec.to_string()),
        _ => credentials::read(credentials::WEBHOOK_SECRET)
            .ok()
            .flatten()
            .map(|c| c.secret),
    };
    let signed = secret.as_ref().is_some_and(|s| !s.is_empty());
    let started = std::time::Instant::now();
    let result = async {
        let w = crate::notify::Webhook::new(&url, secret)?;
        w.test().await
    }
    .await;
    Ok(Json(match result {
        Ok(status) => {
            json!({ "ok": true, "status": status, "signed": signed, "ms": started.elapsed().as_millis() as u64 })
        }
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

// ------------------------------------------------------------ S3 and WebDAV tests (since 2.5)

/// `POST /api/test/s3`: HEAD the bucket, PUT and DELETE a probe. Fields from
/// the body override the settings; keys from the body or the credential.
pub async fn test_s3(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let s = app.settings().integrations.s3.clone();
    let pick = |k: &str, cur: &str| v[k].as_str().unwrap_or(cur).to_string();
    let (ak, sk) = match (v["accessKey"].as_str(), v["secretKey"].as_str()) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (a.to_string(), b.to_string()),
        _ => match credentials::read(credentials::S3) {
            Ok(Some(c)) => (c.user, c.secret),
            _ => {
                return Ok(Json(
                    json!({ "ok": false, "error": "no S3 keys stored - enter access key and secret key" }),
                ))
            }
        },
    };
    let started = std::time::Instant::now();
    let result = async {
        let s3 = crate::s3::S3::new(
            &pick("endpoint", &s.endpoint),
            &pick("region", &s.region),
            &pick("bucket", &s.bucket),
            &pick("prefix", &s.prefix),
            &pick("publicBase", &s.public_base),
            v["presignDays"]
                .as_u64()
                .map(|d| d as u32)
                .unwrap_or(s.presign_days),
            ak,
            sk,
        )?;
        s3.probe().await?;
        Ok::<String, anyhow::Error>(s3.describe_link_mode())
    }
    .await;
    Ok(Json(match result {
        Ok(mode) => {
            json!({ "ok": true, "links": mode, "ms": started.elapsed().as_millis() as u64 })
        }
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

/// `POST /api/test/webdav`: PROPFIND the root, PUT and DELETE a probe.
pub async fn test_webdav(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let v = parse_json(&headers, &body)?;
    let s = app.settings().integrations.webdav.clone();
    let pick = |k: &str, cur: &str| v[k].as_str().unwrap_or(cur).to_string();
    let (user, pw) = match (v["user"].as_str(), v["password"].as_str()) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => (a.to_string(), b.to_string()),
        _ => match credentials::read(credentials::WEBDAV) {
            Ok(Some(c)) => (c.user, c.secret),
            _ => {
                return Ok(Json(
                    json!({ "ok": false, "error": "no WebDAV login stored - enter user and password" }),
                ))
            }
        },
    };
    let started = std::time::Instant::now();
    let result = async {
        let d = crate::dav::WebDav::new(
            &pick("url", &s.url),
            &pick("folder", &s.folder),
            &pick("publicBase", &s.public_base),
            &user,
            &pw,
        )?;
        d.probe().await
    }
    .await;
    Ok(Json(match result {
        Ok(()) => json!({ "ok": true, "user": user, "ms": started.elapsed().as_millis() as u64 }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}
