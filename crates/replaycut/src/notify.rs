//! Notify integrations of 2.6 next to the Discord webhook: a Telegram bot
//! that posts the link into a chat or channel, and a generic webhook that
//! receives the share as JSON with an HMAC signature, so n8n, Home
//! Assistant, Zapier or a Matrix bridge can pick it up.

use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

/// What a finished share tells the notify integrations.
#[derive(Debug, Clone, Default)]
pub struct Notification {
    /// The Discord-style text: `**<prefix>** <label> (<n> s) - <link>`.
    pub text: String,
    /// The display name.
    pub prefix: String,
    /// `<title> - <clip>` or the clip name.
    pub label: String,
    pub title: String,
    pub base: String,
    pub seconds: f64,
    pub target: String,
    pub link: String,
    pub direct: String,
    pub at: String,
    pub job: String,
}

pub fn is_http_url(url: &str) -> bool {
    (url.starts_with("https://") || url.starts_with("http://"))
        && url.len() > 10
        && !url.contains(char::is_whitespace)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------------------
// Telegram

const TELEGRAM_API_BASE: &str = "https://api.telegram.org";

pub fn telegram_api_base() -> String {
    std::env::var("REPLAYCUT_TELEGRAM_API_BASE").unwrap_or_else(|_| TELEGRAM_API_BASE.to_string())
}

pub struct Telegram {
    api: String,
    token: String,
    chat_id: String,
    client: reqwest::Client,
}

impl Telegram {
    pub fn new(token: &str, chat_id: &str) -> Result<Self> {
        anyhow::ensure!(
            token.contains(':') && token.len() > 20 && !token.contains(char::is_whitespace),
            "this does not look like a Telegram bot token (123456:ABC-...)"
        );
        anyhow::ensure!(
            !chat_id.trim().is_empty(),
            "the chat id is empty - a number such as -1001234567890, or @channelname"
        );
        Ok(Self {
            api: telegram_api_base(),
            token: token.trim().to_string(),
            chat_id: chat_id.trim().to_string(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    async fn call(&self, method: &str, body: &Value) -> Result<Value> {
        let res = self
            .client
            .post(format!("{}/bot{}/{method}", self.api, self.token))
            .json(body)
            .send()
            .await?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() || v["ok"] != true {
            bail!(
                "Telegram {method}: HTTP {status} {}",
                v["description"].as_str().unwrap_or("")
            );
        }
        Ok(v["result"].clone())
    }

    /// The bot's user name (`getMe`).
    pub async fn me(&self) -> Result<String> {
        let r = self.call("getMe", &serde_json::json!({})).await?;
        r["username"]
            .as_str()
            .map(|u| format!("@{u}"))
            .ok_or_else(|| anyhow!("getMe answered without a username"))
    }

    /// Send HTML text to the chat; the link previews on its own.
    pub async fn send(&self, html: &str) -> Result<String> {
        let r = self
            .call(
                "sendMessage",
                &serde_json::json!({ "chat_id": self.chat_id, "text": html, "parse_mode": "HTML" }),
            )
            .await?;
        Ok(format!(
            "Link posted{}",
            r["chat"]["title"]
                .as_str()
                .map(|t| format!(" to {t}"))
                .unwrap_or_default()
        ))
    }

    pub async fn post(&self, n: &Notification) -> Result<String> {
        let html = format!(
            "<b>{}</b> {} ({} s)\n{}",
            html_escape(&n.prefix),
            html_escape(&n.label),
            n.seconds.round() as i64,
            html_escape(&n.direct)
        );
        self.send(&html).await
    }
}

// ---------------------------------------------------------------------------
// Generic webhook

pub struct Webhook {
    url: String,
    secret: Option<String>,
    client: reqwest::Client,
}

/// `sha256=<hex>` of the HMAC-SHA256 over the body.
pub fn signature(secret: &str, body: &[u8]) -> String {
    let mac = crate::s3::hmac_sha256(secret.as_bytes(), body);
    format!("sha256={}", crate::s3::hex(&mac))
}

impl Webhook {
    pub fn new(url: &str, secret: Option<String>) -> Result<Self> {
        anyhow::ensure!(
            is_http_url(url.trim()),
            "the webhook URL must start with http:// or https://"
        );
        Ok(Self {
            url: url.trim().to_string(),
            secret: secret.filter(|s| !s.is_empty()),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// POST the event as JSON; with a secret, `X-Replaycut-Signature`
    /// carries the HMAC of the exact body.
    pub async fn send(&self, event: &Value) -> Result<String> {
        let body = serde_json::to_vec(event)?;
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                "X-Replaycut-Event",
                event["event"].as_str().unwrap_or("shared").to_string(),
            );
        if let Some(secret) = &self.secret {
            req = req.header("X-Replaycut-Signature", signature(secret, &body));
        }
        let res = req.body(body).send().await?;
        let status = res.status();
        if !status.is_success() {
            let detail = res.text().await.unwrap_or_default();
            bail!(
                "webhook: HTTP {status} {}",
                detail
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect::<String>()
            );
        }
        Ok(format!("Posted (HTTP {})", status.as_u16()))
    }

    pub async fn post(&self, n: &Notification) -> Result<String> {
        self.send(&serde_json::json!({
            "event": "shared",
            "title": n.title,
            "clip": n.base,
            "seconds": n.seconds,
            "target": n.target,
            "link": n.link,
            "direct": n.direct,
            "at": n.at,
            "job": n.job,
            "displayName": n.prefix,
        }))
        .await
    }

    /// The test event of the settings card.
    pub async fn test(&self) -> Result<String> {
        self.send(&serde_json::json!({
            "event": "test",
            "at": crate::util::now_local(),
            "version": crate::state::VERSION,
        }))
        .await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::Path as AxPath;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};
    use std::sync::Arc;

    /// A Telegram Bot API: `getMe` and `sendMessage` for one token.
    pub(crate) async fn fake_telegram() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/bot{token}/{method}",
            post(
                |AxPath((token, method)): AxPath<(String, String)>, Json(body): Json<Value>| async move {
                    if token != "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11" {
                        return Json(serde_json::json!({ "ok": false, "description": "Unauthorized" }));
                    }
                    match method.as_str() {
                        "getMe" => Json(serde_json::json!({ "ok": true, "result": { "username": "replaycut_bot" } })),
                        "sendMessage" => {
                            assert_eq!(body["chat_id"], "-1001");
                            assert_eq!(body["parse_mode"], "HTML");
                            assert!(body["text"].as_str().unwrap().starts_with("<b>WARDOGS</b>"));
                            Json(serde_json::json!({ "ok": true, "result": { "message_id": 7, "chat": { "title": "Clips" } } }))
                        }
                        _ => Json(serde_json::json!({ "ok": false, "description": "Not Found" })),
                    }
                },
            ),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, task)
    }

    /// A receiver that checks the signature and records the events.
    pub(crate) async fn fake_receiver(
        secret: &'static str,
    ) -> (
        String,
        Arc<parking_lot::Mutex<Vec<Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let seen_h = seen.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/hook",
            post(move |h: HeaderMap, body: axum::body::Bytes| {
                let seen = seen_h.clone();
                async move {
                    let sig = h
                        .get("x-replaycut-signature")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    if !secret.is_empty() {
                        assert_eq!(sig, signature(secret, &body), "signature over the raw body");
                    } else {
                        assert!(sig.is_empty());
                    }
                    let v: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        h.get("x-replaycut-event").and_then(|x| x.to_str().ok()),
                        v["event"].as_str()
                    );
                    seen.lock().push(v);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, seen, task)
    }

    fn notification() -> Notification {
        Notification {
            text: "**WARDOGS** Ace - 2026-09-04 (6 s) - https://cloud.example.com/s/abc/download"
                .into(),
            prefix: "WARDOGS".into(),
            label: "Ace - 2026-09-04".into(),
            title: "Ace".into(),
            base: "WARDOGS 2026-09-04".into(),
            seconds: 6.0,
            target: "nextcloud".into(),
            link: "https://cloud.example.com/s/abc".into(),
            direct: "https://cloud.example.com/s/abc/download".into(),
            at: "2026-09-04T11:38:59".into(),
            job: "412b2e96".into(),
        }
    }

    #[tokio::test]
    async fn telegram_posts_html_to_the_chat() {
        let (base, _srv) = fake_telegram().await;
        let mut t = Telegram::new("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11", "-1001").unwrap();
        t.api = base.clone();
        assert_eq!(t.me().await.unwrap(), "@replaycut_bot");
        assert_eq!(
            t.post(&notification()).await.unwrap(),
            "Link posted to Clips"
        );
        let mut bad = Telegram::new("123456:wrong-token-wrong-token", "-1001").unwrap();
        bad.api = base;
        assert!(bad
            .me()
            .await
            .unwrap_err()
            .to_string()
            .contains("Unauthorized"));
        assert!(Telegram::new("nope", "-1001").is_err());
        assert!(Telegram::new("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11", " ").is_err());
    }

    #[tokio::test]
    async fn webhook_signs_the_body_and_sends_events() {
        let (base, seen, _srv) = fake_receiver("s3cret").await;
        let w = Webhook::new(&format!("{base}/hook"), Some("s3cret".into())).unwrap();
        assert_eq!(w.post(&notification()).await.unwrap(), "Posted (HTTP 204)");
        w.test().await.unwrap();
        let seen = seen.lock();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["event"], "shared");
        assert_eq!(seen[0]["title"], "Ace");
        assert_eq!(seen[0]["clip"], "WARDOGS 2026-09-04");
        assert_eq!(seen[0]["seconds"], 6.0);
        assert_eq!(seen[0]["target"], "nextcloud");
        assert_eq!(
            seen[0]["direct"],
            "https://cloud.example.com/s/abc/download"
        );
        assert_eq!(seen[1]["event"], "test");
        assert!(seen[1]["version"].is_string());
    }

    #[tokio::test]
    async fn webhook_without_secret_sends_no_signature() {
        let (base, seen, _srv) = fake_receiver("").await;
        let w = Webhook::new(&format!("{base}/hook"), Some(String::new())).unwrap();
        w.test().await.unwrap();
        assert_eq!(seen.lock().len(), 1);
        assert!(Webhook::new("ftp://example.com", None).is_err());
        assert!(Webhook::new("https://x", None).is_err());
    }

    #[test]
    fn signature_is_hmac_sha256_hex() {
        // RFC 4231 test case 2
        assert_eq!(
            signature("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
}
