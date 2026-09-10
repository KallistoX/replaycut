//! Access control: the optional password (argon2id hash in settings.json),
//! browser sessions (`rc_session` cookie, token hashes in sessions.json),
//! the login throttle, the Origin check and the Host check.
//!
//! Rules from docs/api.md: loopback never needs a login; with a password
//! set, other clients need a valid session for `/api/*` and `/media/*`;
//! every non-GET request whose `Origin` does not match `Host` is refused;
//! since 2.8 a request whose `Host` names neither this machine nor an
//! allowed name is refused with 421.

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
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, HOST, ORIGIN};
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
/// The device cookie of 3.5: a random id that says which device a sign-in
/// belongs to. It is not a credential and never stands in for one - it only
/// keeps one phone to one row in the device list, whichever way it signs in.
pub const DEVICE_COOKIE: &str = "rc_device";
/// It outlives the session on purpose: a device that signs in again months
/// later is still the same device.
pub const DEVICE_DAYS: u64 = 365;
const MAX_FAILURES: u32 = 10;
const LOCKOUT: Duration = Duration::from_secs(60);
const FAILURE_DELAY: Duration = Duration::from_secs(1);
/// The password rules of 2.8. Four generated words are 27 characters.
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 128;
/// Beyond the per-address throttle: this many failed logins from all
/// addresses together within [`GLOBAL_WINDOW`] pause the password login
/// for [`GLOBAL_LOCKOUT`] (since 2.8). The device login keeps working.
const GLOBAL_MAX_FAILURES: usize = 30;
const GLOBAL_WINDOW: Duration = Duration::from_secs(300);
const GLOBAL_LOCKOUT: Duration = Duration::from_secs(600);
/// `lastSeen` is written at most this often per session (since 2.8).
const TOUCH_EVERY: Duration = Duration::from_secs(60);

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

/// How a session came to be (since 2.8).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// The password on the login page. Sessions from before 2.8 are these.
    #[default]
    Password,
    /// Approved on this PC after the device asked (since 2.8).
    Approve,
    /// A scanned QR code (since 2.8).
    Qr,
}

/// What kind of client holds a session (since 3.4). A browser gets a cookie
/// and nothing else; a client of our own gets the token as a value it stores
/// itself and sends as `Authorization: Bearer`. Keeping them apart is what
/// lets the cookie stay `HttpOnly`: a token in a JSON body that page scripts
/// could read would give that up for everyone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Client {
    #[default]
    Browser,
    Native,
}

