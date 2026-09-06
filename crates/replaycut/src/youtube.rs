//! YouTube through the Data API v3 (since 2.6): every share becomes its own
//! video, uploaded through a resumable session in chunks, unlisted unless
//! the settings say otherwise. The link is `https://youtu.be/<id>`. The
//! account is the user's own Google client (quota: 1600 units per upload
//! out of 10 000 a day per project, so a shared client would be dead after
//! six uploads) plus the refresh token of the connected channel.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::integrations::{PublishMeta, Published};
use crate::oauth::TokenSource;

const API_BASE: &str = "https://www.googleapis.com";
/// Chunks must be multiples of 256 KiB; 10 MiB keeps the request count low.
const CHUNK: u64 = 10 * 1024 * 1024;
/// YouTube's limit for `snippet.title`.
const TITLE_MAX: usize = 100;

pub struct YouTube {
    api: String,
    tokens: Arc<TokenSource>,
    client: reqwest::Client,
    privacy: String,
    description: String,
}

pub fn api_base() -> String {
    std::env::var("REPLAYCUT_YOUTUBE_API_BASE").unwrap_or_else(|_| API_BASE.to_string())
}

/// The video title: the clip's title, else the clip name without the display
/// name prefix; `#Shorts` for a vertical cut. At most 100 characters and
/// without `<` and `>`, which YouTube refuses.
pub fn video_title(title: &str, base: &str, display_name: &str, vertical: bool) -> String {
    let mut t = if title.trim().is_empty() {
        crate::share::post_label(display_name, base, "")
    } else {
        title.trim().to_string()
    };
    t = t.replace(['<', '>'], "");
    if vertical && !t.to_lowercase().contains("#shorts") {
        t.push_str(" #Shorts");
    }
    if t.chars().count() > TITLE_MAX {
        let keep = if vertical {
            TITLE_MAX - " #Shorts".len()
        } else {
            TITLE_MAX
        };
        let head: String = t.chars().take(keep).collect();
        t = head.trim_end().to_string();
        if vertical {
            t.push_str(" #Shorts");
        }
    }
    t
}

/// The description from the template: `{title}`, `{clip}` and `{date}`.
pub fn render_description(template: &str, title: &str, base: &str, date: &str) -> String {
    template
        .replace("{title}", title.trim())
        .replace("{clip}", base)
        .replace("{date}", date)
        .replace(['<', '>'], "")
        .trim()
        .to_string()
}

/// `YYYY-MM-DD` from the clip name, else from the job's timestamp.
pub fn date_of(base: &str, at: &str) -> String {
    let b = base.as_bytes();
    if b.len() >= 10 {
        for i in 0..=b.len() - 10 {
            let w = &b[i..i + 10];
            let digits = [0, 1, 2, 3, 5, 6, 8, 9]
                .iter()
                .all(|&k| w[k].is_ascii_digit());
            if digits && w[4] == b'-' && w[7] == b'-' {
                return String::from_utf8_lossy(w).into_owned();
            }
        }
    }
    at.chars().take(10).collect()
}

