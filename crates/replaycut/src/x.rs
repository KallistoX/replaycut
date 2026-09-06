//! X through the v2 API (since 2.6): every share is a post with the video
//! attached. The video goes up through the chunked media upload
//! (initialize, append, finalize, then polling until X has processed it),
//! the post carries the text from the template, and the link is
//! `https://x.com/<user>/status/<id>`. The account is the replaycut app's
//! client (public, PKCE) plus the refresh token of the connected account.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::integrations::{PublishMeta, Published};
use crate::oauth::TokenSource;

const API_BASE: &str = "https://api.x.com";
/// X takes at most 5 MB per append.
const CHUNK: u64 = 4 * 1024 * 1024;
/// The post text limit.
const TEXT_MAX: usize = 280;
/// How long to wait for X to process the video.
const PROCESSING_TIMEOUT: Duration = Duration::from_secs(300);

pub struct X {
    api: String,
    tokens: Arc<TokenSource>,
    client: reqwest::Client,
    text: String,
}

pub fn api_base() -> String {
    std::env::var("REPLAYCUT_X_API_BASE").unwrap_or_else(|_| API_BASE.to_string())
}

/// The post text from the template (`{title}`, `{clip}`, `{date}`); an
/// empty result falls back to the clip name, and X's limit applies.
pub fn post_text(
    template: &str,
    title: &str,
    base: &str,
    display_name: &str,
    date: &str,
) -> String {
    let title = if title.trim().is_empty() {
        crate::share::post_label(display_name, base, "")
    } else {
        title.trim().to_string()
    };
    let mut t = template
        .replace("{title}", &title)
        .replace("{clip}", base)
        .replace("{date}", date)
        .trim()
        .to_string();
    if t.is_empty() {
        t = title;
    }
    if t.chars().count() > TEXT_MAX {
        t = t
            .chars()
            .take(TEXT_MAX - 1)
            .collect::<String>()
            .trim_end()
            .to_string()
            + "…";
    }
    t
}

/// The link of a post.
pub fn status_url(username: &str, id: &str) -> String {
    format!(
        "https://x.com/{}/status/{id}",
        username.trim_start_matches('@')
    )
}

