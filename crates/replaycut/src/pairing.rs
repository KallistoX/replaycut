//! The device login (since 2.8): a browser asks for access, this PC says
//! yes. The state lives in RAM only - at most five open requests, each two
//! minutes long - because a request that survives a restart is a request
//! nobody watched.
//!
//! Two ways in, one machine: the phone presses "Ask <hostname>" and the PC
//! approves (toast, modal, tray), or the phone scans the QR code, whose URL
//! carries a token that is good once. Both end in a session cookie on the
//! device that asked. See docs/api.md "Since 2.8".

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::auth::random_hex;
use crate::state::AppState;
use crate::util;

/// How long a request waits for the PC.
pub const TTL: Duration = Duration::from_secs(120);
/// How long the answer stays readable for the device that asked.
const KEEP_AFTER: Duration = Duration::from_secs(60);
/// At most this many requests wait at the same time.
const MAX_OPEN: usize = 5;
/// At most this many new requests a minute.
const MAX_PER_MINUTE: usize = 5;
/// More than this many addresses within a minute look like a flood.
const MAX_ADDRESSES: usize = 3;
/// How long the device login pauses after a flood.
const PAUSE: Duration = Duration::from_secs(600);
/// How long a QR token can be redeemed.
const QR_TTL: Duration = Duration::from_secs(120);
/// QR codes are re-drawn while a page shows them; keep only the last few.
const MAX_QR_TOKENS: usize = 8;
/// Letters and digits that cannot be mistaken for each other.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// How often the sweeper looks while requests are open.
const SWEEP_EVERY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Denied => "denied",
            Status::Expired => "expired",
        }
    }
}

/// One device waiting for an answer.
#[derive(Debug, Clone)]
pub struct Request {
    /// 16 random bytes as hex: whoever knows it may ask for the answer.
    pub id: String,
    /// What the device calls itself, "iPhone, Safari".
    pub name: String,
    pub agent: String,
    pub ip: IpAddr,
    /// Four characters, shown on both sides so they mean the same request.
    pub code: String,
    /// Local timestamp, for the card on the PC.
    pub asked: String,
    created: Instant,
    status: Status,
    /// The session token, waiting for the device's next poll.
    token: Option<String>,
}

impl Request {
    fn expires_in(&self) -> u64 {
        TTL.saturating_sub(self.created.elapsed()).as_secs()
    }

    /// What the PC's pages show. The token never leaves through here.
    pub fn view(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "agent": self.agent,
            "ip": self.ip.to_string(),
            "code": self.code,
            "asked": self.asked,
            "expires": self.expires_in(),
        })
    }
}

/// Why a request was turned away.
#[derive(Debug, Clone, Copy)]
pub enum Refused {
    /// The device login is paused after a flood; seconds left.
    Paused(u64),
    /// Too many requests at once or in the last minute.
    TooMany,
}

/// Why a decision did not land.
#[derive(Debug, Clone, Copy)]
pub enum NotDecided {
    Unknown,
    /// Already approved, denied or expired.
    Settled,
}

#[derive(Default)]
struct Inner {
    requests: Vec<Request>,
    /// When and from where a request came, for the rate limits.
    recent: Vec<(Instant, IpAddr)>,
    paused_until: Option<Instant>,
    /// SHA-256 of the QR tokens that are still good.
    qr: Vec<(String, Instant)>,
    /// Whether the sweeper task runs.
    sweeping: bool,
}

/// The device login of one service.
#[derive(Default)]
pub struct Pairing {
    inner: Mutex<Inner>,
}

fn code() -> String {
    let mut bytes = [0u8; 4];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| CODE_ALPHABET[*b as usize % CODE_ALPHABET.len()] as char)
        .collect()
}

fn hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Inner {
    /// Expire what timed out and forget what nobody will read again.
    /// Returns whether anything the pages show has changed.
    fn prune(&mut self) -> bool {
        let mut changed = false;
        for r in &mut self.requests {
            if r.status == Status::Pending && r.created.elapsed() >= TTL {
                r.status = Status::Expired;
                changed = true;
                tracing::info!("sign-in request from {} expired", r.ip);
            }
        }
        let before = self.requests.len();
        self.requests
            .retain(|r| r.created.elapsed() < TTL + KEEP_AFTER);
        changed |= self.requests.len() != before;
        self.recent
            .retain(|(t, _)| t.elapsed() < Duration::from_secs(60));
        self.qr.retain(|(_, t)| t.elapsed() < QR_TTL);
        if self.paused_until.is_some_and(|u| u <= Instant::now()) {
            self.paused_until = None;
            tracing::info!("the device login accepts requests again");
            changed = true;
        }
        changed
    }

    fn open(&self) -> usize {
        self.requests
            .iter()
            .filter(|r| r.status == Status::Pending)
            .count()
    }
}

