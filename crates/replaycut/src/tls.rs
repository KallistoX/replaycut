//! TLS on the port (since 3.4), off unless `https.enabled` says otherwise.
//!
//! Two ways to get a certificate:
//!
//! - **Own PEM pair** (`https.cert` and `https.key`). The comfortable one: a
//!   certificate from Tailscale, a reverse proxy or a real domain is trusted
//!   by every device already, and replaycut only reads the files.
//! - **Own certificate authority** in `<data-dir>/tls`. The CA is the
//!   *identity of this service*: it is made once and never replaced, so a
//!   device that trusts it keeps trusting it. The certificate the CA signs
//!   carries the machine's names and addresses and is re-issued whenever
//!   those change or it runs out - the CA stays put through all of it. That
//!   split is what makes pinning possible for a client later on: it pins the
//!   CA, not the certificate of the day.
//!
//! Browsers do not know that CA, so they warn once until someone imports
//! `ca.crt` by hand. That is the deliberate limit of this package - see
//! `docs/api.md` "Since 3.4".

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio::net::{TcpListener, TcpStream};

use crate::settings::Settings;

/// The CA outlives every certificate it signs, because replacing it costs
/// every device its trust; the leaf is cheap to replace and therefore short.
const CA_YEARS: i64 = 10;
const LEAF_YEARS: i64 = 2;
/// Re-issue the leaf when less than this is left. The check runs at startup,
/// so the diagnostics tell the user to restart rather than to wait.
const RENEW_BEFORE_DAYS: i64 = 30;

/// How long a connection has to reveal whether it speaks TLS. A browser sends
/// its first bytes at once; a port scanner that says nothing costs one task.
const FIRST_BYTES_TIMEOUT: Duration = Duration::from_secs(10);
/// A plaintext request only has to get as far as its headers before we answer
/// it with a 400. Anything longer is not a browser asking politely.
const MAX_HEAD: usize = 8 * 1024;

/// What `prepare` hands to the listener.
pub struct Tls {
    pub config: Arc<ServerConfig>,
    pub info: Info,
}

/// What the rest of the service needs to know about TLS: which scheme the
/// addresses carry, what to pin, what the diagnostics report. Cheap to clone
/// and free of anything cryptographic, so it can live in the app state.
#[derive(Debug, Clone, Default)]
pub struct Info {
    /// TLS is actually running. `https.enabled` alone is not enough: a
    /// certificate that cannot be read leaves the service on plain HTTP.
    pub active: bool,
    /// SHA-256 of the CA certificate (DER), base64url without padding. What a
    /// client pins, and what the settings page shows for comparing by hand.
    /// Empty with an own PEM pair: that certificate needs no pinning.
    pub fingerprint: String,
    /// The CA file to import by hand. `None` with an own PEM pair.
    pub ca_file: Option<PathBuf>,
    /// When the certificate being served runs out.
    pub not_after: Option<DateTime<Utc>>,
    /// The certificate belongs to the user, not to us.
    pub own: bool,
}

impl Info {
    /// One line for the startup log: which certificate is being served, how
    /// long it lasts, and the fingerprint to compare a client's pin against.
    pub fn describe(&self) -> String {
        if !self.active {
            return "off".to_string();
        }
        let until = match self.not_after {
            Some(t) => format!(", valid until {}", t.format("%Y-%m-%d")),
            None => String::new(),
        };
        if self.own {
            return format!("on, your own certificate{until}");
        }
        format!(
            "on, own certificate authority ({}){until}, fingerprint {}",
            self.ca_file
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.fingerprint
        )
    }
}

/// Read or create everything TLS needs. Errors are for the caller to report;
/// it starts the service on plain HTTP rather than not at all.
pub fn prepare(data_dir: &Path, settings: &Settings) -> Result<Tls> {
    if let Some((cert, key)) = settings.https.own_pair() {
        return own_pair(Path::new(cert), Path::new(key));
    }
    own_ca(&data_dir.join("tls"), settings)
}

/// The user's own certificate: read the two files, nothing else.
fn own_pair(cert: &Path, key: &Path) -> Result<Tls> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("read the certificate {}", cert.display()))?
        .collect::<Result<_, _>>()
        .with_context(|| format!("read the certificate {}", cert.display()))?;
    anyhow::ensure!(!chain.is_empty(), "{} holds no certificate", cert.display());
    let private = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("read the private key {}", key.display()))?;
    let not_after = read_leaf(&chain[0]).ok().map(|(_, until)| until);
    Ok(Tls {
        config: server_config(chain, private)?,
        info: Info {
            active: true,
            not_after,
            own: true,
            ..Info::default()
        },
    })
}

