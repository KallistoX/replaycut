//! OAuth for the account integrations (since 2.5): the device-code flow
//! (the page shows a code, the user types it in at the provider on any
//! device, the service polls for the tokens) and token refresh. The refresh
//! token lives in the Credential Manager, the access token in memory. Client
//! IDs of installed apps are public by design; PKCE is not needed for the
//! device flow.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;

use crate::credentials;
use crate::state::AppState;

/// The replaycut app registration in the maintainer's Entra tenant (public
/// client, no secret). `REPLAYCUT_ONEDRIVE_CLIENT_ID` overrides it for tests;
/// an empty value makes the card say the build has no client.
pub const ONEDRIVE_CLIENT_ID: &str = "985879f2-d85e-4032-bdfa-665b08b8734a";
// `common`: the app is registered for personal and work accounts alike.
const MS_LOGIN_BASE: &str = "https://login.microsoftonline.com/common/oauth2/v2.0";
/// Google's device flow: `<base>/device/code` and `<base>/token`. It needs a
/// client of the type "TVs and Limited Input devices" and sends the client
/// secret along; the allowed scopes include `youtube` but not
/// `youtube.upload`.
const GOOGLE_LOGIN_BASE: &str = "https://oauth2.googleapis.com";
/// Google's authorization endpoint for the loopback flow (a "Desktop app"
/// client); `REPLAYCUT_GOOGLE_AUTH_URL` points it at a fake.
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TIMEOUT: Duration = Duration::from_secs(20);

/// An OAuth provider: the device-code flow (the default) or, since 2.6, the
/// loopback flow with PKCE (the browser on this PC is sent to the provider
/// and comes back to `http://127.0.0.1:<port>/oauth/<provider>/callback`).
#[derive(Debug, Clone)]
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    /// `<base>/<device_path>` and `<base>/token`.
    pub login_base: String,
    /// `devicecode` (Microsoft) or `device/code` (Google).
    pub device_path: &'static str,
    /// The authorization endpoint of the loopback flow.
    pub auth_url: String,
    /// Connect through the browser redirect instead of a code.
    pub loopback: bool,
    pub client_id: String,
    /// Sent with the token requests when the provider wants it (Google).
    pub client_secret: Option<String>,
    pub scope: &'static str,
    /// Credential Manager target for the refresh token (user = account name).
    pub credential: &'static str,
    /// What the card says when `client_id` is empty.
    pub missing_client: &'static str,
}