impl Pairing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seconds the device login is still paused, if it is.
    pub fn paused(&self) -> Option<u64> {
        let mut inner = self.inner.lock();
        inner.prune();
        inner
            .paused_until
            .map(|u| u.saturating_duration_since(Instant::now()).as_secs().max(1))
    }

    /// A device asks for access.
    pub fn ask(
        &self,
        name: &str,
        agent: &str,
        ip: IpAddr,
    ) -> Result<(String, String, u64), Refused> {
        let mut inner = self.inner.lock();
        inner.prune();
        if let Some(until) = inner.paused_until {
            return Err(Refused::Paused(
                until
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    .max(1),
            ));
        }
        if inner.recent.len() >= MAX_PER_MINUTE {
            return Err(Refused::TooMany);
        }
        let mut addresses: Vec<IpAddr> = inner.recent.iter().map(|(_, a)| *a).collect();
        addresses.push(ip);
        addresses.sort();
        addresses.dedup();
        if addresses.len() > MAX_ADDRESSES {
            inner.paused_until = Some(Instant::now() + PAUSE);
            tracing::warn!(
                "sign-in requests from {} addresses within a minute: the device login pauses for {} minutes (the password login is not affected)",
                addresses.len(),
                PAUSE.as_secs() / 60
            );
            return Err(Refused::Paused(PAUSE.as_secs()));
        }
        // one open request per device: asking again replaces the old one
        inner
            .requests
            .retain(|r| !(r.ip == ip && r.status == Status::Pending));
        if inner.open() >= MAX_OPEN {
            return Err(Refused::TooMany);
        }
        let request = Request {
            id: random_hex(16),
            name: name.chars().take(60).collect(),
            agent: agent.chars().take(200).collect(),
            ip,
            code: code(),
            asked: util::now_local(),
            created: Instant::now(),
            status: Status::Pending,
            token: None,
        };
        let answer = (request.id.clone(), request.code.clone(), TTL.as_secs());
        tracing::info!(
            "sign-in request from {ip} ({}) - code {}",
            request.name,
            request.code
        );
        inner.recent.push((Instant::now(), ip));
        inner.requests.push(request);
        Ok(answer)
    }

    /// The device asks how its request is doing. The token comes with the
    /// first answer after an approval, once, and only to the address that
    /// asked.
    pub fn poll(&self, id: &str, ip: IpAddr) -> (&'static str, Option<String>) {
        let mut inner = self.inner.lock();
        inner.prune();
        let Some(request) = inner.requests.iter_mut().find(|r| r.id == id) else {
            // unknown or long gone - the device starts over
            return ("expired", None);
        };
        if request.ip != ip {
            tracing::warn!(
                "sign-in request {} polled from {ip} instead of {}",
                request.code,
                request.ip
            );
            return ("denied", None);
        }
        let token = if request.status == Status::Approved {
            request.token.take()
        } else {
            None
        };
        (request.status.as_str(), token)
    }

    /// The PC says yes. `token` makes the session for the device and is
    /// called with the request it belongs to.
    pub fn approve(
        &self,
        id: &str,
        token: impl FnOnce(&Request) -> String,
    ) -> Result<Request, NotDecided> {
        let mut inner = self.inner.lock();
        inner.prune();
        let request = inner
            .requests
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(NotDecided::Unknown)?;
        if request.status != Status::Pending {
            return Err(NotDecided::Settled);
        }
        request.status = Status::Approved;
        request.token = Some(token(request));
        tracing::info!(
            "sign-in request from {} ({}) approved",
            request.ip,
            request.name
        );
        Ok(request.clone())
    }

    /// The PC says no.
    pub fn deny(&self, id: &str) -> Result<Request, NotDecided> {
        let mut inner = self.inner.lock();
        inner.prune();
        let request = inner
            .requests
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or(NotDecided::Unknown)?;
        if request.status != Status::Pending {
            return Err(NotDecided::Settled);
        }
        request.status = Status::Denied;
        request.token = None;
        tracing::info!(
            "sign-in request from {} ({}) denied",
            request.ip,
            request.name
        );
        Ok(request.clone())
    }

    /// The open requests, newest last, for the pages on the PC.
    pub fn pending(&self) -> Vec<Value> {
        let mut inner = self.inner.lock();
        inner.prune();
        inner
            .requests
            .iter()
            .filter(|r| r.status == Status::Pending)
            .map(Request::view)
            .collect()
    }

    /// How many devices are waiting (tray, tooltip).
    pub fn pending_count(&self) -> usize {
        let mut inner = self.inner.lock();
        inner.prune();
        inner.open()
    }

    /// A token for the QR code, good once and for two minutes.
    pub fn qr_token(&self) -> String {
        use base64::Engine as _;
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut inner = self.inner.lock();
        inner.prune();
        inner.qr.push((hash(&token), Instant::now()));
        while inner.qr.len() > MAX_QR_TOKENS {
            inner.qr.remove(0);
        }
        token
    }

    /// Redeem a scanned token. True at most once per token.
    pub fn redeem_qr(&self, token: &str) -> bool {
        let mut inner = self.inner.lock();
        inner.prune();
        let wanted = hash(token);
        match inner.qr.iter().position(|(h, _)| *h == wanted) {
            Some(i) => {
                inner.qr.remove(i);
                true
            }
            None => false,
        }
    }

    /// One round of the sweeper: `(something changed, keep sweeping)`.
    fn sweep_step(&self) -> (bool, bool) {
        let mut inner = self.inner.lock();
        let changed = inner.prune();
        let busy = !inner.requests.is_empty() || inner.paused_until.is_some();
        if !busy {
            inner.sweeping = false;
        }
        (changed, busy)
    }

    /// Whether a sweeper has to be started for what was just added.
    fn needs_sweeper(&self) -> bool {
        let mut inner = self.inner.lock();
        if inner.sweeping {
            return false;
        }
        inner.sweeping = true;
        true
    }
}