/// The certificate authority of this installation, and a certificate from it
/// that matches what this machine is called today.
fn own_ca(dir: &Path, settings: &Settings) -> Result<Tls> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("create the certificate folder {}", dir.display()))?;
    protect(dir);

    let (ca_pem, ca_key_pem) = load_or_make_ca(dir)?;
    let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes())
        .context("the stored certificate authority is not a certificate")?;
    let fingerprint = fingerprint(&ca_der);

    let want = wanted_sans(
        &crate::platform::hostname(),
        &crate::platform::lan_host(),
        &settings.allowed_hosts,
        crate::platform::primary_ipv4(),
    );

    let (leaf_pem, leaf_key_pem, not_after) = match reusable_leaf(dir, &want) {
        Some(found) => found,
        None => {
            let made = issue_leaf(&ca_pem, &ca_key_pem, &want)?;
            std::fs::write(dir.join("server.crt"), &made.0).context("write server.crt")?;
            std::fs::write(dir.join("server.key"), &made.1).context("write server.key")?;
            protect(&dir.join("server.key"));
            tracing::info!(
                "TLS certificate issued for {} - valid until {}",
                want.join(", "),
                made.2.format("%Y-%m-%d")
            );
            made
        }
    };

    let leaf = CertificateDer::from_pem_slice(leaf_pem.as_bytes())
        .context("the stored server certificate is not a certificate")?;
    let private = PrivateKeyDer::from_pem_slice(leaf_key_pem.as_bytes())
        .context("the stored server key is not a private key")?;
    Ok(Tls {
        config: server_config(vec![leaf, ca_der], private)?,
        info: Info {
            active: true,
            fingerprint,
            ca_file: Some(dir.join("ca.crt")),
            not_after: Some(not_after),
            own: false,
        },
    })
}

/// The stored certificate, if it still says what we want it to say.
fn reusable_leaf(dir: &Path, want: &[String]) -> Option<(String, String, DateTime<Utc>)> {
    let cert_pem = std::fs::read_to_string(dir.join("server.crt")).ok()?;
    let key_pem = std::fs::read_to_string(dir.join("server.key")).ok()?;
    let der = CertificateDer::from_pem_slice(cert_pem.as_bytes()).ok()?;
    let (have, not_after) = read_leaf(&der).ok()?;
    if needs_reissue(&have, want, not_after, Utc::now()) {
        tracing::info!("TLS certificate re-issued: the addresses changed or it is running out");
        return None;
    }
    Some((cert_pem, key_pem, not_after))
}

/// Read the CA from disk, or make one on the first run with HTTPS on.
fn load_or_make_ca(dir: &Path) -> Result<(String, String)> {
    let (crt, key) = (dir.join("ca.crt"), dir.join("ca.key"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&crt), std::fs::read_to_string(&key)) {
        if !c.trim().is_empty() && !k.trim().is_empty() {
            return Ok((c, k));
        }
    }
    let (c, k) = make_ca()?;
    std::fs::write(&crt, &c).with_context(|| format!("write {}", crt.display()))?;
    std::fs::write(&key, &k).with_context(|| format!("write {}", key.display()))?;
    protect(&key);
    tracing::info!(
        "certificate authority created in {} - it identifies this replaycut from now on",
        dir.display()
    );
    Ok((c, k))
}

fn make_ca() -> Result<(String, String)> {
    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
        KeyUsagePurpose,
    };
    let key = KeyPair::generate().context("generate the CA key")?;
    let mut params = CertificateParams::new(Vec::new()).context("CA parameters")?;
    let mut name = DistinguishedName::new();
    // The name carries the machine, so that a certificate store with several
    // of these in it stays readable. It is not used for matching anywhere.
    name.push(
        DnType::CommonName,
        format!("replaycut CA on {}", crate::platform::hostname()),
    );
    name.push(DnType::OrganizationName, "replaycut");
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let (from, until) = lifetime(CA_YEARS);
    params.not_before = from;
    params.not_after = until;
    let cert = params.self_signed(&key).context("sign the CA")?;
    Ok((cert.pem(), key.serialize_pem()))
}