/// The provider behind a target id, if it has one. OneDrive uses the
/// replaycut app registration; YouTube the user's own Google client from
/// the Credential Manager (since 2.6), so `client_id` is empty until one is
/// stored, and its `clientType` setting decides the flow.
pub fn provider(id: &str, settings: &crate::settings::Settings) -> Option<Provider> {
    match id {
        "onedrive" => Some(Provider {
            id: "onedrive",
            label: "OneDrive",
            login_base: std::env::var("REPLAYCUT_MS_LOGIN_BASE")
                .unwrap_or_else(|_| MS_LOGIN_BASE.to_string()),
            device_path: "devicecode",
            auth_url: format!("{MS_LOGIN_BASE}/authorize"),
            loopback: false,
            client_id: std::env::var("REPLAYCUT_ONEDRIVE_CLIENT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| ONEDRIVE_CLIENT_ID.to_string()),
            client_secret: None,
            scope: "Files.ReadWrite.AppFolder User.Read offline_access",
            credential: credentials::ONEDRIVE,
            missing_client: "this build has no OneDrive client id",
        }),
        "youtube" => {
            let client = credentials::read(credentials::YOUTUBE_CLIENT)
                .ok()
                .flatten()
                .filter(|c| !c.user.trim().is_empty() && !c.secret.trim().is_empty());
            Some(Provider {
                id: "youtube",
                label: "YouTube",
                login_base: std::env::var("REPLAYCUT_GOOGLE_LOGIN_BASE")
                    .unwrap_or_else(|_| GOOGLE_LOGIN_BASE.to_string()),
                device_path: "device/code",
                auth_url: std::env::var("REPLAYCUT_GOOGLE_AUTH_URL")
                    .unwrap_or_else(|_| GOOGLE_AUTH_URL.to_string()),
                loopback: settings.integrations.youtube.client_type == "desktop",
                client_id: client
                    .as_ref()
                    .map(|c| c.user.trim().to_string())
                    .unwrap_or_default(),
                client_secret: client.map(|c| c.secret.trim().to_string()),
                scope: "https://www.googleapis.com/auth/youtube",
                credential: credentials::YOUTUBE,
                missing_client: "no Google client stored - enter client id and client secret under Settings > Integrations > YouTube",
            })
        }
        _ => None,
    }
}

// ------------------------------------------------------------ loopback flow (since 2.6)

/// What the callback needs to finish a loopback login.
#[derive(Debug, Clone)]
pub struct LoopbackPending {
    pub state: String,
    pub verifier: String,
    pub redirect_uri: String,
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random_urlsafe(bytes: usize) -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    base64url(&buf)
}

/// The PKCE challenge of a verifier (S256).
pub fn pkce_challenge(verifier: &str) -> String {
    use sha2::Digest;
    base64url(&sha2::Sha256::digest(verifier.as_bytes()))
}

/// The URL the browser is sent to, plus what the callback needs. Google
/// only hands out a refresh token with `access_type=offline` and
/// `prompt=consent`.
pub fn loopback_start(p: &Provider, redirect_uri: &str) -> Result<(String, LoopbackPending)> {
    if p.client_id.is_empty() {
        bail!("{}", p.missing_client);
    }
    let pending = LoopbackPending {
        state: random_urlsafe(24),
        verifier: random_urlsafe(48),
        redirect_uri: redirect_uri.to_string(),
    };
    let q = |s: &str| crate::util::encode_path_segment(s);
    let url = format!(
        "{}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}&code_challenge={}&code_challenge_method=S256",
        p.auth_url,
        q(&p.client_id),
        q(redirect_uri),
        q(p.scope),
        q(&pending.state),
        q(&pkce_challenge(&pending.verifier)),
    );
    Ok((url, pending))
}

/// Exchange the code from the callback for tokens.
pub async fn loopback_exchange(
    p: &Provider,
    pending: &LoopbackPending,
    code: &str,
) -> Result<Tokens> {
    let mut form = p.client_form();
    form.push(("grant_type", "authorization_code".to_string()));
    form.push(("code", code.to_string()));
    form.push(("redirect_uri", pending.redirect_uri.clone()));
    form.push(("code_verifier", pending.verifier.clone()));
    let v: Value = client()?
        .post(format!("{}/token", p.login_base))
        .form(&form)
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = v["error"].as_str() {
        bail!(
            "{e}: {}",
            v["error_description"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        );
    }
    parse_tokens(&v)
}

impl Provider {
    /// The form fields every token request starts with.
    fn client_form(&self) -> Vec<(&'static str, String)> {
        let mut f = vec![("client_id", self.client_id.clone())];
        if let Some(s) = &self.client_secret {
            f.push(("client_secret", s.clone()));
        }
        f
    }
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// What the provider hands out at the start of a device flow.
#[derive(Debug, Clone)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub interval: u64,
    pub expires_in: u64,
}

#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
}

pub enum Poll {
    Pending,
    SlowDown,
    Tokens(Tokens),
}

pub async fn device_start(p: &Provider) -> Result<DeviceStart> {
    if p.client_id.is_empty() {
        bail!("{}", p.missing_client);
    }
    let v: Value = client()?
        .post(format!("{}/{}", p.login_base, p.device_path))
        .form(&[("client_id", p.client_id.as_str()), ("scope", p.scope)])
        .send()
        .await?
        .error_for_status()
        .context("device code request")?
        .json()
        .await?;
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Ok(DeviceStart {
        device_code: s("device_code").ok_or_else(|| anyhow!("no device_code"))?,
        user_code: s("user_code").ok_or_else(|| anyhow!("no user_code"))?,
        verification_uri: s("verification_uri")
            .or_else(|| s("verification_url"))
            .ok_or_else(|| anyhow!("no verification_uri"))?,
        interval: v["interval"].as_u64().unwrap_or(5).max(1),
        expires_in: v["expires_in"].as_u64().unwrap_or(900),
    })
}

fn parse_tokens(v: &Value) -> Result<Tokens> {
    Ok(Tokens {
        access_token: v["access_token"]
            .as_str()
            .ok_or_else(|| anyhow!("no access_token"))?
            .to_string(),
        refresh_token: v["refresh_token"].as_str().map(str::to_string),
        expires_in: v["expires_in"].as_u64().unwrap_or(3600),
    })
}

pub async fn device_poll(p: &Provider, device_code: &str) -> Result<Poll> {
    let mut form = p.client_form();
    form.push((
        "grant_type",
        "urn:ietf:params:oauth:grant-type:device_code".to_string(),
    ));
    form.push(("device_code", device_code.to_string()));
    // Google answers pending polls with HTTP 428 and a JSON body; the body decides
    let res = client()?
        .post(format!("{}/token", p.login_base))
        .form(&form)
        .send()
        .await?;
    let v: Value = res.json().await.unwrap_or(Value::Null);
    match v["error"].as_str() {
        None => Ok(Poll::Tokens(parse_tokens(&v)?)),
        Some("authorization_pending") => Ok(Poll::Pending),
        Some("slow_down") => Ok(Poll::SlowDown),
        Some(other) => bail!(
            "{other}: {}",
            v["error_description"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        ),
    }
}

pub async fn refresh(p: &Provider, refresh_token: &str) -> Result<Tokens> {
    let mut form = p.client_form();
    form.push(("grant_type", "refresh_token".to_string()));
    form.push(("refresh_token", refresh_token.to_string()));
    form.push(("scope", p.scope.to_string()));
    let v: Value = client()?
        .post(format!("{}/token", p.login_base))
        .form(&form)
        .send()
        .await?
        .json()
        .await?;
    if let Some(e) = v["error"].as_str() {
        bail!(
            "token refresh failed: {e}: {}",
            v["error_description"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
        );
    }
    parse_tokens(&v)
}

/// Access tokens for one connected account: refreshed on demand, the new
/// refresh token written back to the Credential Manager.
pub struct TokenSource {
    provider: Provider,
    account: String,
    refresh_token: Mutex<String>,
    access: Mutex<Option<(String, Instant)>>,
}

impl TokenSource {
    pub fn new(provider: Provider, account: String, refresh_token: String) -> Self {
        Self {
            provider,
            account,
            refresh_token: Mutex::new(refresh_token),
            access: Mutex::new(None),
        }
    }

    pub fn account(&self) -> &str {
        &self.account
    }

    /// A valid access token, refreshing when the cached one is about to expire.
    pub async fn access(&self) -> Result<String> {
        if let Some((token, until)) = self.access.lock().clone() {
            if until > Instant::now() + Duration::from_secs(60) {
                return Ok(token);
            }
        }
        let current = self.refresh_token.lock().clone();
        let tokens = refresh(&self.provider, &current).await?;
        if let Some(rt) = &tokens.refresh_token {
            if *rt != current {
                *self.refresh_token.lock() = rt.clone();
                if let Err(e) = credentials::write(self.provider.credential, &self.account, rt) {
                    tracing::warn!(
                        "{}: cannot store the new refresh token: {e:#}",
                        self.provider.label
                    );
                }
            }
        }
        *self.access.lock() = Some((
            tokens.access_token.clone(),
            Instant::now() + Duration::from_secs(tokens.expires_in),
        ));
        Ok(tokens.access_token)
    }

    /// Forget the cached access token (after a 401).
    pub fn invalidate(&self) {
        *self.access.lock() = None;
    }
}

// ------------------------------------------------------------ flows in the service

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "status")]
pub enum FlowStatus {
    Pending,
    Done { account: String },
    Failed { error: String },
}

/// A login in progress the page can watch through `GET /api/oauth/<provider>`:
/// a device flow (code and link) or a loopback flow (the link only; the
/// callback finishes it).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flow {
    pub user_code: String,
    pub verification_uri: String,
    #[serde(skip)]
    pub expires_at: Instant,
    #[serde(skip)]
    pub loopback: Option<LoopbackPending>,
    #[serde(flatten)]
    pub status: FlowStatus,
}

