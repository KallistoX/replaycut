//! Access control: the optional password (argon2id hash in settings.json),
//! browser sessions (`rc_session` cookie, token hashes in sessions.json),
//! the login throttle and the Origin check.
//!
//! Rules from docs/api.md: loopback never needs a login; with a password
//! set, other clients need a valid session for `/api/*` and `/media/*`;
//! every non-GET request whose `Origin` does not match `Host` is refused.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::header::{CONTENT_TYPE, COOKIE, HOST, ORIGIN};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::http::ApiError;
use crate::state::AppState;
use crate::util;

pub const COOKIE_NAME: &str = "rc_session";
pub const SESSION_DAYS: u64 = 30;
const MAX_FAILURES: u32 = 10;
const LOCKOUT: Duration = Duration::from_secs(60);
const FAILURE_DELAY: Duration = Duration::from_secs(1);

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("cannot hash the password: {e}"))
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

pub fn is_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Session {
    /// SHA-256 of the token, hex.
    hash: String,
    created: String,
    /// Unix seconds.
    expires: u64,
}

struct Attempts {
    failures: u32,
    last: Instant,
}

/// Browser sessions plus the login throttle.
pub struct Sessions {
    file: PathBuf,
    list: Mutex<Vec<Session>>,
    attempts: Mutex<HashMap<IpAddr, Attempts>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Sessions {
    pub fn load(file: &Path) -> Self {
        let mut list: Vec<Session> = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let now = now_unix();
        list.retain(|s| s.expires > now);
        Self {
            file: file.to_path_buf(),
            list: Mutex::new(list),
            attempts: Mutex::new(HashMap::new()),
        }
    }

    fn save(&self, list: &[Session]) {
        let text = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".into());
        if let Err(e) = util::write_atomic(&self.file, text.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.file.display());
        }
    }

    /// A new session; returns the token for the cookie.
    pub fn create(&self) -> String {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let now = now_unix();
        let mut list = self.list.lock();
        list.retain(|s| s.expires > now);
        list.push(Session {
            hash: token_hash(&token),
            created: util::now_local(),
            expires: now + SESSION_DAYS * 86_400,
        });
        self.save(&list);
        token
    }

    pub fn is_valid(&self, token: &str) -> bool {
        let hash = token_hash(token);
        let now = now_unix();
        self.list
            .lock()
            .iter()
            .any(|s| s.hash == hash && s.expires > now)
    }

    pub fn remove(&self, token: &str) {
        let hash = token_hash(token);
        let mut list = self.list.lock();
        list.retain(|s| s.hash != hash);
        self.save(&list);
    }

    pub fn clear(&self) {
        let mut list = self.list.lock();
        list.clear();
        self.save(&list);
    }

    /// Before checking a password: `Err(seconds)` while the client is locked out.
    pub fn check_lockout(&self, ip: IpAddr) -> Result<(), u64> {
        let attempts = self.attempts.lock();
        if let Some(a) = attempts.get(&ip) {
            if a.failures >= MAX_FAILURES {
                let since = a.last.elapsed();
                if since < LOCKOUT {
                    return Err((LOCKOUT - since).as_secs().max(1));
                }
            }
        }
        Ok(())
    }

    pub fn record_failure(&self, ip: IpAddr) {
        let mut attempts = self.attempts.lock();
        let entry = attempts.entry(ip).or_insert(Attempts {
            failures: 0,
            last: Instant::now(),
        });
        if entry.failures >= MAX_FAILURES && entry.last.elapsed() >= LOCKOUT {
            entry.failures = 0;
        }
        entry.failures += 1;
        entry.last = Instant::now();
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.attempts.lock().remove(&ip);
    }

    pub fn failure_delay() -> Duration {
        FAILURE_DELAY
    }
}

/// The session token from the request's cookies, if any.
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == COOKIE_NAME).then(|| v.trim().to_string())
    })
}

pub fn set_cookie_value(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_DAYS * 86_400
    )
}

pub fn clear_cookie_value() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// The host part of an Origin header (`http://host:port` -> `host:port`).
fn origin_host(origin: &str) -> Option<&str> {
    let rest = origin.split_once("://")?.1;
    Some(rest.trim_end_matches('/'))
}