impl YouTube {
    pub fn new(tokens: Arc<TokenSource>, privacy: &str, description: &str) -> Result<Self> {
        Ok(Self {
            api: api_base(),
            tokens,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
            privacy: privacy.to_string(),
            description: description.to_string(),
        })
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.tokens.access().await?))
    }

    /// An API request with the access token; one retry after a 401.
    async fn call(
        &self,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let res = build(&self.client, &self.api)
            .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
            .send()
            .await?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.tokens.invalidate();
            let res = build(&self.client, &self.api)
                .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
                .send()
                .await?;
            return Ok(res);
        }
        Ok(res)
    }

    /// Title of the signed-in user's channel (`channels.list mine=true`, 1 unit).
    pub async fn channel_title(api: &str, access_token: &str) -> Result<String> {
        let v: Value = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?
            .get(format!("{api}/youtube/v3/channels"))
            .query(&[("part", "snippet"), ("mine", "true")])
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()
            .context("YouTube channels.list")?
            .json()
            .await?;
        channel_of(&v)
    }

    /// The connected channel, for the card and the diagnostics.
    pub async fn account(&self) -> Result<String> {
        let res = self
            .call(|c, a| {
                c.get(format!("{a}/youtube/v3/channels"))
                    .query(&[("part", "snippet"), ("mine", "true")])
            })
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!(
                "YouTube channels.list: HTTP {status} {}",
                api_error(&res.json().await.unwrap_or(Value::Null))
            );
        }
        channel_of(&res.json().await?)
    }

    pub async fn publish(&self, file: &Path, meta: &PublishMeta) -> Result<Published> {
        let title = video_title(&meta.title, &meta.base, &meta.display_name, meta.vertical);
        let description = render_description(
            &self.description,
            &meta.title,
            &meta.base,
            &date_of(&meta.base, &meta.at),
        );
        let mut f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let total = f.metadata().await?.len();
        // 1. the resumable session with the metadata
        let body = serde_json::json!({
            "snippet": { "title": title, "description": description, "categoryId": "20" },
            "status": { "privacyStatus": self.privacy, "selfDeclaredMadeForKids": false }
        });
        let res = self
            .call(|c, a| {
                c.post(format!("{a}/upload/youtube/v3/videos"))
                    .query(&[("uploadType", "resumable"), ("part", "snippet,status")])
                    .header("X-Upload-Content-Type", "video/mp4")
                    .header("X-Upload-Content-Length", total)
                    .json(&body)
            })
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!(
                "YouTube upload session: HTTP {status} {}",
                api_error(&res.json().await.unwrap_or(Value::Null))
            );
        }
        let upload_url = res
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| anyhow!("no upload location in the session answer"))?
            .to_string();
        // 2. the chunks; 308 means "send the next one"
        let mut offset = 0u64;
        let mut video: Value = Value::Null;
        while offset < total {
            let len = CHUNK.min(total - offset);
            let mut buf = vec![0u8; len as usize];
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            f.read_exact(&mut buf).await?;
            let res = self
                .client
                .put(&upload_url)
                .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
                .header(reqwest::header::CONTENT_TYPE, "video/mp4")
                .header(reqwest::header::CONTENT_LENGTH, len)
                .header(
                    reqwest::header::CONTENT_RANGE,
                    format!("bytes {offset}-{}/{total}", offset + len - 1),
                )
                .body(buf)
                .send()
                .await
                .context("upload chunk")?;
            let status = res.status();
            if status.as_u16() == 308 {
                offset += len;
                continue;
            }
            if !status.is_success() {
                bail!(
                    "YouTube upload: HTTP {status} {}",
                    api_error(&res.json().await.unwrap_or(Value::Null))
                );
            }
            offset += len;
            video = res.json().await.unwrap_or(Value::Null);
        }
        let id = video["id"]
            .as_str()
            .ok_or_else(|| anyhow!("the upload finished without a video id"))?
            .to_string();
        let link = format!("https://youtu.be/{id}");
        Ok(Published {
            page: link.clone(),
            direct: link,
            path: id,
        })
    }

    /// `videos.delete` (50 units each); unknown ids do not count as errors.
    pub async fn delete(&self, ids: &[String]) -> Result<usize> {
        let mut n = 0;
        for id in ids {
            if id.is_empty() {
                continue;
            }
            let res = self
                .call(|c, a| {
                    c.delete(format!("{a}/youtube/v3/videos"))
                        .query(&[("id", id.as_str())])
                })
                .await?;
            match res.status().as_u16() {
                200 | 204 => n += 1,
                404 => {}
                s => bail!(
                    "YouTube delete {id}: HTTP {s} {}",
                    api_error(&res.json().await.unwrap_or(Value::Null))
                ),
            }
        }
        Ok(n)
    }
}