pub type Flows = Mutex<HashMap<String, Flow>>;

/// The account name once the tokens are there, per provider.
pub type AccountLookup =
    Box<dyn Fn(String) -> futures_util::future::BoxFuture<'static, Result<String>> + Send + Sync>;

/// Store the tokens of a finished login: the refresh token in the Credential
/// Manager under the account's name, the outcome in the flow, the runtime
/// rebuilt so the storage becomes a target.
async fn finish_login(
    state: &AppState,
    p: &Provider,
    tokens: Result<Tokens>,
    account_name: &AccountLookup,
) -> Result<String> {
    let outcome: Result<String> = async {
        let t = tokens?;
        let refresh = t
            .refresh_token
            .clone()
            .ok_or_else(|| anyhow!("the provider sent no refresh token"))?;
        let account = account_name(t.access_token.clone())
            .await
            .unwrap_or_else(|_| p.label.to_string());
        credentials::write(p.credential, &account, &refresh)?;
        Ok(account)
    }
    .await;
    let status = match &outcome {
        Ok(account) => {
            tracing::info!("{}: connected as {account}", p.label);
            FlowStatus::Done {
                account: account.clone(),
            }
        }
        Err(e) => {
            tracing::warn!("{}: login failed: {e:#}", p.label);
            FlowStatus::Failed {
                error: format!("{e:#}"),
            }
        }
    };
    if let Some(f) = state.oauth.lock().get_mut(p.id) {
        f.status = status;
        f.loopback = None;
    }
    if outcome.is_ok() {
        if let Err(e) = state.rebuild_runtime().await {
            tracing::warn!("rebuild after {} connect: {e:#}", p.label);
        }
        state.tray_changed();
    }
    outcome
}

