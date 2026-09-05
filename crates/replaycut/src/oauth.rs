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

/// Registered with Microsoft by the maintainer; empty until then (the UI
/// then says the build has no OneDrive client). `REPLAYCUT_ONEDRIVE_CLIENT_ID`
/// overrides it for tests.
pub const ONEDRIVE_CLIENT_ID: &str = "";
const MS_LOGIN_BASE: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";
const TIMEOUT: Duration = Duration::from_secs(20);

/// An OAuth provider with a device-code flow.
#[derive(Debug, Clone)]
pub struct DeviceProvider {
    pub id: &'static str,
    pub label: &'static str,
    /// `<base>/devicecode` and `<base>/token`.
    pub login_base: String,
    pub client_id: String,
    pub scope: &'static str,
    /// Credential Manager target for the refresh token (user = account name).
    pub credential: &'static str,
}

/// The provider behind a target id, if it has one.
pub fn provider(id: &str) -> Option<DeviceProvider> {
    match id {
        "onedrive" => Some(DeviceProvider {
            id: "onedrive",
            label: "OneDrive",
            login_base: std::env::var("REPLAYCUT_MS_LOGIN_BASE")
                .unwrap_or_else(|_| MS_LOGIN_BASE.to_string()),
            client_id: std::env::var("REPLAYCUT_ONEDRIVE_CLIENT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| ONEDRIVE_CLIENT_ID.to_string()),
            scope: "Files.ReadWrite.AppFolder User.Read offline_access",
            credential: credentials::ONEDRIVE,
        }),
        _ => None,
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

pub async fn device_start(p: &DeviceProvider) -> Result<DeviceStart> {
    if p.client_id.is_empty() {
        bail!("this build has no {} client id", p.label);
    }
    let v: Value = client()?
        .post(format!("{}/devicecode", p.login_base))
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

pub async fn device_poll(p: &DeviceProvider, device_code: &str) -> Result<Poll> {
    let res = client()?
        .post(format!("{}/token", p.login_base))
        .form(&[
            ("client_id", p.client_id.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
        ])
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

pub async fn refresh(p: &DeviceProvider, refresh_token: &str) -> Result<Tokens> {
    let v: Value = client()?
        .post(format!("{}/token", p.login_base))
        .form(&[
            ("client_id", p.client_id.as_str()),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", p.scope),
        ])
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
    provider: DeviceProvider,
    account: String,
    refresh_token: Mutex<String>,
    access: Mutex<Option<(String, Instant)>>,
}

impl TokenSource {
    pub fn new(provider: DeviceProvider, account: String, refresh_token: String) -> Self {
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

/// A device flow the page can watch through `GET /api/oauth/<provider>`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Flow {
    pub user_code: String,
    pub verification_uri: String,
    #[serde(skip)]
    pub expires_at: Instant,
    #[serde(flatten)]
    pub status: FlowStatus,
}

pub type Flows = Mutex<HashMap<String, Flow>>;

/// Poll a started device flow to the end in the background; the outcome
/// lands in `state.oauth` and, on success, the refresh token in the
/// Credential Manager and the runtime is rebuilt. The HTTP handler calls
/// `device_start` itself so its answer can carry the code.
pub async fn run_started_flow(
    state: Arc<AppState>,
    p: DeviceProvider,
    start: DeviceStart,
    account_name: impl Fn(String) -> futures_util::future::BoxFuture<'static, Result<String>>,
) -> Result<()> {
    let expires_at = Instant::now() + Duration::from_secs(start.expires_in);
    state.oauth.lock().insert(
        p.id.to_string(),
        Flow {
            user_code: start.user_code.clone(),
            verification_uri: start.verification_uri.clone(),
            expires_at,
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
    let outcome: Result<String> = async {
        loop {
            tokio::time::sleep(Duration::from_secs(interval)).await;
            if Instant::now() > expires_at {
                bail!("the code expired before it was entered");
            }
            match device_poll(&p, &start.device_code).await? {
                Poll::Pending => {}
                Poll::SlowDown => interval += 5,
                Poll::Tokens(t) => {
                    let refresh = t
                        .refresh_token
                        .clone()
                        .ok_or_else(|| anyhow!("the provider sent no refresh token"))?;
                    let account = account_name(t.access_token.clone())
                        .await
                        .unwrap_or_else(|_| p.label.to_string());
                    credentials::write(p.credential, &account, &refresh)?;
                    return Ok(account);
                }
            }
        }
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
            tracing::warn!("{}: device flow failed: {e:#}", p.label);
            FlowStatus::Failed {
                error: format!("{e:#}"),
            }
        }
    };
    if let Some(f) = state.oauth.lock().get_mut(p.id) {
        f.status = status;
    }
    if outcome.is_ok() {
        if let Err(e) = state.rebuild_runtime().await {
            tracing::warn!("rebuild after {} connect: {e:#}", p.label);
        }
        state.tray_changed();
    }
    outcome.map(|_| ())
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

    fn test_provider(base: &str) -> DeviceProvider {
        DeviceProvider {
            id: "onedrive",
            label: "OneDrive",
            login_base: base.to_string(),
            client_id: "test-client".into(),
            scope: "Files.ReadWrite.AppFolder offline_access",
            credential: "replaycut/test-oauth",
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

    #[tokio::test]
    async fn a_build_without_client_id_refuses_to_start() {
        let mut p = test_provider("http://127.0.0.1:1");
        p.client_id = String::new();
        let err = device_start(&p).await.unwrap_err();
        assert!(err.to_string().contains("client id"), "{err}");
    }
}