impl Client {
    /// What `POST /api/login` and `POST /api/pair/request` accept in
    /// `client`. Anything else is a 400 rather than a silent browser.
    pub fn parse(raw: Option<&str>) -> Result<Self, &'static str> {
        match raw.unwrap_or("browser").trim() {
            "browser" => Ok(Self::Browser),
            "native" => Ok(Self::Native),
            _ => Err("client must be \"browser\" or \"native\""),
        }
    }

    pub fn is_native(self) -> bool {
        self == Self::Native
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Session {
    /// Handle for the device list and for revoking (since 2.8); random,
    /// not a secret.
    pub id: String,
    /// SHA-256 of the token, hex.
    hash: String,
    /// What the device calls itself, "iPhone, Safari" (since 2.8).
    pub name: String,
    /// The `User-Agent` as it arrived (since 2.8).
    pub agent: String,
    /// The address the session was created from (since 2.8).
    pub ip: String,
    pub created: String,
    /// Local timestamp of the last request, at most one write a minute
    /// (since 2.8).
    pub last_seen: String,
    pub via: Via,
    /// Browser or a client of our own (since 3.4).
    pub client: Client,
    /// Which device this session belongs to (since 3.5). Empty for a session
    /// from before 3.5 and for a client of our own, which keeps no cookies.
    pub device: String,
    /// Unix seconds.
    expires: u64,
}

/// What a new session records about the device it belongs to (since 2.8).
#[derive(Debug, Clone)]
pub struct NewSession {
    pub name: String,
    pub agent: String,
    pub ip: String,
    pub via: Via,
    pub client: Client,
    /// The device id from the `rc_device` cookie (since 3.5), empty when
    /// the device does not carry one.
    pub device: String,
}

impl NewSession {
    /// A password login: the name is guessed from the `User-Agent`.
    pub fn from_agent(agent: &str, ip: IpAddr, via: Via) -> Self {
        Self {
            name: device_name(agent),
            agent: agent.chars().take(200).collect(),
            ip: ip.to_string(),
            via,
            client: Client::Browser,
            device: String::new(),
        }
    }

    /// Which device signed in (since 3.5): a login from a device that has a
    /// session renews that one instead of adding a second row.
    pub fn on_device(mut self, device: &str) -> Self {
        self.device = device.to_string();
        self
    }

    /// The same session, held by a client of our own instead of a browser.
    pub fn held_by(mut self, client: Client) -> Self {
        self.client = client;
        self
    }
}

/// A short name for a browser, "iPhone, Safari" (since 2.8). The device
/// may replace it when it asks for access; this is the suggestion and what
/// a password login is filed under.
pub fn device_name(agent: &str) -> String {
    if agent.trim().is_empty() {
        return "Unknown device".into();
    }
    let system = if agent.contains("iPhone") {
        "iPhone"
    } else if agent.contains("iPad") {
        "iPad"
    } else if agent.contains("Android") {
        "Android"
    } else if agent.contains("Windows") {
        "Windows PC"
    } else if agent.contains("Macintosh") || agent.contains("Mac OS") {
        "Mac"
    } else if agent.contains("Linux") {
        "Linux"
    } else {
        "Device"
    };
    // Order matters: Edge and Opera also call themselves Chrome, and
    // every Chrome also claims Safari.
    let browser = if agent.contains("Edg/") {
        "Edge"
    } else if agent.contains("OPR/") || agent.contains("Opera") {
        "Opera"
    } else if agent.contains("Firefox/") {
        "Firefox"
    } else if agent.contains("Chrome/") || agent.contains("CriOS/") {
        "Chrome"
    } else if agent.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };
    format!("{system}, {browser}")
}

struct Attempts {
    failures: u32,
    last: Instant,
}

/// The throttle over all addresses (since 2.8).
#[derive(Default)]
struct Global {
    /// Failed logins inside the window, oldest first.
    recent: Vec<Instant>,
    locked_until: Option<Instant>,
}

/// Browser sessions plus the login throttle.
pub struct Sessions {
    file: PathBuf,
    list: Mutex<Vec<Session>>,
    attempts: Mutex<HashMap<IpAddr, Attempts>>,
    global: Mutex<Global>,
    /// When `lastSeen` of a session (by token hash) was last written.
    touched: Mutex<HashMap<String, Instant>>,
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

/// `n` bytes from the OS random source as hex.
pub fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Sessions {
    pub fn load(file: &Path) -> Self {
        let mut list: Vec<Session> = std::fs::read_to_string(file)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let now = now_unix();
        list.retain(|s| s.expires > now);
        // Sessions written before 2.8 only knew the token hash: give them
        // an id and say what little is known about the device.
        let mut migrated = false;
        for s in &mut list {
            if s.id.is_empty() {
                s.id = random_hex(8);
                migrated = true;
            }
            if s.name.is_empty() {
                s.name = "Unknown device".into();
                migrated = true;
            }
            if s.last_seen.is_empty() {
                s.last_seen.clone_from(&s.created);
                migrated = true;
            }
        }
        let sessions = Self {
            file: file.to_path_buf(),
            list: Mutex::new(list),
            attempts: Mutex::new(HashMap::new()),
            global: Mutex::new(Global::default()),
            touched: Mutex::new(HashMap::new()),
        };
        if migrated {
            tracing::info!("sessions.json: filled in the device fields of 2.8");
            let list = sessions.list.lock();
            sessions.save(&list);
        }
        sessions
    }

    fn save(&self, list: &[Session]) {
        let text = serde_json::to_string_pretty(list).unwrap_or_else(|_| "[]".into());
        if let Err(e) = util::write_atomic(&self.file, text.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.file.display());
        }
    }