fn channel_of(v: &Value) -> Result<String> {
    v["items"][0]["snippet"]["title"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the account has no YouTube channel"))
}

/// The message of a Google API error body, or nothing.
fn api_error(v: &Value) -> String {
    v["error"]["message"]
        .as_str()
        .or_else(|| v["error"]["errors"][0]["reason"].as_str())
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::oauth::Provider;
    use axum::body::Bytes;
    use axum::extract::Query;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{delete as ax_delete, get, post, put};
    use axum::{Form, Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A Google login: `device/code`, then the token endpoint that wants the
    /// client secret, answers pending with 428 and refreshes without a new
    /// refresh token, as Google does.
    pub(crate) async fn fake_google_login() -> (String, tokio::task::JoinHandle<()>) {
        let polls = Arc::new(AtomicU64::new(0));
        // the PKCE challenge the browser login announced, checked at the token exchange
        let challenge = Arc::new(parking_lot::Mutex::new(String::new()));
        let challenge_auth = challenge.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/auth",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let challenge = challenge_auth.clone();
                    async move {
                        assert_eq!(q.get("client_id").map(String::as_str), Some("gid.apps"));
                        assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
                        assert_eq!(q.get("access_type").map(String::as_str), Some("offline"));
                        assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
                        *challenge.lock() = q.get("code_challenge").cloned().unwrap_or_default();
                        let to = format!(
                            "{}?code=GCODE&state={}",
                            q.get("redirect_uri").cloned().unwrap_or_default(),
                            q.get("state").cloned().unwrap_or_default()
                        );
                        (StatusCode::FOUND, [(axum::http::header::LOCATION, to)])
                    }
                }),
            )
            .route(
                "/device/code",
                post(|Form(f): Form<HashMap<String, String>>| async move {
                    assert_eq!(f.get("client_id").map(String::as_str), Some("gid.apps"));
                    assert_eq!(
                        f.get("scope").map(String::as_str),
                        Some("https://www.googleapis.com/auth/youtube")
                    );
                    Json(serde_json::json!({
                        "device_code": "GDEV", "user_code": "WXYZ-1234",
                        "verification_url": "https://www.google.com/device",
                        "interval": 0, "expires_in": 1800
                    }))
                }),
            )
            .route(
                "/token",
                post(move |Form(f): Form<HashMap<String, String>>| {
                    let polls = polls.clone();
                    let challenge = challenge.clone();
                    async move {
                        assert_eq!(f.get("client_id").map(String::as_str), Some("gid.apps"));
                        assert_eq!(f.get("client_secret").map(String::as_str), Some("gsecret"));
                        match f.get("grant_type").map(String::as_str) {
                            Some("authorization_code") => {
                                assert_eq!(f.get("code").map(String::as_str), Some("GCODE"));
                                assert!(f.get("redirect_uri").is_some_and(|r| r.ends_with("/oauth/youtube/callback")));
                                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                                assert_eq!(crate::oauth::pkce_challenge(&verifier), *challenge.lock(), "PKCE verifier");
                                (
                                    StatusCode::OK,
                                    Json(serde_json::json!({
                                        "access_token": "GAT1", "refresh_token": "GRT1", "expires_in": 3599
                                    })),
                                )
                            }
                            Some("urn:ietf:params:oauth:grant-type:device_code") => {
                                assert_eq!(f.get("device_code").map(String::as_str), Some("GDEV"));
                                if polls.fetch_add(1, Ordering::Relaxed) < 1 {
                                    (
                                        StatusCode::PRECONDITION_REQUIRED,
                                        Json(serde_json::json!({ "error": "authorization_pending" })),
                                    )
                                } else {
                                    (
                                        StatusCode::OK,
                                        Json(serde_json::json!({
                                            "access_token": "GAT1", "refresh_token": "GRT1", "expires_in": 3599
                                        })),
                                    )
                                }
                            }
                            Some("refresh_token") => {
                                assert_eq!(f.get("refresh_token").map(String::as_str), Some("GRT1"));
                                (
                                    StatusCode::OK,
                                    Json(serde_json::json!({ "access_token": "GAT2", "expires_in": 3599 })),
                                )
                            }
                            _ => (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({ "error": "unsupported_grant_type" })),
                            ),
                        }
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, task)
    }

    pub(crate) fn google_provider(base: &str) -> Provider {
        Provider {
            id: "youtube",
            label: "YouTube",
            login_base: base.to_string(),
            device_path: "device/code",
            auth_url: format!("{base}/auth"),
            loopback: false,
            client_id: "gid.apps".into(),
            client_secret: Some("gsecret".into()),
            scope: "https://www.googleapis.com/auth/youtube",
            credential: "replaycut/test-youtube",
            missing_client: "no client",
        }
    }

    /// A YouTube API: channel of the user, resumable session, chunks with
    /// 308 until the last one, delete.
    pub(crate) async fn fake_youtube() -> (String, tokio::task::JoinHandle<()>) {
        let received = Arc::new(AtomicU64::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let up = base.clone();
        let auth_ok = |h: &HeaderMap| {
            h.get("authorization").map(|v| v.to_str().unwrap_or("")) == Some("Bearer GAT2")
        };
        let app = Router::new()
            .route(
                "/youtube/v3/channels",
                get(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    assert_eq!(q.get("mine").map(String::as_str), Some("true"));
                    Json(serde_json::json!({ "items": [{ "snippet": { "title": "Test Channel" } }] }))
                }),
            )
            .route(
                "/upload/youtube/v3/videos",
                post(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap, Json(body): Json<Value>| {
                    let up = up.clone();
                    async move {
                        assert!(auth_ok(&h));
                        assert_eq!(q.get("uploadType").map(String::as_str), Some("resumable"));
                        assert_eq!(body["snippet"]["title"], "Nice shot #Shorts");
                        assert_eq!(body["status"]["privacyStatus"], "unlisted");
                        assert!(body["snippet"]["description"].as_str().unwrap().contains("2026-09-04"));
                        assert_eq!(
                            h.get("x-upload-content-length").unwrap().to_str().unwrap(),
                            (3 * 1024 * 1024 + 7).to_string()
                        );
                        (
                            StatusCode::OK,
                            [(axum::http::header::LOCATION, format!("{up}/upload/session1?upload_id=abc"))],
                        )
                    }
                }),
            )
            .route(
                "/upload/session1",
                put(move |h: HeaderMap, body: Bytes| {
                    let received = received.clone();
                    async move {
                        assert!(auth_ok(&h));
                        let range = h.get("content-range").unwrap().to_str().unwrap().to_string();
                        let total: u64 = range.rsplit('/').next().unwrap().parse().unwrap();
                        let got = received.fetch_add(body.len() as u64, Ordering::Relaxed) + body.len() as u64;
                        if got >= total {
                            (StatusCode::OK, Json(serde_json::json!({ "id": "vid123xyz00" }))).into_response()
                        } else {
                            (StatusCode::PERMANENT_REDIRECT, [("Range", format!("bytes=0-{}", got - 1))]).into_response()
                        }
                    }
                }),
            )
            .route(
                "/youtube/v3/videos",
                ax_delete(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    if q.get("id").map(String::as_str) == Some("vid123xyz00") {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::NOT_FOUND
                    }
                }),
            );
        let app = app.layer(axum::extract::DefaultBodyLimit::disable());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, task)
    }

    use axum::response::IntoResponse;

    #[tokio::test]
    async fn google_device_flow_wants_the_secret_and_refreshes() {
        let (base, _srv) = fake_google_login().await;
        let p = google_provider(&base);
        let start = crate::oauth::device_start(&p).await.unwrap();
        assert_eq!(start.user_code, "WXYZ-1234");
        assert_eq!(start.verification_uri, "https://www.google.com/device");
        assert!(matches!(
            crate::oauth::device_poll(&p, &start.device_code)
                .await
                .unwrap(),
            crate::oauth::Poll::Pending
        ));
        match crate::oauth::device_poll(&p, &start.device_code)
            .await
            .unwrap()
        {
            crate::oauth::Poll::Tokens(t) => assert_eq!(t.refresh_token.as_deref(), Some("GRT1")),
            _ => panic!("expected tokens"),
        }
        let t = crate::oauth::refresh(&p, "GRT1").await.unwrap();
        assert_eq!(t.access_token, "GAT2");
        assert!(t.refresh_token.is_none());
    }

    #[tokio::test]
    async fn loopback_login_exchanges_the_code_with_pkce() {
        let (base, _srv) = fake_google_login().await;
        let mut p = google_provider(&base);
        p.loopback = true;
        let redirect = "http://127.0.0.1:8420/oauth/youtube/callback";
        let (url, pending) = crate::oauth::loopback_start(&p, redirect).unwrap();
        // the browser would follow this; here the redirect is read by hand
        let res = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(&url)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 302);
        let to = res.headers()["location"].to_str().unwrap().to_string();
        assert!(to.starts_with(redirect), "{to}");
        let query: HashMap<String, String> = to
            .split_once('?')
            .unwrap()
            .1
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(query["state"], pending.state);
        let t = crate::oauth::loopback_exchange(&p, &pending, &query["code"])
            .await
            .unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("GRT1"));
        // a wrong verifier is refused by the fake as Google would
        let bad = crate::oauth::LoopbackPending {
            verifier: "nope".into(),
            ..pending.clone()
        };
        assert!(crate::oauth::loopback_exchange(&p, &bad, "GCODE")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn publish_uploads_resumably_and_links_youtu_be() {
        let (login, _l) = fake_google_login().await;
        let (api, _y) = fake_youtube().await;
        let tokens = Arc::new(TokenSource::new(
            google_provider(&login),
            "Test Channel".into(),
            "GRT1".into(),
        ));
        let yt = YouTube {
            api,
            tokens,
            client: reqwest::Client::new(),
            privacy: "unlisted".into(),
            description: "{title}\n{clip} on {date}".into(),
        };
        let dir = std::env::temp_dir().join(format!("rc-youtube-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Replay_2026-09-04_1-9_Nice-shot_9x16.mp4");
        std::fs::write(&file, vec![9u8; 3 * 1024 * 1024 + 7]).unwrap();
        let meta = PublishMeta {
            month: "2026-09".into(),
            title: "Nice shot".into(),
            base: "Replay 2026-09-04 11-40-00".into(),
            display_name: "replaycut".into(),
            vertical: true,
            at: "2026-09-05T10:00:00".into(),
            seconds: 8.0,
        };
        let p = yt.publish(&file, &meta).await.unwrap();
        assert_eq!(p.page, "https://youtu.be/vid123xyz00");
        assert_eq!(p.direct, p.page);
        assert_eq!(p.path, "vid123xyz00");
        assert_eq!(yt.account().await.unwrap(), "Test Channel");
        assert_eq!(
            yt.delete(&["vid123xyz00".into(), "missing".into(), String::new()])
                .await
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn titles_and_descriptions() {
        assert_eq!(
            video_title("", "WARDOGS 2026-09-04 23-26-58", "WARDOGS", false),
            "2026-09-04 23-26-58"
        );
        assert_eq!(video_title(" Ace ", "x", "", true), "Ace #Shorts");
        assert_eq!(video_title("Ace #shorts", "x", "", true), "Ace #shorts");
        assert_eq!(video_title("<b>Ace</b>", "x", "", false), "bAce/b");
        let long = video_title(&"a".repeat(120), "x", "", true);
        assert!(
            long.chars().count() <= 100 && long.ends_with(" #Shorts"),
            "{long}"
        );
        assert_eq!(
            video_title(&"a".repeat(120), "x", "", false)
                .chars()
                .count(),
            100
        );
        assert_eq!(
            render_description(
                "{title}\n\nClip from {date}, shared with replaycut.",
                "Ace",
                "Replay 2026-09-04",
                "2026-09-04"
            ),
            "Ace\n\nClip from 2026-09-04, shared with replaycut."
        );
        assert_eq!(render_description("{title}{clip}", "", "R", "d"), "R");
        assert_eq!(
            date_of("Replay 2026-09-04 11-40-00", "2026-09-05T10:00:00"),
            "2026-09-04"
        );
        assert_eq!(date_of("clip", "2026-09-05T10:00:00"), "2026-09-05");
    }
}

#[cfg(test)]
mod fake_servers {
    /// Not a test: serves the fake Google (login on 8484, YouTube API on
    /// 8485) for trying the YouTube card against a dev instance started with
    /// `REPLAYCUT_GOOGLE_LOGIN_BASE=http://127.0.0.1:8484
    /// REPLAYCUT_GOOGLE_AUTH_URL=http://127.0.0.1:8484/auth
    /// REPLAYCUT_YOUTUBE_API_BASE=http://127.0.0.1:8485` (store any client id
    /// and secret in the card first; `/auth` sends the browser straight
    /// back with a code, so the desktop client type can be tried too). Run with
    /// `cargo test -p replaycut serve_fake_google -- --ignored --nocapture`, stop with Ctrl+C.
    #[tokio::test]
    #[ignore]
    async fn serve_fake_google() {
        use axum::body::Bytes;
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::{delete, get, post, put};
        use axum::{Json, Router};
        let login = Router::new()
            .route(
                "/auth",
                get(|axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                    let to = format!(
                        "{}?code=GCODE&state={}",
                        q.get("redirect_uri").cloned().unwrap_or_default(),
                        q.get("state").cloned().unwrap_or_default()
                    );
                    println!("browser login -> {to}");
                    (StatusCode::FOUND, [(axum::http::header::LOCATION, to)])
                }),
            )
            .route(
                "/device/code",
                post(|| async {
                    Json(serde_json::json!({
                        "device_code": "GDEV", "user_code": "WXYZ-1234",
                        "verification_url": "https://www.google.com/device",
                        "interval": 2, "expires_in": 1800
                    }))
                }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({ "access_token": "GAT-FAKE", "refresh_token": "GRT-FAKE", "expires_in": 3599 }))
                }),
            );
        let api = Router::new()
            .route(
                "/youtube/v3/channels",
                get(|| async { Json(serde_json::json!({ "items": [{ "snippet": { "title": "Fake Channel" } }] })) }),
            )
            .route(
                "/upload/youtube/v3/videos",
                post(|Json(body): Json<serde_json::Value>| async move {
                    println!("upload session: {}", body);
                    (StatusCode::OK, [(axum::http::header::LOCATION, "http://127.0.0.1:8485/upload/s")])
                }),
            )
            .route(
                "/upload/s",
                put(|h: HeaderMap, body: Bytes| async move {
                    let range = h.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("bytes 0-0/1").to_string();
                    let total: u64 = range.rsplit('/').next().and_then(|t| t.parse().ok()).unwrap_or(0);
                    let end: u64 = range.trim_start_matches("bytes ").split('/').next().and_then(|r| r.split('-').nth(1)).and_then(|e| e.parse().ok()).unwrap_or(0);
                    let _ = body;
                    if end + 1 >= total {
                        (StatusCode::OK, Json(serde_json::json!({ "id": "fakeVideo01" }))).into_response()
                    } else {
                        StatusCode::PERMANENT_REDIRECT.into_response()
                    }
                }),
            )
            .route("/youtube/v3/videos", delete(|| async { StatusCode::NO_CONTENT }));
        let api = api.layer(axum::extract::DefaultBodyLimit::disable());
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:8484")
            .await
            .unwrap();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:8485")
            .await
            .unwrap();
        println!("fake Google: login http://127.0.0.1:8484, YouTube API http://127.0.0.1:8485");
        tokio::join!(async { axum::serve(l1, login).await.unwrap() }, async {
            axum::serve(l2, api).await.unwrap()
        });
    }
}