/// Begin a loopback flow: remembers state and verifier, returns the URL the
/// browser must open. The flow expires after ten minutes.
pub fn start_loopback_flow(state: &AppState, p: &Provider, redirect_uri: &str) -> Result<String> {
    let (url, pending) = loopback_start(p, redirect_uri)?;
    state.oauth.lock().insert(
        p.id.to_string(),
        Flow {
            user_code: String::new(),
            verification_uri: url.clone(),
            expires_at: Instant::now() + Duration::from_secs(600),
            loopback: Some(pending),
            status: FlowStatus::Pending,
        },
    );
    tracing::info!("{}: browser login started", p.label);
    Ok(url)
}

/// `GET /oauth/<provider>/callback`: the code comes back from the provider.
/// Checks the state, exchanges the code and stores the account.
pub async fn finish_loopback_flow(
    state: &AppState,
    p: &Provider,
    code: &str,
    state_param: &str,
    account_name: AccountLookup,
) -> Result<String> {
    let pending = {
        let flows = state.oauth.lock();
        let flow = flows
            .get(p.id)
            .ok_or_else(|| anyhow!("no {} login is waiting", p.label))?;
        let pending = flow
            .loopback
            .clone()
            .ok_or_else(|| anyhow!("no {} browser login is waiting", p.label))?;
        if flow.expires_at < Instant::now() {
            bail!("the {} login expired - start it again", p.label);
        }
        if pending.state != state_param {
            bail!(
                "the login answer does not belong to the waiting {} login",
                p.label
            );
        }
        pending
    };
    let tokens = loopback_exchange(p, &pending, code).await;
    finish_login(state, p, tokens, &account_name).await
}