    /// A session for a device; returns the token for the cookie. A device
    /// that already has one keeps its row (since 3.5): the token is replaced,
    /// `via` says the newest way in and `created` stays the first sign-in, so
    /// one phone is one line however often it signs in and however.
    pub fn create(&self, new: NewSession) -> String {
        let token = random_hex(32);
        let now = now_unix();
        let stamp = util::now_local();
        let mut list = self.list.lock();
        list.retain(|s| s.expires > now);
        let known = (!new.device.is_empty())
            .then(|| list.iter().position(|s| s.device == new.device))
            .flatten();
        if let Some(i) = known {
            let old = std::mem::replace(&mut list[i].hash, token_hash(&token));
            let session = &mut list[i];
            session.name = new.name;
            session.agent = new.agent;
            session.ip = new.ip;
            session.last_seen = stamp;
            session.via = new.via;
            session.client = new.client;
            session.expires = now + SESSION_DAYS * 86_400;
            self.save(&list);
            self.touched.lock().remove(&old);
            return token;
        }
        list.push(Session {
            id: random_hex(8),
            hash: token_hash(&token),
            name: new.name,
            agent: new.agent,
            ip: new.ip,
            created: stamp.clone(),
            last_seen: stamp,
            via: new.via,
            client: new.client,
            device: new.device,
            expires: now + SESSION_DAYS * 86_400,
        });
        self.save(&list);
        token
    }

    pub fn is_valid(&self, token: &str) -> bool {
        let hash = token_hash(token);
        let now = now_unix();
        let valid = self
            .list
            .lock()
            .iter()
            .any(|s| s.hash == hash && s.expires > now);
        if valid {
            self.touch(&hash);
        }
        valid
    }

    /// Note that the session was used. The file is written at most once a
    /// minute per session, so a polling page does not keep the disk busy.
    fn touch(&self, hash: &str) {
        {
            let mut touched = self.touched.lock();
            if let Some(last) = touched.get(hash) {
                if last.elapsed() < TOUCH_EVERY {
                    return;
                }
            }
            touched.insert(hash.to_string(), Instant::now());
        }
        let stamp = util::now_local();
        let mut list = self.list.lock();
        let Some(session) = list.iter_mut().find(|s| s.hash == hash) else {
            return;
        };
        session.last_seen = stamp;
        self.save(&list);
    }

    /// What the device list folds sessions by (since 3.5): the device id
    /// where there is one, otherwise the `User-Agent` and the address, which
    /// is all a session from before 3.5 knows about its device.
    fn device_key(s: &Session) -> String {
        if s.device.is_empty() {
            format!("agent\n{}\n{}", s.agent, s.ip)
        } else {
            format!("device\n{}", s.device)
        }
    }

    /// The device list of `GET /api/sessions` (since 2.8), newest first.
    /// `current` is the caller's token, so its own row can say so.
    ///
    /// One row is one device (since 3.5). Sessions that carry the same
    /// device id are one row already; the ones from before 3.5 carry none,
    /// so rows that share the `User-Agent` and the address are folded into
    /// one - a guess, but one that only ever joins rows and never drops a
    /// session. `sessions` says how many are behind a row, `created` is the
    /// first sign-in of the device and `via` the newest way in.
    pub fn list(&self, current: Option<&str>) -> Vec<serde_json::Value> {
        let now = now_unix();
        let mine = current.map(token_hash);
        let mut list: Vec<Session> = self
            .list
            .lock()
            .iter()
            .filter(|s| s.expires > now)
            .cloned()
            .collect();
        list.sort_by(|a, b| b.created.cmp(&a.created));
        let mut keys: Vec<String> = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for s in &list {
            let is_mine = mine.as_deref() == Some(s.hash.as_str());
            if let Some(i) = keys.iter().position(|k| *k == Self::device_key(s)) {
                let row = &mut rows[i];
                // the list is newest first, so this one is the older sign-in
                row["created"] = json!(s.created);
                if s.last_seen.as_str() > row["lastSeen"].as_str().unwrap_or("") {
                    row["lastSeen"] = json!(s.last_seen);
                }
                if is_mine {
                    row["current"] = json!(true);
                }
                row["sessions"] = json!(row["sessions"].as_u64().unwrap_or(1) + 1);
                continue;
            }
            keys.push(Self::device_key(s));
            rows.push(json!({
                "id": s.id,
                "name": s.name,
                "agent": s.agent,
                "ip": s.ip,
                "created": s.created,
                "lastSeen": s.last_seen,
                "via": s.via,
                // since 3.4
                "client": s.client,
                "current": is_mine,
                // since 3.5
                "device": s.device,
                "sessions": 1,
            }));
        }
        rows
    }