/// A server certificate for `sans`, signed by the stored CA.
fn issue_leaf(
    ca_pem: &str,
    ca_key_pem: &str,
    sans: &[String],
) -> Result<(String, String, DateTime<Utc>)> {
    use rcgen::{
        CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, Issuer, KeyPair,
        KeyUsagePurpose,
    };
    let ca_key = KeyPair::from_pem(ca_key_pem).context("read the CA key")?;
    let issuer = Issuer::from_ca_cert_pem(ca_pem, ca_key).context("read the CA certificate")?;

    let key = KeyPair::generate().context("generate the server key")?;
    let mut params =
        CertificateParams::new(sans.to_vec()).context("the machine's names are not usable")?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, crate::platform::hostname());
    params.distinguished_name = name;
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let (from, until) = lifetime(LEAF_YEARS);
    params.not_before = from;
    params.not_after = until;
    let cert = params
        .signed_by(&key, &issuer)
        .context("sign the server certificate")?;
    Ok((
        cert.pem(),
        key.serialize_pem(),
        offset_to_chrono(until).unwrap_or_else(Utc::now),
    ))
}

/// Valid from an hour ago, so that a machine whose clock runs behind does not
/// reject a certificate made seconds earlier.
fn lifetime(years: i64) -> (time::OffsetDateTime, time::OffsetDateTime) {
    let now = time::OffsetDateTime::now_utc();
    (
        now - time::Duration::hours(1),
        now + time::Duration::days(365 * years),
    )
}

fn offset_to_chrono(t: time::OffsetDateTime) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(t.unix_timestamp(), 0)
}