/// True when the request may proceed: no Origin header, or one that names
/// the same host and port as the Host header.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let (Some(origin_host), Some(host)) = (origin_host(origin), host) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(host.trim())
}

/// Refuse cross-site writes: any method but GET/HEAD/OPTIONS whose Origin
/// does not match Host.
pub async fn origin_check(req: Request, next: Next) -> Response {
    if !matches!(
        req.method(),
        &Method::GET | &Method::HEAD | &Method::OPTIONS
    ) {
        let origin = req.headers().get(ORIGIN).and_then(|v| v.to_str().ok());
        let host = req.headers().get(HOST).and_then(|v| v.to_str().ok());
        if !origin_allowed(origin, host) {
            tracing::warn!(
                "refused {} {} from origin {:?} (host {:?})",
                req.method(),
                req.uri().path(),
                origin,
                host
            );
            return (
                StatusCode::FORBIDDEN,
                Json(json!({ "ok": false, "error": "cross-site request refused" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// With a password set, `/api/*` and `/media/*` need a session unless the
/// client is this machine. Pages, themes, the session probe and the login
/// itself stay open.
pub async fn guard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let protected = path.starts_with("/api/") || path.starts_with("/media/");
    let open = matches!(path, "/api/session" | "/api/login" | "/api/logout");
    if protected && !open && !is_loopback(&addr) && state.password_set() {
        let ok = cookie_token(req.headers()).is_some_and(|t| state.sessions.is_valid(&t));
        if !ok {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "ok": false, "error": "login required" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// JSON endpoints of 2.1 insist on `Content-Type: application/json`.
pub fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    let ok = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            ct.trim_start()
                .to_ascii_lowercase()
                .starts_with("application/json")
        });
    if ok {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "send Content-Type: application/json",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_round_trip() {
        let hash = hash_password("hunter2").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(&hash, "hunter2"));
        assert!(!verify_password(&hash, "hunter3"));
        assert!(!verify_password("garbage", "hunter2"));
    }

    #[test]
    fn origin_rules() {
        assert!(origin_allowed(None, Some("localhost:8420")));
        assert!(origin_allowed(
            Some("http://localhost:8420"),
            Some("localhost:8420")
        ));
        assert!(origin_allowed(
            Some("http://Gaming-PC:8420"),
            Some("gaming-pc:8420")
        ));
        assert!(!origin_allowed(
            Some("http://localhost:8421"),
            Some("localhost:8420")
        ));
        assert!(!origin_allowed(
            Some("http://evil.example"),
            Some("localhost:8420")
        ));
        assert!(!origin_allowed(Some("null"), Some("localhost:8420")));
        assert!(!origin_allowed(Some("http://localhost:8420"), None));
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback(&"127.0.0.1:1".parse().unwrap()));
        assert!(is_loopback(&"127.8.8.8:1".parse().unwrap()));
        assert!(is_loopback(&"[::1]:1".parse().unwrap()));
        assert!(is_loopback(&"[::ffff:127.0.0.1]:1".parse().unwrap()));
        assert!(!is_loopback(&"192.0.2.20:1".parse().unwrap()));
    }

    #[test]
    fn cookies_and_sessions() {
        let dir = std::env::temp_dir().join(format!("rc-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sessions.json");
        let sessions = Sessions::load(&file);
        let token = sessions.create();
        assert_eq!(token.len(), 64);
        assert!(sessions.is_valid(&token));
        assert!(!sessions.is_valid("nope"));

        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            format!("other=1; {COOKIE_NAME}={token}; x=y")
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some(token.as_str()));

        // survives a reload
        let again = Sessions::load(&file);
        assert!(again.is_valid(&token));
        again.remove(&token);
        assert!(!again.is_valid(&token));
        assert!(!Sessions::load(&file).is_valid(&token));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lockout_after_ten_failures() {
        let dir = std::env::temp_dir().join(format!("rc-lockout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = Sessions::load(&dir.join("s.json"));
        let ip: IpAddr = "192.0.2.5".parse().unwrap();
        for _ in 0..9 {
            sessions.record_failure(ip);
            assert!(sessions.check_lockout(ip).is_ok());
        }
        sessions.record_failure(ip);
        assert!(sessions.check_lockout(ip).is_err());
        sessions.record_success(ip);
        assert!(sessions.check_lockout(ip).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