impl X {
    pub fn new(tokens: Arc<TokenSource>, text: &str) -> Result<Self> {
        Ok(Self {
            api: api_base(),
            tokens,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
            text: text.to_string(),
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

    /// A JSON answer, or the API's error text.
    async fn json(res: reqwest::Response, what: &str) -> Result<Value> {
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("X {what}: HTTP {status} {}", api_error(&v));
        }
        Ok(v)
    }

    /// `@username` of the signed-in account (`GET /2/users/me`).
    pub async fn username(api: &str, access_token: &str) -> Result<String> {
        let v: Value = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?
            .get(format!("{api}/2/users/me"))
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()
            .context("X users/me")?
            .json()
            .await?;
        username_of(&v)
    }

    /// The connected account, for the card and the diagnostics.
    pub async fn account(&self) -> Result<String> {
        let res = self.call(|c, a| c.get(format!("{a}/2/users/me"))).await?;
        username_of(&Self::json(res, "users/me").await?)
    }

    pub async fn publish(&self, file: &Path, meta: &PublishMeta) -> Result<Published> {
        let text = post_text(
            &self.text,
            &meta.title,
            &meta.base,
            &meta.display_name,
            &crate::youtube::date_of(&meta.base, &meta.at),
        );
        let mut f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let total = f.metadata().await?.len();
        // 1. initialize
        let res = self
            .call(|c, a| {
                c.post(format!("{a}/2/media/upload/initialize"))
                    .json(&serde_json::json!({
                        "media_type": "video/mp4",
                        "total_bytes": total,
                        "media_category": "tweet_video"
                    }))
            })
            .await?;
        let init = Self::json(res, "media initialize").await?;
        let media_id = init["data"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("no media id in the initialize answer"))?
            .to_string();
        // 2. append the chunks
        let mut offset = 0u64;
        let mut segment = 0u32;
        while offset < total {
            let len = CHUNK.min(total - offset);
            let mut buf = vec![0u8; len as usize];
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            f.read_exact(&mut buf).await?;
            let res = self
                .call(|c, a| {
                    let part = reqwest::multipart::Part::bytes(buf.clone())
                        .file_name("chunk")
                        .mime_str("application/octet-stream")
                        .unwrap_or_else(|_| reqwest::multipart::Part::bytes(buf.clone()));
                    c.post(format!("{a}/2/media/upload/{media_id}/append"))
                        .multipart(
                            reqwest::multipart::Form::new()
                                .text("segment_index", segment.to_string())
                                .part("media", part),
                        )
                })
                .await?;
            let status = res.status();
            if !status.is_success() {
                bail!(
                    "X media append: HTTP {status} {}",
                    api_error(&res.json().await.unwrap_or(Value::Null))
                );
            }
            offset += len;
            segment += 1;
        }
        // 3. finalize, then wait for the processing
        let res = self
            .call(|c, a| c.post(format!("{a}/2/media/upload/{media_id}/finalize")))
            .await?;
        let mut info = Self::json(res, "media finalize").await?;
        let started = std::time::Instant::now();
        loop {
            let state = info["data"]["processing_info"]["state"]
                .as_str()
                .unwrap_or("succeeded");
            match state {
                "succeeded" => break,
                "failed" => bail!(
                    "X could not process the video: {}",
                    info["data"]["processing_info"]["error"]["message"]
                        .as_str()
                        .unwrap_or("unknown reason")
                ),
                _ => {}
            }
            if started.elapsed() > PROCESSING_TIMEOUT {
                bail!("X did not finish processing the video within 5 minutes");
            }
            let wait = info["data"]["processing_info"]["check_after_secs"]
                .as_u64()
                .unwrap_or(2)
                .clamp(1, 30);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            let res = self
                .call(|c, a| {
                    c.get(format!("{a}/2/media/upload"))
                        .query(&[("command", "STATUS"), ("media_id", media_id.as_str())])
                })
                .await?;
            info = Self::json(res, "media status").await?;
        }
        // 4. the post
        let res = self
            .call(|c, a| {
                c.post(format!("{a}/2/tweets")).json(&serde_json::json!({
                    "text": text,
                    "media": { "media_ids": [media_id] }
                }))
            })
            .await?;
        let post = Self::json(res, "tweets").await?;
        let id = post["data"]["id"]
            .as_str()
            .ok_or_else(|| anyhow!("the post answer carries no id"))?
            .to_string();
        let link = status_url(self.tokens.account(), &id);
        Ok(Published {
            page: link.clone(),
            direct: link,
            path: id,
        })
    }

    /// `DELETE /2/tweets/<id>`; unknown ids do not count as errors.
    pub async fn delete(&self, ids: &[String]) -> Result<usize> {
        let mut n = 0;
        for id in ids {
            if id.is_empty() {
                continue;
            }
            let res = self
                .call(|c, a| c.delete(format!("{a}/2/tweets/{id}")))
                .await?;
            match res.status().as_u16() {
                200 | 204 => n += 1,
                404 => {}
                s => bail!(
                    "X delete {id}: HTTP {s} {}",
                    api_error(&res.json().await.unwrap_or(Value::Null))
                ),
            }
        }
        Ok(n)
    }
}

fn username_of(v: &Value) -> Result<String> {
    v["data"]["username"]
        .as_str()
        .map(|u| format!("@{u}"))
        .ok_or_else(|| anyhow!("the answer carries no username"))
}

/// The message of an X API error body, or nothing.
fn api_error(v: &Value) -> String {
    v["detail"]
        .as_str()
        .or_else(|| v["errors"][0]["message"].as_str())
        .or_else(|| v["title"].as_str())
        .or_else(|| v["error_description"].as_str())
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
    use axum::extract::{Path as AxPath, Query};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// The X login: authorization page that sends the browser straight back
    /// with a code, token endpoint for a public client (no secret) with
    /// PKCE, refresh rotates the refresh token.
    pub(crate) async fn fake_x_login() -> (String, tokio::task::JoinHandle<()>) {
        let challenge = Arc::new(parking_lot::Mutex::new(String::new()));
        let challenge_auth = challenge.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/authorize",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    let challenge = challenge_auth.clone();
                    async move {
                        assert_eq!(q.get("client_id").map(String::as_str), Some("x-client"));
                        assert!(q.get("scope").is_some_and(|s| s.contains("media.write")));
                        *challenge.lock() = q.get("code_challenge").cloned().unwrap_or_default();
                        let to = format!(
                            "{}?code=XCODE&state={}",
                            q.get("redirect_uri").cloned().unwrap_or_default(),
                            q.get("state").cloned().unwrap_or_default()
                        );
                        (StatusCode::FOUND, [(axum::http::header::LOCATION, to)])
                    }
                }),
            )
            .route(
                "/token",
                post(move |Form(f): Form<HashMap<String, String>>| {
                    let challenge = challenge.clone();
                    async move {
                        assert_eq!(f.get("client_id").map(String::as_str), Some("x-client"));
                        assert!(!f.contains_key("client_secret"));
                        match f.get("grant_type").map(String::as_str) {
                            Some("authorization_code") => {
                                assert_eq!(f.get("code").map(String::as_str), Some("XCODE"));
                                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                                assert_eq!(crate::oauth::pkce_challenge(&verifier), *challenge.lock());
                                Json(serde_json::json!({
                                    "access_token": "XAT1", "refresh_token": "XRT1", "expires_in": 7200
                                }))
                            }
                            Some("refresh_token") => {
                                assert_eq!(f.get("refresh_token").map(String::as_str), Some("XRT1"));
                                Json(serde_json::json!({
                                    "access_token": "XAT2", "refresh_token": "XRT2", "expires_in": 7200
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

    pub(crate) fn x_provider(base: &str) -> Provider {
        Provider {
            id: "x",
            label: "X",
            login_base: base.to_string(),
            device_path: "",
            auth_url: format!("{base}/authorize"),
            loopback: true,
            client_id: "x-client".into(),
            client_secret: None,
            scope: "tweet.read tweet.write users.read media.write offline.access",
            credential: "replaycut/test-x",
            missing_client: "no client",
        }
    }

    /// The X API: the user, the chunked upload with one status poll, the
    /// post and its deletion.
    pub(crate) async fn fake_x_api() -> (String, tokio::task::JoinHandle<()>) {
        let received = Arc::new(AtomicU64::new(0));
        let polls = Arc::new(AtomicU64::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let auth_ok = |h: &HeaderMap| {
            h.get("authorization").map(|v| v.to_str().unwrap_or("")) == Some("Bearer XAT2")
        };
        let app = Router::new()
            .route(
                "/2/users/me",
                get(move |h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    Json(serde_json::json!({ "data": { "id": "1", "name": "Tester", "username": "tester" } }))
                }),
            )
            .route(
                "/2/media/upload/initialize",
                post(move |h: HeaderMap, Json(body): Json<Value>| async move {
                    assert!(auth_ok(&h));
                    assert_eq!(body["media_category"], "tweet_video");
                    assert_eq!(body["total_bytes"], 9 * 1024 * 1024 + 5);
                    Json(serde_json::json!({ "data": { "id": "MEDIA1", "expires_after_secs": 3600 } }))
                }),
            )
            .route(
                "/2/media/upload/{id}/append",
                post(move |AxPath(id): AxPath<String>, h: HeaderMap, body: Bytes| {
                    let received = received.clone();
                    async move {
                        assert!(auth_ok(&h));
                        assert_eq!(id, "MEDIA1");
                        // a multipart body: the segment index and the chunk (not parsed here)
                        let text = String::from_utf8_lossy(&body[..body.len().min(400)]).into_owned();
                        assert!(text.contains("name=\"segment_index\""), "{text}");
                        assert!(text.contains("name=\"media\""), "{text}");
                        received.fetch_add(body.len() as u64, Ordering::Relaxed);
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/2/media/upload/{id}/finalize",
                post(move |AxPath(id): AxPath<String>, h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    assert_eq!(id, "MEDIA1");
                    Json(serde_json::json!({ "data": { "id": "MEDIA1", "processing_info": { "state": "pending", "check_after_secs": 1 } } }))
                }),
            )
            .route(
                "/2/media/upload",
                get(move |Query(q): Query<HashMap<String, String>>, h: HeaderMap| {
                    let polls = polls.clone();
                    async move {
                        assert!(auth_ok(&h));
                        assert_eq!(q.get("command").map(String::as_str), Some("STATUS"));
                        assert_eq!(q.get("media_id").map(String::as_str), Some("MEDIA1"));
                        let state = if polls.fetch_add(1, Ordering::Relaxed) == 0 { "in_progress" } else { "succeeded" };
                        Json(serde_json::json!({ "data": { "id": "MEDIA1", "processing_info": { "state": state, "check_after_secs": 1 } } }))
                    }
                }),
            )
            .route(
                "/2/tweets",
                post(move |h: HeaderMap, Json(body): Json<Value>| async move {
                    assert!(auth_ok(&h));
                    assert_eq!(body["text"], "Nice shot");
                    assert_eq!(body["media"]["media_ids"][0], "MEDIA1");
                    (StatusCode::CREATED, Json(serde_json::json!({ "data": { "id": "1234567890", "text": "Nice shot" } })))
                }),
            )
            .route(
                "/2/tweets/{id}",
                axum::routing::delete(move |AxPath(id): AxPath<String>, h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    if id == "1234567890" {
                        (StatusCode::OK, Json(serde_json::json!({ "data": { "deleted": true } }))).into_response()
                    } else {
                        StatusCode::NOT_FOUND.into_response()
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
    async fn loopback_login_with_a_public_client() {
        let (base, _srv) = fake_x_login().await;
        let p = x_provider(&base);
        let redirect = "http://127.0.0.1:8420/oauth/x/callback";
        let (url, pending) = crate::oauth::loopback_start(&p, redirect).unwrap();
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
        let code = to.split("code=").nth(1).unwrap().split('&').next().unwrap();
        let t = crate::oauth::loopback_exchange(&p, &pending, code)
            .await
            .unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("XRT1"));
        let t = crate::oauth::refresh(&p, "XRT1").await.unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("XRT2"));
    }

    #[tokio::test]
    async fn publish_uploads_in_chunks_waits_and_posts() {
        let (login, _l) = fake_x_login().await;
        let (api, _a) = fake_x_api().await;
        let tokens = Arc::new(TokenSource::new(
            x_provider(&login),
            "@tester".into(),
            "XRT1".into(),
        ));
        let x = X {
            api,
            tokens,
            client: reqwest::Client::new(),
            text: "{title}".into(),
        };
        let dir = std::env::temp_dir().join(format!("rc-x-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("Replay_2026-09-04_1-9_Nice-shot.mp4");
        std::fs::write(&file, vec![3u8; 9 * 1024 * 1024 + 5]).unwrap();
        let meta = PublishMeta {
            month: "2026-09".into(),
            title: "Nice shot".into(),
            base: "Replay 2026-09-04 11-40-00".into(),
            display_name: "replaycut".into(),
            vertical: false,
            at: "2026-09-05T10:00:00".into(),
        };
        let p = x.publish(&file, &meta).await.unwrap();
        assert_eq!(p.page, "https://x.com/tester/status/1234567890");
        assert_eq!(p.direct, p.page);
        assert_eq!(p.path, "1234567890");
        assert_eq!(x.account().await.unwrap(), "@tester");
        assert_eq!(
            x.delete(&["1234567890".into(), "missing".into(), String::new()])
                .await
                .unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn post_texts() {
        assert_eq!(
            post_text(
                "{title}",
                "Ace",
                "Replay 2026-09-04",
                "WARDOGS",
                "2026-09-04"
            ),
            "Ace"
        );
        assert_eq!(
            post_text(
                "{title}",
                "",
                "WARDOGS 2026-09-04 23-26-58",
                "WARDOGS",
                "2026-09-04"
            ),
            "2026-09-04 23-26-58"
        );
        assert_eq!(
            post_text("{title} #wardogs {date}", "Ace", "R", "", "2026-09-04"),
            "Ace #wardogs 2026-09-04"
        );
        assert_eq!(post_text("   ", "Ace", "R", "", "d"), "Ace");
        let long = post_text("{title}", &"a".repeat(300), "R", "", "d");
        assert_eq!(long.chars().count(), 280);
        assert!(long.ends_with('…'));
        assert_eq!(status_url("@tester", "1"), "https://x.com/tester/status/1");
    }
}

#[cfg(test)]
mod fake_servers {
    /// Not a test: serves the fake X (login on 8486, API on 8487) for trying
    /// the X card against a dev instance started with
    /// `REPLAYCUT_X_CLIENT_ID=x-client REPLAYCUT_X_LOGIN_BASE=http://127.0.0.1:8486
    /// REPLAYCUT_X_AUTH_URL=http://127.0.0.1:8486/authorize
    /// REPLAYCUT_X_API_BASE=http://127.0.0.1:8487`. Run with
    /// `cargo test -p replaycut serve_fake_x -- --ignored --nocapture`, stop with Ctrl+C.
    #[tokio::test]
    #[ignore]
    async fn serve_fake_x() {
        use axum::body::Bytes;
        use axum::extract::{Path as AxPath, Query};
        use axum::http::StatusCode;
        use axum::routing::{delete, get, post};
        use axum::{Json, Router};
        use std::collections::HashMap;
        let login = Router::new()
            .route(
                "/authorize",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    let to = format!(
                        "{}?code=XCODE&state={}",
                        q.get("redirect_uri").cloned().unwrap_or_default(),
                        q.get("state").cloned().unwrap_or_default()
                    );
                    println!("browser login -> {to}");
                    (StatusCode::FOUND, [(axum::http::header::LOCATION, to)])
                }),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({ "access_token": "XAT-FAKE", "refresh_token": "XRT-FAKE", "expires_in": 7200 }))
                }),
            );
        let api = Router::new()
            .route(
                "/2/users/me",
                get(|| async { Json(serde_json::json!({ "data": { "id": "1", "name": "Fake", "username": "fake_tester" } })) }),
            )
            .route(
                "/2/media/upload/initialize",
                post(|Json(body): Json<serde_json::Value>| async move {
                    println!("media initialize: {body}");
                    Json(serde_json::json!({ "data": { "id": "MEDIA-FAKE", "expires_after_secs": 3600 } }))
                }),
            )
            .route(
                "/2/media/upload/{id}/append",
                post(|AxPath(_id): AxPath<String>, body: Bytes| async move {
                    println!("append: {} bytes", body.len());
                    StatusCode::NO_CONTENT
                }),
            )
            .route(
                "/2/media/upload/{id}/finalize",
                post(|| async { Json(serde_json::json!({ "data": { "id": "MEDIA-FAKE", "processing_info": { "state": "succeeded" } } })) }),
            )
            .route(
                "/2/tweets",
                post(|Json(body): Json<serde_json::Value>| async move {
                    println!("post: {body}");
                    (StatusCode::CREATED, Json(serde_json::json!({ "data": { "id": "9876543210", "text": body["text"] } })))
                }),
            )
            .route("/2/tweets/{id}", delete(|| async { Json(serde_json::json!({ "data": { "deleted": true } })) }));
        let api = api.layer(axum::extract::DefaultBodyLimit::disable());
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:8486")
            .await
            .unwrap();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:8487")
            .await
            .unwrap();
        println!("fake X: login http://127.0.0.1:8486, API http://127.0.0.1:8487");
        tokio::join!(async { axum::serve(l1, login).await.unwrap() }, async {
            axum::serve(l2, api).await.unwrap()
        });
    }
}