/// Watch the open requests while there are any: a request that runs out
/// has to disappear from the modal, the approve page and the tray without
/// anybody asking. The task ends with the last request, so an idle service
/// stays idle.
pub fn watch(app: &Arc<AppState>) {
    if !app.pairing.needs_sweeper() {
        return;
    }
    let app = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            let (changed, keep_going) = app.pairing.sweep_step();
            if changed {
                app.tray_changed();
            }
            if !keep_going {
                break;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 0, 2, last])
    }

    fn pairing() -> Pairing {
        Pairing::new()
    }

    #[test]
    fn codes_are_four_readable_characters() {
        for _ in 0..200 {
            let c = code();
            assert_eq!(c.chars().count(), 4, "{c}");
            assert!(
                c.bytes().all(|b| CODE_ALPHABET.contains(&b)),
                "{c} has a character that can be misread"
            );
        }
    }

    #[test]
    fn ask_approve_poll_hands_the_token_over_once() {
        let p = pairing();
        let (id, code, expires) = p.ask("iPhone, Safari", "agent", ip(7)).expect("asked");
        assert_eq!(code.len(), 4);
        assert_eq!(expires, 120);
        assert_eq!(p.poll(&id, ip(7)), ("pending", None));
        assert_eq!(p.pending_count(), 1);
        let view = p.pending().remove(0);
        assert_eq!(view["id"], id.as_str());
        assert_eq!(view["name"], "iPhone, Safari");
        assert_eq!(view["ip"], "192.0.2.7");
        assert!(view["expires"].as_u64().unwrap_or(0) > 100);

        let approved = p
            .approve(&id, |r| format!("token-for-{}", r.ip))
            .expect("approve");
        assert_eq!(approved.name, "iPhone, Safari");
        assert_eq!(p.pending_count(), 0, "an answered request is not pending");
        // the token comes once, to the device that asked
        let (status, token) = p.poll(&id, ip(7));
        assert_eq!(status, "approved");
        assert_eq!(token.as_deref(), Some("token-for-192.0.2.7"));
        assert_eq!(p.poll(&id, ip(7)), ("approved", None), "only once");
        // and a second decision does not land
        assert!(matches!(
            p.approve(&id, |_| "x".into()),
            Err(NotDecided::Settled)
        ));
        assert!(matches!(p.deny("nope"), Err(NotDecided::Unknown)));
    }

    #[test]
    fn another_address_gets_nothing() {
        let p = pairing();
        let (id, _, _) = p.ask("iPhone", "agent", ip(7)).unwrap();
        p.approve(&id, |_| "secret".into()).unwrap();
        assert_eq!(p.poll(&id, ip(8)), ("denied", None), "not this device");
        // the real device still gets its token
        assert_eq!(p.poll(&id, ip(7)).1.as_deref(), Some("secret"));
    }

    #[test]
    fn deny_and_an_unknown_id() {
        let p = pairing();
        let (id, _, _) = p.ask("Laptop", "agent", ip(9)).unwrap();
        p.deny(&id).unwrap();
        assert_eq!(p.poll(&id, ip(9)), ("denied", None));
        assert_eq!(p.poll("0123456789abcdef", ip(9)), ("expired", None));
    }

    #[test]
    fn a_second_request_replaces_the_first_one_of_that_device() {
        let p = pairing();
        let (first, _, _) = p.ask("iPhone", "agent", ip(7)).unwrap();
        let (second, _, _) = p.ask("iPhone", "agent", ip(7)).unwrap();
        assert_ne!(first, second);
        assert_eq!(p.pending_count(), 1);
        assert_eq!(p.poll(&first, ip(7)), ("expired", None));
        assert_eq!(p.poll(&second, ip(7)), ("pending", None));
    }

    #[test]
    fn a_flood_pauses_the_device_login() {
        let p = pairing();
        p.ask("a", "agent", ip(1)).unwrap();
        p.ask("b", "agent", ip(2)).unwrap();
        p.ask("c", "agent", ip(3)).unwrap();
        assert!(p.paused().is_none());
        // the fourth address within the minute is one too many
        let refused = p.ask("d", "agent", ip(4)).expect_err("paused");
        assert!(matches!(refused, Refused::Paused(s) if s > 500));
        assert!(p.paused().is_some());
        // even the addresses that were fine before have to wait now
        assert!(matches!(
            p.ask("a", "agent", ip(1)),
            Err(Refused::Paused(_))
        ));
    }

    #[test]
    fn five_requests_a_minute_are_enough() {
        let p = pairing();
        for _ in 0..5 {
            p.ask("iPhone", "agent", ip(7)).unwrap();
        }
        assert!(matches!(
            p.ask("iPhone", "agent", ip(7)),
            Err(Refused::TooMany)
        ));
    }

    #[test]
    fn qr_tokens_work_once() {
        let p = pairing();
        let token = p.qr_token();
        assert!(token.len() >= 42, "{token}");
        assert!(p.redeem_qr(&token));
        assert!(!p.redeem_qr(&token), "a scanned code is used up");
        assert!(!p.redeem_qr("nonsense"));
        // the newest tokens survive, the oldest fall out
        let tokens: Vec<String> = (0..MAX_QR_TOKENS + 2).map(|_| p.qr_token()).collect();
        assert!(!p.redeem_qr(&tokens[0]));
        assert!(p.redeem_qr(tokens.last().unwrap()));
    }

    #[test]
    fn requests_run_out() {
        let p = pairing();
        let (id, _, _) = p.ask("iPhone", "agent", ip(7)).unwrap();
        // pretend the two minutes are over
        {
            let mut inner = p.inner.lock();
            let r = &mut inner.requests[0];
            r.created = Instant::now() - TTL - Duration::from_secs(1);
        }
        assert_eq!(p.poll(&id, ip(7)), ("expired", None));
        assert_eq!(p.pending_count(), 0);
        assert!(matches!(
            p.approve(&id, |_| "x".into()),
            Err(NotDecided::Settled)
        ));
        // and it is forgotten once nobody can be waiting for the answer
        {
            let mut inner = p.inner.lock();
            let r = &mut inner.requests[0];
            r.created = Instant::now() - TTL - KEEP_AFTER - Duration::from_secs(1);
        }
        let (_, keep_going) = p.sweep_step();
        assert!(!keep_going, "nothing left to watch");
        assert_eq!(p.poll(&id, ip(7)), ("expired", None));
    }
}