/// The names and addresses in a certificate, and when it runs out.
fn read_leaf(der: &CertificateDer<'_>) -> Result<(Vec<String>, DateTime<Utc>)> {
    use x509_parser::prelude::*;
    let (_, cert) =
        X509Certificate::from_der(der.as_ref()).context("parse the stored certificate")?;
    let mut names = Vec::new();
    if let Ok(Some(san)) = cert.subject_alternative_name() {
        for general in &san.value.general_names {
            match general {
                GeneralName::DNSName(n) => names.push(n.to_ascii_lowercase()),
                GeneralName::IPAddress(bytes) => {
                    if let Some(ip) = ip_from_bytes(bytes) {
                        names.push(ip.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    names.sort();
    names.dedup();
    let not_after = DateTime::from_timestamp(cert.validity().not_after.timestamp(), 0)
        .context("the certificate has no usable expiry date")?;
    Ok((names, not_after))
}

fn ip_from_bytes(bytes: &[u8]) -> Option<IpAddr> {
    match bytes.len() {
        4 => Some(IpAddr::from(<[u8; 4]>::try_from(bytes).ok()?)),
        16 => Some(IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?)),
        _ => None,
    }
}

/// Every name and address this machine answers to, as a certificate wants
/// them: sorted and without duplicates, so that comparing two lists is a
/// comparison and not a search.
pub fn wanted_sans(
    hostname: &str,
    lan_host: &str,
    allowed_hosts: &[String],
    ip: Option<Ipv4Addr>,
) -> Vec<String> {
    let mut out = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    let mut add_name = |raw: &str| {
        let name = raw.trim().trim_matches('.').to_ascii_lowercase();
        if name.is_empty() {
            return;
        }
        let is_ip = name.parse::<IpAddr>().is_ok();
        out.push(name.clone());
        // Windows resolves plain computer names, Linux announces `<name>.local`
        // over mDNS; the certificate carries both so either way in works.
        if !is_ip && !name.contains('.') {
            out.push(format!("{name}.local"));
        }
    };
    add_name(hostname);
    add_name(lan_host);
    for host in allowed_hosts {
        add_name(host);
    }
    if let Some(ip) = ip {
        out.push(ip.to_string());
    }
    out.sort();
    out.dedup();
    out
}

/// Whether the stored certificate still fits. The CA is never part of this
/// question - only the leaf is ever replaced.
pub fn needs_reissue(
    have: &[String],
    want: &[String],
    not_after: DateTime<Utc>,
    now: DateTime<Utc>,
) -> bool {
    have != want || not_after - now < chrono::Duration::days(RENEW_BEFORE_DAYS)
}

fn fingerprint(der: &CertificateDer<'_>) -> String {
    let digest = Sha256::digest(der.as_ref());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    // Name the provider instead of relying on the default one: reqwest
    // decides which rustls features are on, and a silent change there would
    // otherwise turn into a panic at startup.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("no usable TLS versions")?
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .context("the certificate and the key do not belong together")?;
    // HTTP/2 is not worth its complexity for one browser on a LAN.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Keep the private keys to this account, where the file system says who may
/// read a file. A failure is a warning, not a reason to stay on HTTP.
///
/// Windows is deliberately not in here. The data folder lives under
/// `%LOCALAPPDATA%` and is already bound to this account - the same protection
/// `settings.json` (with the password hash) and `sessions.json` (with the
/// session token hashes) rely on. An `icacls` call on top of that buys close
/// to nothing and can take the file away from its owner for good: dropping
/// the inherited entries without a valid grant leaves an empty ACL, and then
/// not even the account that made the file can read it.
fn protect(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if path.is_dir() { 0o700 } else { 0o600 };
        if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
            tracing::warn!("cannot restrict {}: {e}", path.display());
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

// ---------------------------------------------------------------------------
// Talking to our own service from the command line

/// Where `replaycut install` and `replaycut test` reach the running service.
pub fn local_base(settings: &Settings) -> String {
    let scheme = if settings.https.enabled {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://localhost:{}", settings.port)
}

/// A client for that address. With our own certificate authority nothing on
/// this machine knows it yet, so the client is told about it here - the same
/// way the test suite is told through `TLS_CA`. An own PEM pair needs no help.
pub fn local_client(data_dir: &Path, settings: &Settings, timeout: Duration) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if settings.https.enabled && settings.https.own_pair().is_none() {
        let ca = data_dir.join("tls").join("ca.crt");
        match std::fs::read(&ca)
            .map_err(|e| e.to_string())
            .and_then(|pem| reqwest::Certificate::from_pem(&pem).map_err(|e| e.to_string()))
        {
            Ok(cert) => builder = builder.add_root_certificate(cert),
            Err(e) => tracing::debug!("cannot read {} for the local client: {e}", ca.display()),
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

// ---------------------------------------------------------------------------
// The listener

/// A listener that hands axum finished TLS connections.
///
/// The handshake happens in a task of its own, not in `accept`: a slow or
/// broken handshake would otherwise hold up every other connection behind it.
/// Plaintext arriving on the port gets an answer instead of a reset - old
/// bookmarks and the toast link of a session that was running when HTTPS went
/// on both end up here.
pub struct TlsListener {
    local: SocketAddr,
    rx: tokio::sync::mpsc::Receiver<(tokio_rustls::server::TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    pub fn new(listener: TcpListener, config: Arc<ServerConfig>) -> Result<Self> {
        let local = listener.local_addr().context("the socket has no address")?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(accept_loop(
            listener,
            tokio_rustls::TlsAcceptor::from(config),
            tx,
        ));
        Ok(Self { local, rx })
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(pair) => pair,
            // The accept loop only ends with the runtime. Never handing out a
            // connection is the honest answer; axum has no way to be told.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    tx: tokio::sync::mpsc::Sender<(tokio_rustls::server::TlsStream<TcpStream>, SocketAddr)>,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                // Out of file descriptors, mostly. Wait rather than spin.
                tracing::warn!("cannot accept a connection: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            match first_bytes(&stream).await {
                Some(Kind::Tls) => match acceptor.accept(stream).await {
                    Ok(tls) => {
                        let _ = tx.send((tls, peer)).await;
                    }
                    // One failed handshake per port scan would fill the log.
                    Err(e) => tracing::debug!("TLS handshake with {peer} failed: {e}"),
                },
                Some(Kind::Plain) => {
                    if let Err(e) = answer_plaintext(stream).await {
                        tracing::debug!("cannot answer the plaintext request from {peer}: {e}");
                    }
                }
                _ => {}
            }
        });
    }
}

/// What the first bytes of a connection say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Tls,
    Plain,
    Unknown,
}

async fn first_bytes(stream: &TcpStream) -> Option<Kind> {
    let mut buf = [0u8; 8];
    let read = tokio::time::timeout(FIRST_BYTES_TIMEOUT, stream.peek(&mut buf))
        .await
        .ok()?
        .ok()?;
    Some(classify(&buf[..read]))
}

/// `0x16` is the first byte of a TLS handshake record and can be nothing else
/// here; a plaintext request starts with a method name. Everything else gets
/// no answer, because we would only be guessing what it wanted.
pub fn classify(first: &[u8]) -> Kind {
    if first.first() == Some(&0x16) {
        return Kind::Tls;
    }
    const METHODS: [&[u8]; 7] = [
        b"GET ", b"POST", b"HEAD", b"PUT ", b"DELE", b"OPTI", b"PATC",
    ];
    if first.len() >= 4 && METHODS.iter().any(|m| first.starts_with(m)) {
        return Kind::Plain;
    }
    Kind::Unknown
}

async fn answer_plaintext(mut stream: TcpStream) -> io::Result<()> {
    let head = read_head(&mut stream).await?;
    let body = plaintext_page(host_header(&head));
    let response = format!(
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let read = tokio::time::timeout(FIRST_BYTES_TIMEOUT, stream.read(&mut chunk))
            .await
            .unwrap_or(Ok(0))?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_HEAD {
            break;
        }
    }
    buf.truncate(MAX_HEAD);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The `Host` header, but only when it looks like one. It ends up in a link
/// on the page we answer with, and a header is whatever the caller typed.
pub fn host_header(head: &str) -> Option<&str> {
    let value = head
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .map(|(_, v)| v.trim())?;
    let plausible = !value.is_empty()
        && value.len() <= 260
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._:[]".contains(&b));
    plausible.then_some(value)
}

/// The page a plaintext request gets. No stylesheet and no script: this
/// answer has to work on a port that speaks TLS to everyone else.
pub fn plaintext_page(host: Option<&str>) -> String {
    let target = match host {
        Some(h) => format!("https://{h}/"),
        None => String::new(),
    };
    let link = if target.is_empty() {
        "<p>Use <strong>https://</strong> instead of http:// in the address bar.</p>".to_string()
    } else {
        format!("<p><a href=\"{target}\">{target}</a></p>")
    };
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>replaycut - use HTTPS</title>\
         <body style=\"font-family:system-ui,sans-serif;background:#141519;color:#e8e8ec;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <div style=\"max-width:32rem;padding:2rem\">\
         <h1 style=\"font-size:1.4rem\">replaycut speaks HTTPS on this port</h1>\
         {link}\
         <p style=\"color:#9a9aa6\">The certificate is replaycut's own, so the browser warns \
         once. Import <code>ca.crt</code> from the certificate folder to stop it - \
         Settings &rsaquo; Access names the path.</p></div>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sans(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_certificate_covers_every_way_in() {
        let out = wanted_sans(
            "gaming-pc",
            "gaming-pc",
            &sans(&["replay.example"]),
            Some(Ipv4Addr::new(192, 168, 1, 5)),
        );
        for expected in [
            "127.0.0.1",
            "192.168.1.5",
            "::1",
            "gaming-pc",
            "gaming-pc.local",
            "localhost",
            "replay.example",
        ] {
            assert!(
                out.contains(&expected.to_string()),
                "{expected} missing from {out:?}"
            );
        }
        let mut sorted = out.clone();
        sorted.sort();
        assert_eq!(out, sorted, "the list has to be sorted to be comparable");
        let mut unique = out.clone();
        unique.dedup();
        assert_eq!(out, unique, "no duplicates");
    }

    #[test]
    fn a_name_that_is_already_a_dotted_name_or_an_address_gets_no_local_suffix() {
        // Linux hands out `<host>.local` or the address itself; neither may
        // grow another suffix.
        let out = wanted_sans("box", "box.local", &[], Some(Ipv4Addr::new(10, 0, 0, 2)));
        assert!(out.contains(&"box.local".to_string()));
        assert!(!out.iter().any(|n| n == "box.local.local"));
        let out = wanted_sans("box", "10.0.0.2", &[], None);
        assert!(!out.iter().any(|n| n.ends_with("10.0.0.2.local")));
    }

    #[test]
    fn the_hostname_is_folded_and_trimmed() {
        let out = wanted_sans("Gaming-PC.", " gaming-pc ", &[], None);
        assert_eq!(
            out.iter().filter(|n| n.as_str() == "gaming-pc").count(),
            1,
            "the same name twice is one entry: {out:?}"
        );
    }

    #[test]
    fn a_certificate_is_kept_while_it_fits() {
        let now = Utc::now();
        let want = sans(&["localhost", "gaming-pc"]);
        assert!(!needs_reissue(
            &want,
            &want,
            now + chrono::Duration::days(200),
            now
        ));
    }

    #[test]
    fn a_new_address_and_a_near_expiry_both_force_a_new_certificate() {
        let now = Utc::now();
        let have = sans(&["gaming-pc", "localhost"]);
        let want = sans(&["192.168.1.5", "gaming-pc", "localhost"]);
        assert!(
            needs_reissue(&have, &want, now + chrono::Duration::days(200), now),
            "a new IP has to reach the certificate"
        );
        assert!(
            needs_reissue(&have, &have, now + chrono::Duration::days(29), now),
            "29 days left is inside the renewal window"
        );
        assert!(!needs_reissue(
            &have,
            &have,
            now + chrono::Duration::days(31),
            now
        ));
    }

    #[test]
    fn the_authority_signs_a_certificate_for_this_machine() {
        let (ca_pem, ca_key) = make_ca().unwrap();
        let want = sans(&["127.0.0.1", "gaming-pc", "localhost"]);
        let (leaf_pem, _, not_after) = issue_leaf(&ca_pem, &ca_key, &want).unwrap();
        let der = CertificateDer::from_pem_slice(leaf_pem.as_bytes()).unwrap();
        let (have, until) = read_leaf(&der).unwrap();
        assert_eq!(have, want, "the certificate carries exactly what was asked");
        assert!(until > Utc::now() + chrono::Duration::days(700));
        assert_eq!(until, not_after);
        assert!(!needs_reissue(&have, &want, until, Utc::now()));
    }

    #[test]
    fn the_authority_outlives_the_certificates_it_signs() {
        // What the whole split is for: a client pins the CA once, and every
        // re-issued leaf keeps working under it.
        let (ca_pem, ca_key) = make_ca().unwrap();
        let ca_der = CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
        let first = fingerprint(&ca_der);
        let a = issue_leaf(&ca_pem, &ca_key, &sans(&["localhost"])).unwrap();
        let b = issue_leaf(&ca_pem, &ca_key, &sans(&["localhost", "10.0.0.2"])).unwrap();
        assert_ne!(a.0, b.0, "a new address means a new certificate");
        assert_eq!(
            first,
            fingerprint(&CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap()),
            "the fingerprint a client pins does not move"
        );
    }

    #[test]
    fn a_certificate_and_its_key_make_a_server_config() {
        let (ca_pem, ca_key) = make_ca().unwrap();
        let (leaf_pem, leaf_key, _) = issue_leaf(&ca_pem, &ca_key, &sans(&["localhost"])).unwrap();
        let chain = vec![
            CertificateDer::from_pem_slice(leaf_pem.as_bytes()).unwrap(),
            CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap(),
        ];
        let key = PrivateKeyDer::from_pem_slice(leaf_key.as_bytes()).unwrap();
        let config = server_config(chain, key).unwrap();
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn the_first_bytes_say_what_the_connection_is() {
        assert_eq!(classify(&[0x16, 0x03, 0x01, 0x00]), Kind::Tls);
        assert_eq!(classify(b"GET /api/clips HTTP/1.1"), Kind::Plain);
        assert_eq!(classify(b"POST /api/share"), Kind::Plain);
        assert_eq!(classify(b"OPTIONS *"), Kind::Plain);
        assert_eq!(classify(b"\x00\x01\x02\x03"), Kind::Unknown);
        // Too little to tell: better silence than a guess.
        assert_eq!(classify(b"GE"), Kind::Unknown);
        assert_eq!(classify(b""), Kind::Unknown);
    }

    #[test]
    fn the_host_header_is_read_but_not_trusted() {
        let head = "GET / HTTP/1.1\r\nHost: gaming-pc:8420\r\nAccept: */*\r\n\r\n";
        assert_eq!(host_header(head), Some("gaming-pc:8420"));
        assert_eq!(
            host_header("GET / HTTP/1.1\r\nhost: [::1]:8420\r\n\r\n"),
            Some("[::1]:8420")
        );
        assert_eq!(host_header("GET / HTTP/1.1\r\n\r\n"), None);
        // A header is whatever the caller typed, and it ends up in a link.
        assert_eq!(
            host_header("GET / HTTP/1.1\r\nHost: a\"><script>alert(1)</script>\r\n\r\n"),
            None
        );
    }

    #[test]
    fn the_plaintext_answer_points_at_the_https_address() {
        let page = plaintext_page(Some("gaming-pc:8420"));
        assert!(page.contains("https://gaming-pc:8420/"));
        assert!(page.contains("speaks HTTPS"));
        // Without a usable host there is nothing to link to, but the page
        // still has to say what to do.
        let page = plaintext_page(None);
        assert!(!page.contains("href"));
        assert!(page.contains("https://"));
    }
}