/// Poll a started device flow to the end in the background; the outcome
/// lands in `state.oauth` and, on success, the refresh token in the
/// Credential Manager and the runtime is rebuilt. The HTTP handler calls
/// `device_start` itself so its answer can carry the code.
pub async fn run_started_flow(
    state: Arc<AppState>,
    p: Provider,
    start: DeviceStart,
    account_name: AccountLookup,
) -> Result<()> {
    let expires_at = Instant::now() + Duration::from_secs(start.expires_in);
    state.oauth.lock().insert(
        p.id.to_string(),
        Flow {
            user_code: start.user_code.clone(),
            verification_uri: start.verification_uri.clone(),
            expires_at,
            loopback: None,
            status: FlowStatus::Pending,
        },
    );
    tracing::info!(
        "{}: device flow started, code {} at {}",
        p.label,
        start.user_code,
        start.verification_uri
    );
    let mut interval = start.interval;
    let tokens: Result<Tokens> = async {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if Instant::now() > expires_at {
                bail!("the code expired before it was entered");
            }
            match device_poll(&p, &start.device_code).await? {
                Poll::Pending => {}
                Poll::SlowDown => interval += 5,
                Poll::Tokens(t) => return Ok(t),
            }
        }
    }
    .await;
    finish_login(&state, &p, tokens, &account_name)
        .await
        .map(|_| ())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::{routing::post, Form, Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A login server: device code, then pending twice, then tokens; refresh
    /// hands out a rotated refresh token.
    pub(crate) async fn fake_login() -> (String, tokio::task::JoinHandle<()>) {
        let polls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/devicecode",
                post(|Form(f): Form<HashMap<String, String>>| async move {
                    assert_eq!(f.get("client_id").map(String::as_str), Some("test-client"));
                    Json(serde_json::json!({
                        "device_code": "DEV123", "user_code": "ABCD-EFGH",
                        "verification_uri": "https://example.com/devicelogin",
                        "interval": 0, "expires_in": 60
                    }))
                }),
            )
            .route(
                "/token",
                post(move |Form(f): Form<HashMap<String, String>>| {
                    let polls = polls.clone();
                    async move {
                        match f.get("grant_type").map(String::as_str) {
                            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                                assert_eq!(f.get("device_code").map(String::as_str), Some("DEV123"));
                                let n = polls.fetch_add(1, Ordering::Relaxed);
                                if n < 2 {
                                    Json(serde_json::json!({ "error": "authorization_pending" }))
                                } else {
                                    Json(serde_json::json!({
                                        "access_token": "AT1", "refresh_token": "RT1", "expires_in": 3600
                                    }))
                                }
                            }
                            Some("refresh_token") => {
                                assert_eq!(f.get("refresh_token").map(String::as_str), Some("RT1"));
                                Json(serde_json::json!({
                                    "access_token": "AT2", "refresh_token": "RT2", "expires_in": 3600
                                }))
                            }
                            _ => Json(serde_json::json!({ "error": "unsupported_grant_type" })),
                        }
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, task)
    }

    pub(crate) fn test_provider(base: &str) -> Provider {
        Provider {
            id: "onedrive",
            label: "OneDrive",
            login_base: base.to_string(),
            device_path: "devicecode",
            auth_url: format!("{base}/authorize"),
            loopback: false,
            client_id: "test-client".into(),
            client_secret: None,
            scope: "Files.ReadWrite.AppFolder offline_access",
            credential: "replaycut/test-oauth",
            missing_client: "no client",
        }
    }

    #[tokio::test]
    async fn device_flow_polls_until_the_tokens_arrive() {
        let (base, _srv) = fake_login().await;
        let p = test_provider(&base);
        let start = device_start(&p).await.unwrap();
        assert_eq!(start.user_code, "ABCD-EFGH");
        assert!(matches!(
            device_poll(&p, &start.device_code).await.unwrap(),
            Poll::Pending
        ));
        assert!(matches!(
            device_poll(&p, &start.device_code).await.unwrap(),
            Poll::Pending
        ));
        match device_poll(&p, &start.device_code).await.unwrap() {
            Poll::Tokens(t) => {
                assert_eq!(t.access_token, "AT1");
                assert_eq!(t.refresh_token.as_deref(), Some("RT1"));
            }
            _ => panic!("expected tokens"),
        }
        let t = refresh(&p, "RT1").await.unwrap();
        assert_eq!(t.access_token, "AT2");
        assert_eq!(t.refresh_token.as_deref(), Some("RT2"));
    }

    #[test]
    fn loopback_url_carries_pkce_and_state() {
        let p = test_provider("http://127.0.0.1:1");
        let (url, pending) = loopback_start(&p, "http://127.0.0.1:8420/oauth/x/callback").unwrap();
        assert!(
            url.starts_with("http://127.0.0.1:1/authorize?client_id=test-client&"),
            "{url}"
        );
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A8420%2Foauth%2Fx%2Fcallback"),
            "{url}"
        );
        assert!(url.contains(&format!("state={}", pending.state)), "{url}");
        assert!(
            url.contains(&format!(
                "code_challenge={}&code_challenge_method=S256",
                pkce_challenge(&pending.verifier)
            )),
            "{url}"
        );
        assert!(url.contains("access_type=offline&prompt=consent"), "{url}");
        assert_eq!(pending.verifier.len(), 64);
        // RFC 7636 appendix B
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let mut none = p.clone();
        none.client_id = String::new();
        assert!(loopback_start(&none, "http://127.0.0.1:1/cb").is_err());
    }

    #[tokio::test]
    async fn a_build_without_client_id_refuses_to_start() {
        let mut p = test_provider("http://127.0.0.1:1");
        p.client_id = String::new();
        let err = device_start(&p).await.unwrap_err();
        assert!(err.to_string().contains("no client"), "{err}");
    }
}