    /// Sign a device out by the id of its row (since 2.8); the name for the
    /// log. A row stands for a device, so every session behind it goes
    /// (since 3.5) - for a device from before 3.5 that is every session with
    /// the same `User-Agent` and address.
    pub fn revoke(&self, id: &str) -> Option<String> {
        let mut list = self.list.lock();
        let i = list.iter().position(|s| s.id == id)?;
        let gone = list[i].clone();
        let key = Self::device_key(&gone);
        let mut hashes: Vec<String> = Vec::new();
        list.retain(|s| {
            let same = Self::device_key(s) == key;
            if same {
                hashes.push(s.hash.clone());
            }
            !same
        });
        self.save(&list);
        let mut touched = self.touched.lock();
        for hash in &hashes {
            touched.remove(hash);
        }
        Some(gone.name)
    }

    /// "Sign out everywhere" (since 2.8): everyone but the caller.
    pub fn clear_except(&self, current: Option<&str>) -> usize {
        let keep = current.map(token_hash);
        let mut list = self.list.lock();
        let before = list.len();
        list.retain(|s| keep.as_deref() == Some(s.hash.as_str()));
        let removed = before - list.len();
        if removed > 0 {
            self.save(&list);
            self.touched.lock().clear();
        }
        removed
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

    /// Before checking a password: `Err(seconds)` while the password login
    /// is paused for everyone (since 2.8). The device login is not.
    pub fn check_global(&self) -> Result<(), u64> {
        let mut global = self.global.lock();
        match global.locked_until {
            Some(until) if until > Instant::now() => Err((until - Instant::now()).as_secs().max(1)),
            Some(_) => {
                global.locked_until = None;
                global.recent.clear();
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub fn record_failure(&self, ip: IpAddr) {
        {
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
        let mut global = self.global.lock();
        global.recent.retain(|t| t.elapsed() < GLOBAL_WINDOW);
        global.recent.push(Instant::now());
        if global.recent.len() > GLOBAL_MAX_FAILURES && global.locked_until.is_none() {
            global.locked_until = Some(Instant::now() + GLOBAL_LOCKOUT);
            tracing::warn!(
                "{} failed logins within {} s: the password login is paused for {} minutes",
                global.recent.len(),
                GLOBAL_WINDOW.as_secs(),
                GLOBAL_LOCKOUT.as_secs() / 60
            );
        }
    }

    pub fn record_success(&self, ip: IpAddr) {
        self.attempts.lock().remove(&ip);
    }

    pub fn failure_delay() -> Duration {
        FAILURE_DELAY
    }
}

/// One cookie from the request's `Cookie` header.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (k, v) = part.trim().split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// The session token from the request's cookies, if any.
pub fn cookie_token(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, COOKIE_NAME)
}

/// The device id the browser carries (since 3.5), if it is one of ours.
/// It says which device is asking and nothing else: it is never accepted as
/// proof of anything, so a made-up one buys its sender no access.
pub fn device_id(headers: &HeaderMap) -> Option<String> {
    cookie_value(headers, DEVICE_COOKIE).filter(|v| is_device_id(v))
}

fn is_device_id(value: &str) -> bool {
    value.len() == 32 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// A fresh device id for a browser that carries none.
fn new_device_id() -> String {
    random_hex(16)
}

/// Which device a browser request comes from (since 3.5): the id it carries,
/// or a fresh one - and then the answer has to hand it back as a cookie. A
/// client of our own keeps no cookie jar, so it stays without a device id and
/// every one of its sign-ins is its own row.
pub fn device_of(headers: &HeaderMap, client: Client) -> (String, bool) {
    if client.is_native() {
        return (String::new(), false);
    }
    match device_id(headers) {
        Some(id) => (id, false),
        None => (new_device_id(), true),
    }
}

/// `Authorization: Bearer <token>` (since 3.4), for a client that is not a
/// browser and therefore has no cookie jar.
pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// The session token of a request, from wherever it came. Everything that
/// asks "which session is this" goes through here, so a native client is
/// treated exactly like a browser once it is signed in.
pub fn session_token(headers: &HeaderMap) -> Option<String> {
    cookie_token(headers).or_else(|| bearer_token(headers))
}

/// `secure` is added once TLS is running (since 3.4) and never before: a
/// `Secure` cookie handed out over plain HTTP is thrown away by the browser,
/// which would lock everyone out of an installation without HTTPS.
pub fn set_cookie_value(token: &str, secure: bool) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict{}; Max-Age={}",
        if secure { "; Secure" } else { "" },
        SESSION_DAYS * 86_400
    )
}

pub fn clear_cookie_value(secure: bool) -> String {
    format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict{}; Max-Age=0",
        if secure { "; Secure" } else { "" }
    )
}

/// The device cookie (since 3.5). `SameSite=Lax`, not `Strict`: a QR code is
/// opened from a scanner app, which is a navigation from outside the site,
/// and a strict cookie would stay at home for exactly the sign-in that has
/// to recognise the device. It is `HttpOnly` all the same - no page script
/// has any use for it.
pub fn set_device_cookie_value(device: &str, secure: bool) -> String {
    format!(
        "{DEVICE_COOKIE}={device}; Path=/; HttpOnly; SameSite=Lax{}; Max-Age={}",
        if secure { "; Secure" } else { "" },
        DEVICE_DAYS * 86_400
    )
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

/// The name part of a `Host` header: `gaming-pc:8420` -> `gaming-pc`,
/// `[::1]:8420` -> `::1`. Lower case, because names are.
pub fn host_name(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[') {
        return rest
            .split(']')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
    }
    match raw.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => {
            name.to_ascii_lowercase()
        }
        _ => raw.to_ascii_lowercase(),
    }
}

/// Whether a request may name this host (since 2.8): `localhost`, any IP
/// address, this machine's name (also with `.local`) and the names in
/// `allowedHosts` are fine, everything else is not.
///
/// An IP address is always allowed because DNS rebinding - the attack this
/// check is for - needs a name: the browser sends the name the attacker's
/// page used, and a name that is not ours never reaches a handler.
pub fn host_allowed(raw: Option<&str>, hostname: &str, allowed: &[String]) -> bool {
    // A request without a Host carries no name to rebind (and HTTP/1.1
    // clients always send one).
    let Some(raw) = raw else {
        return true;
    };
    let name = host_name(raw);
    if name.is_empty() {
        return false;
    }
    if name == "localhost" || name.ends_with(".localhost") {
        return true;
    }
    if name.parse::<IpAddr>().is_ok() {
        return true;
    }
    let hostname = hostname.to_ascii_lowercase();
    if !hostname.is_empty() && (name == hostname || name == format!("{hostname}.local")) {
        return true;
    }
    allowed
        .iter()
        .any(|a| a.trim().to_ascii_lowercase() == name)
}

/// Refuse a request whose `Host` names something that is not this service
/// (since 2.8): `421 { ok: false, error: "unknown host" }`.
pub async fn host_check(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok());
    if !host_allowed(host, &crate::platform::hostname(), &state.allowed_hosts()) {
        tracing::warn!(
            "refused {} {} for host {:?}",
            req.method(),
            req.uri().path(),
            host
        );
        return (
            StatusCode::MISDIRECTED_REQUEST,
            Json(json!({ "ok": false, "error": "unknown host" })),
        )
            .into_response();
    }
    next.run(req).await
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

/// Whether the client is signed in, or is this PC and does not have to be.
/// Since 2.8 `requireLoginOnLoopback` makes this PC sign in as well, for a
/// Windows account other people use.
pub fn is_authenticated(state: &AppState, addr: &SocketAddr, headers: &HeaderMap) -> bool {
    if session_token(headers).is_some_and(|t| state.sessions.is_valid(&t)) {
        return true;
    }
    if !state.password_set() {
        return true;
    }
    is_loopback(addr) && !state.require_login_on_loopback()
}

/// With a password set, `/api/*` and `/media/*` need a session unless the
/// client is this machine. Pages, themes, the session probe, the login and
/// the device login (since 2.8) stay open.
pub async fn guard(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let protected = path.starts_with("/api/") || path.starts_with("/media/");
    // the device login is the way in for a device that has no session yet;
    // its own handlers decide who may see and answer the requests
    let open = matches!(path, "/api/session" | "/api/login" | "/api/logout")
        || path.starts_with("/api/pair/");
    if protected && !open {
        let ok = is_authenticated(&state, &addr, req.headers());
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
    fn a_session_is_found_by_cookie_or_by_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_token(&headers), None);

        headers.insert(AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers).as_deref(), Some("abc123"));
        assert_eq!(session_token(&headers).as_deref(), Some("abc123"));
        // The scheme is case-insensitive, the value is not.
        headers.insert(AUTHORIZATION, "bearer abc123".parse().unwrap());
        assert_eq!(bearer_token(&headers).as_deref(), Some("abc123"));
        // Anything that is not a bearer token is none of our business.
        headers.insert(AUTHORIZATION, "Basic dXNlcjpwdw==".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
        headers.insert(AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);

        // A cookie wins: a browser that also carries a stale header keeps
        // the session it actually has.
        headers.insert(AUTHORIZATION, "Bearer from-header".parse().unwrap());
        headers.insert(COOKIE, "rc_session=from-cookie".parse().unwrap());
        assert_eq!(session_token(&headers).as_deref(), Some("from-cookie"));
    }

    #[test]
    fn the_client_kind_is_browser_unless_it_says_otherwise() {
        assert_eq!(Client::parse(None).unwrap(), Client::Browser);
        assert_eq!(Client::parse(Some("browser")).unwrap(), Client::Browser);
        assert_eq!(Client::parse(Some("native")).unwrap(), Client::Native);
        assert!(
            Client::parse(Some("app")).is_err(),
            "a typo is not a browser"
        );
    }

    #[test]
    fn the_cookie_is_secure_only_when_tls_is_running() {
        // A `Secure` cookie over plain HTTP is dropped by the browser, which
        // would lock everyone out of an installation without HTTPS.
        let plain = set_cookie_value("t", false);
        assert!(!plain.contains("Secure"), "{plain}");
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        let secure = set_cookie_value("t", true);
        assert!(secure.contains("; Secure"), "{secure}");
        assert!(secure.contains("HttpOnly") && secure.contains("SameSite=Strict"));
        // The cookie that clears the session has to match the one that set
        // it, or the browser keeps the old one alongside.
        assert!(!clear_cookie_value(false).contains("Secure"));
        assert!(clear_cookie_value(true).contains("; Secure"));
    }

    #[test]
    fn cookies_and_sessions() {
        let dir = std::env::temp_dir().join(format!("rc-sessions-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sessions.json");
        let sessions = Sessions::load(&file);
        let token = sessions.create(NewSession::from_agent(
            "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari/605.1.15",
            "192.0.2.7".parse().unwrap(),
            Via::Approve,
        ));
        assert_eq!(token.len(), 64);
        let listed = sessions.list.lock()[0].clone();
        assert_eq!(listed.name, "iPhone, Safari");
        assert_eq!(listed.ip, "192.0.2.7");
        assert_eq!(listed.via, Via::Approve);
        assert_eq!(listed.id.len(), 16);
        assert_eq!(listed.last_seen, listed.created);
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
    fn sessions_of_2_7_keep_working_and_gain_the_new_fields() {
        let dir = std::env::temp_dir().join(format!("rc-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sessions.json");
        // what 2.7 wrote: the token hash, when it was made, when it ends
        let token = "cafe";
        let old = json!([{
            "hash": token_hash(token),
            "created": "2026-09-01T20:15:00",
            "expires": now_unix() + 86_400,
        }]);
        std::fs::write(&file, old.to_string()).unwrap();

        let sessions = Sessions::load(&file);
        let migrated = sessions.list.lock()[0].clone();
        assert_eq!(migrated.id.len(), 16);
        assert_eq!(migrated.name, "Unknown device");
        assert_eq!(migrated.via, Via::Password);
        // nothing is known about the device, so it was last seen when it signed in
        assert_eq!(migrated.last_seen, "2026-09-01T20:15:00");
        assert!(migrated.agent.is_empty() && migrated.ip.is_empty());
        assert!(sessions.is_valid(token), "the old session still opens");
        assert_ne!(
            sessions.list.lock()[0].last_seen,
            migrated.last_seen,
            "using it writes lastSeen"
        );
        // and the file on disk carries them now
        let again = Sessions::load(&file);
        assert_eq!(again.list.lock()[0].id, migrated.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn host_rules() {
        let allowed = ["replay.example".to_string(), "Proxy.Example".to_string()];
        let host = |h: &str| host_allowed(Some(h), "gaming-pc", &allowed);
        // this machine, however it is addressed
        assert!(host("localhost:8420"));
        assert!(host("LOCALHOST"));
        assert!(host("127.0.0.1:8420"));
        assert!(host("[::1]:8420"));
        assert!(host("[::1]"));
        assert!(host("192.168.1.23:8420"));
        assert!(host("gaming-pc:8420"));
        assert!(host("Gaming-PC"));
        assert!(host("gaming-pc.local:8420"));
        // names the settings allow, case-insensitively
        assert!(host("replay.example"));
        assert!(host("proxy.example:8420"));
        // and the ones a rebinding attack would use
        assert!(!host("evil.example"));
        assert!(!host("evil.example:8420"));
        assert!(!host("gaming-pc.evil.example"));
        assert!(!host(""));
        // no Host header at all carries no name to rebind
        assert!(host_allowed(None, "gaming-pc", &allowed));
        assert_eq!(host_name("[fe80::1%25eth0]:8420"), "fe80::1%25eth0");
        assert_eq!(host_name("gaming-pc:8420"), "gaming-pc");
        assert_eq!(host_name("gaming-pc"), "gaming-pc");
    }

    #[test]
    fn thirty_failures_from_anywhere_pause_the_password_login() {
        let dir = std::env::temp_dir().join(format!("rc-global-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sessions = Sessions::load(&dir.join("s.json"));
        // one failure each from thirty addresses: below the limit, and no
        // single address is locked out either
        for i in 0..30u8 {
            sessions.record_failure(IpAddr::from([192, 0, 2, i]));
        }
        assert!(sessions.check_global().is_ok());
        assert!(sessions.check_lockout(IpAddr::from([192, 0, 2, 0])).is_ok());
        sessions.record_failure(IpAddr::from([192, 0, 2, 200]));
        let seconds = sessions.check_global().expect_err("paused");
        assert!(seconds > 500 && seconds <= 600, "{seconds} s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_names_read_the_usual_agents() {
        let name = |a: &str| device_name(a);
        assert_eq!(
            name("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Mobile/15E148 Safari/604.1"),
            "iPhone, Safari"
        );
        assert_eq!(
            name("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36 Edg/141.0.0.0"),
            "Windows PC, Edge"
        );
        assert_eq!(
            name(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:135.0) Gecko/20100101 Firefox/135.0"
            ),
            "Windows PC, Firefox"
        );
        assert_eq!(
            name("Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Mobile Safari/537.36"),
            "Android, Chrome"
        );
        assert_eq!(name(""), "Unknown device");
        assert_eq!(name("curl/8.9.1"), "Device, Browser");
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

    #[test]
    fn one_device_stays_one_row_however_it_signs_in() {
        let dir = std::env::temp_dir().join(format!("rc-device-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sessions.json");
        let sessions = Sessions::load(&file);
        let agent = "Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X) Safari/605.1.15";
        let ip: IpAddr = "192.0.2.7".parse().unwrap();
        let phone = "0123456789abcdef0123456789abcdef";
        let first =
            sessions.create(NewSession::from_agent(agent, ip, Via::Approve).on_device(phone));
        let created = sessions.list.lock()[0].created.clone();

        // the same phone, a second way in: the row it has is renewed
        let second = sessions.create(NewSession::from_agent(agent, ip, Via::Qr).on_device(phone));
        assert_eq!(sessions.list.lock().len(), 1, "one device, one session");
        let row = sessions.list.lock()[0].clone();
        assert_eq!(row.via, Via::Qr, "via says the newest way in");
        assert_eq!(row.created, created, "created stays the first sign-in");
        assert!(sessions.is_valid(&second));
        assert!(
            !sessions.is_valid(&first),
            "the token of the row is replaced"
        );

        // a device that carries no id is not folded into anyone's row
        sessions.create(NewSession::from_agent(agent, ip, Via::Password));
        assert_eq!(sessions.list.lock().len(), 2);
        // and another device is another row, however alike it looks
        sessions.create(
            NewSession::from_agent(agent, ip, Via::Password)
                .on_device("fedcba9876543210fedcba9876543210"),
        );
        assert_eq!(sessions.list.lock().len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_of_one_device_from_before_3_5_are_listed_as_one() {
        let dir = std::env::temp_dir().join(format!("rc-fold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("sessions.json");
        let agent = "Mozilla/5.0 (iPhone) Safari/605.1.15";
        let ends = now_unix() + 86_400;
        let old = json!([
            {
                "id": "1111111111111111", "hash": token_hash("a"), "name": "iPhone, Safari",
                "agent": agent, "ip": "192.0.2.7", "created": "2026-09-07T20:15:00",
                "lastSeen": "2026-09-08T09:00:00", "via": "approve", "expires": ends,
            },
            {
                "id": "2222222222222222", "hash": token_hash("b"), "name": "Windows PC, Edge",
                "agent": "Mozilla/5.0 (Windows NT 10.0) Edg/141.0.0.0", "ip": "192.0.2.9",
                "created": "2026-09-08T18:00:00", "lastSeen": "2026-09-08T18:30:00",
                "via": "password", "expires": ends,
            },
            {
                "id": "3333333333333333", "hash": token_hash("c"), "name": "iPhone, Safari",
                "agent": agent, "ip": "192.0.2.7", "created": "2026-09-10T19:00:00",
                "lastSeen": "2026-09-10T21:00:00", "via": "qr", "expires": ends,
            },
        ]);
        std::fs::write(&file, old.to_string()).unwrap();

        let sessions = Sessions::load(&file);
        assert!(
            sessions.list.lock().iter().all(|s| s.device.is_empty()),
            "a session from before 3.5 has no device id"
        );
        let rows = sessions.list(None);
        assert_eq!(rows.len(), 2, "one phone, one PC: {rows:?}");
        let phone = &rows[0];
        assert_eq!(phone["sessions"], 2, "{phone}");
        assert_eq!(phone["via"], "qr", "the newest way in: {phone}");
        assert_eq!(phone["created"], "2026-09-07T20:15:00", "{phone}");
        assert_eq!(phone["lastSeen"], "2026-09-10T21:00:00", "{phone}");
        assert_eq!(phone["device"], "", "{phone}");

        // signing that one row out ends both of its sessions, and nobody else's
        let id = phone["id"].as_str().unwrap_or_default().to_string();
        assert_eq!(sessions.revoke(&id).as_deref(), Some("iPhone, Safari"));
        assert_eq!(sessions.list.lock().len(), 1, "the PC keeps its session");
        assert_eq!(sessions.list(None).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_device_cookie_is_lax_and_never_proof_of_anything() {
        let phone = "0123456789abcdef0123456789abcdef";
        let value = set_device_cookie_value(phone, false);
        // Lax, or the sign-in that needs it most - a QR code opened from a
        // scanner app - would arrive without it.
        assert!(value.contains("SameSite=Lax"), "{value}");
        assert!(value.contains("HttpOnly"), "{value}");
        assert!(!value.contains("Secure"), "{value}");
        assert!(
            value.contains(&format!("Max-Age={}", DEVICE_DAYS * 86_400)),
            "{value}"
        );
        assert!(set_device_cookie_value(phone, true).contains("; Secure"));

        let mut headers = HeaderMap::new();
        headers.insert(
            COOKIE,
            format!("{COOKIE_NAME}=abc; {DEVICE_COOKIE}={phone}")
                .parse()
                .unwrap(),
        );
        assert_eq!(device_id(&headers).as_deref(), Some(phone));
        assert_eq!(device_of(&headers, Client::Browser), (phone.into(), false));
        // a client of ours keeps no cookies, so it stays without a device
        assert_eq!(device_of(&headers, Client::Native).0, "");
        // anything that is not one of our ids is ignored
        let mut junk = HeaderMap::new();
        junk.insert(
            COOKIE,
            format!("{DEVICE_COOKIE}=../../secrets").parse().unwrap(),
        );
        assert_eq!(device_id(&junk), None);
        let (fresh, is_new) = device_of(&junk, Client::Browser);
        assert!(is_new && is_device_id(&fresh), "{fresh}");
    }
}
