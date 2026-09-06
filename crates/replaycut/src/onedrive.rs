//! OneDrive through Microsoft Graph (since 2.5): the file goes into the
//! app folder (`Apps/replaycut/<month>/`) through an upload session, then
//! an anonymous view link is created. Deletion by path for the clip's
//! delete dialog; account name and quota for the settings card.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::integrations::Published;
use crate::oauth::TokenSource;

const GRAPH_BASE: &str = "https://graph.microsoft.com/v1.0";
const CHUNK: u64 = 10 * 1024 * 1024;

pub struct OneDrive {
    graph: String,
    tokens: Arc<TokenSource>,
    client: reqwest::Client,
}

/// What the settings card and the diagnostics show.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub used: Option<u64>,
    pub total: Option<u64>,
}

pub fn graph_base() -> String {
    std::env::var("REPLAYCUT_GRAPH_BASE").unwrap_or_else(|_| GRAPH_BASE.to_string())
}

impl OneDrive {
    pub fn new(tokens: Arc<TokenSource>) -> Result<Self> {
        Ok(Self {
            graph: graph_base(),
            tokens,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// Path inside the app folder: `<month>/<file>`.
    pub fn remote_path(month: &str, file_name: &str) -> String {
        format!("{month}/{file_name}")
    }

    async fn bearer(&self) -> Result<String> {
        Ok(format!("Bearer {}", self.tokens.access().await?))
    }

    /// A Graph request with the access token; one retry after a 401.
    async fn graph(
        &self,
        build: impl Fn(&reqwest::Client, &str) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let res = build(&self.client, &self.graph)
            .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
            .send()
            .await?;
        if res.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.tokens.invalidate();
            let res = build(&self.client, &self.graph)
                .header(reqwest::header::AUTHORIZATION, self.bearer().await?)
                .send()
                .await?;
            return Ok(res);
        }
        Ok(res)
    }

    /// Display name of the signed-in user, for the card after connecting.
    pub async fn me(graph: &str, access_token: &str) -> Result<String> {
        let v: Value = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?
            .get(format!("{graph}/me"))
            .bearer_auth(access_token)
            .send()
            .await?
            .error_for_status()
            .context("Graph /me")?
            .json()
            .await?;
        Ok(v["displayName"]
            .as_str()
            .or_else(|| v["userPrincipalName"].as_str())
            .unwrap_or("OneDrive")
            .to_string())
    }

    /// Account name plus quota from `/me/drive`.
    pub async fn account(&self) -> Result<Account> {
        let res = self.graph(|c, g| c.get(format!("{g}/me/drive"))).await?;
        let status = res.status();
        if !status.is_success() {
            bail!("Graph /me/drive: HTTP {status}");
        }
        let v: Value = res.json().await?;
        Ok(Account {
            name: self.tokens.account().to_string(),
            used: v["quota"]["used"].as_u64(),
            total: v["quota"]["total"].as_u64(),
        })
    }

    pub async fn publish(&self, file: &Path, month: &str) -> Result<Published> {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let path = Self::remote_path(month, &name);
        // 1. the upload session (creates the folders on the way)
        let res = self
            .graph(|c, g| {
                c.post(format!(
                    "{g}/me/drive/special/approot:/{}:/createUploadSession",
                    encode_path(&path)
                ))
                .json(&serde_json::json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } }))
            })
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!(
                "OneDrive upload session: HTTP {status} {}",
                res.text().await.unwrap_or_default()
            );
        }
        let session: Value = res.json().await?;
        let upload_url = session["uploadUrl"]
            .as_str()
            .ok_or_else(|| anyhow!("no uploadUrl in the session"))?
            .to_string();
        // 2. the chunks (the upload URL carries its own auth)
        let mut f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let total = f.metadata().await?.len();
        let mut offset = 0u64;
        let mut item: Value = Value::Null;
        while offset < total {
            let len = CHUNK.min(total - offset);
            let mut buf = vec![0u8; len as usize];
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            f.read_exact(&mut buf).await?;
            let res = self
                .client
                .put(&upload_url)
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
            if !status.is_success() {
                bail!(
                    "OneDrive upload: HTTP {status} {}",
                    res.text().await.unwrap_or_default()
                );
            }
            offset += len;
            if offset >= total {
                item = res.json().await.unwrap_or(Value::Null);
            }
        }
        let id = item["id"]
            .as_str()
            .ok_or_else(|| anyhow!("the upload finished without an item id"))?
            .to_string();
        // 3. the anonymous view link
        let res = self
            .graph(|c, g| {
                c.post(format!("{g}/me/drive/items/{id}/createLink"))
                    .json(&serde_json::json!({ "type": "view", "scope": "anonymous" }))
            })
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!(
                "OneDrive createLink: HTTP {status} {}",
                res.text().await.unwrap_or_default()
            );
        }
        let link: Value = res.json().await?;
        let web = link["link"]["webUrl"]
            .as_str()
            .or_else(|| item["webUrl"].as_str())
            .ok_or_else(|| anyhow!("no link in the createLink answer"))?
            .to_string();
        Ok(Published {
            page: web.clone(),
            direct: web,
            path,
        })
    }

    /// Delete by app-folder path; missing files do not count as errors.
    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        let mut n = 0;
        for p in paths {
            let res = self
                .graph(|c, g| c.delete(format!("{g}/me/drive/special/approot:/{}", encode_path(p))))
                .await?;
            match res.status().as_u16() {
                200 | 204 => n += 1,
                404 => {}
                s => bail!("OneDrive delete {p}: HTTP {s}"),
            }
        }
        Ok(n)
    }
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(crate::util::encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::DeviceProvider;
    use axum::body::Bytes;
    use axum::extract::Path as AxPath;
    use axum::http::HeaderMap;
    use axum::routing::{get, post, put};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A Graph that checks the bearer token, hands out an upload URL on
    /// itself, counts the chunk bytes and creates a link.
    async fn fake_graph() -> (String, tokio::task::JoinHandle<()>) {
        let received = Arc::new(AtomicU64::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let up = base.clone();
        let auth_ok = |h: &HeaderMap| {
            h.get("authorization").map(|v| v.to_str().unwrap_or("")) == Some("Bearer AT2")
        };
        let app = Router::new()
            .route(
                "/me/drive",
                get(move |h: HeaderMap| async move {
                    assert!(auth_ok(&h));
                    Json(serde_json::json!({ "quota": { "used": 1024, "total": 5368709120u64 } }))
                }),
            )
            .route(
                "/me/drive/special/approot:/{*rest}",
                post(move |AxPath(rest): AxPath<String>, h: HeaderMap| {
                    let up = up.clone();
                    async move {
                        assert!(auth_ok(&h));
                        assert!(rest.ends_with(":/createUploadSession"), "{rest}");
                        assert!(rest.starts_with("2026-09/clip"), "{rest}");
                        Json(serde_json::json!({ "uploadUrl": format!("{up}/upload/session1") }))
                    }
                })
                .delete(|AxPath(rest): AxPath<String>| async move {
                    if rest.contains("missing") {
                        axum::http::StatusCode::NOT_FOUND
                    } else {
                        axum::http::StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/upload/session1",
                put(move |h: HeaderMap, body: Bytes| {
                    let received = received.clone();
                    async move {
                        let range = h.get("content-range").unwrap().to_str().unwrap().to_string();
                        let total: u64 = range.rsplit('/').next().unwrap().parse().unwrap();
                        let got = received.fetch_add(body.len() as u64, Ordering::Relaxed) + body.len() as u64;
                        if got >= total {
                            (
                                axum::http::StatusCode::CREATED,
                                Json(serde_json::json!({ "id": "ITEM1", "webUrl": "https://1drv.example/item" })),
                            )
                        } else {
                            (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({ "nextExpectedRanges": [] })))
                        }
                    }
                }),
            )
            .route(
                "/me/drive/items/{id}/createLink",
                post(|AxPath(id): AxPath<String>, Json(body): Json<Value>| async move {
                    assert_eq!(id, "ITEM1");
                    assert_eq!(body["scope"], "anonymous");
                    Json(serde_json::json!({ "link": { "webUrl": "https://1drv.example/s/abc" } }))
                }),
            );
        // chunks are up to 10 MiB, well past axum's 2 MiB default
        let app = app.layer(axum::extract::DefaultBodyLimit::disable());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (base, task)
    }

    #[tokio::test]
    async fn publish_uploads_in_chunks_and_links_anonymously() {
        let (login, _l) = crate::oauth::tests::fake_login().await;
        let (graph, _g) = fake_graph().await;
        let provider = DeviceProvider {
            credential: "replaycut/test-onedrive",
            ..crate::oauth::tests::test_provider(&login)
        };
        let tokens = Arc::new(TokenSource::new(provider, "Tester".into(), "RT1".into()));
        let od = OneDrive {
            graph,
            tokens,
            client: reqwest::Client::new(),
        };
        let dir = std::env::temp_dir().join(format!("rc-onedrive-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip 1.mp4");
        std::fs::write(&file, vec![7u8; 3 * 1024 * 1024]).unwrap();
        let p = od.publish(&file, "2026-09").await.unwrap();
        assert_eq!(p.page, "https://1drv.example/s/abc");
        assert_eq!(p.direct, p.page);
        assert_eq!(p.path, "2026-09/clip 1.mp4");
        assert_eq!(
            od.delete(&["2026-09/clip 1.mp4".into(), "2026-09/missing.mp4".into()])
                .await
                .unwrap(),
            1
        );
        let acc = od.account().await.unwrap();
        assert_eq!(acc.name, "Tester");
        assert_eq!(acc.total, Some(5368709120));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod fake_servers {
    /// Not a test: serves the fake Microsoft (login on 8482, Graph on 8483)
    /// for trying the OneDrive card against a dev instance started with
    /// `REPLAYCUT_ONEDRIVE_CLIENT_ID=test-client REPLAYCUT_MS_LOGIN_BASE=http://127.0.0.1:8482
    /// REPLAYCUT_GRAPH_BASE=http://127.0.0.1:8483`. Run with
    /// `cargo test -p replaycut serve_fake_microsoft -- --ignored --nocapture`, stop with Ctrl+C.
    #[tokio::test]
    #[ignore]
    async fn serve_fake_microsoft() {
        use axum::body::Bytes;
        use axum::extract::Path as AxPath;
        use axum::http::HeaderMap;
        use axum::routing::{get, post, put};
        use axum::{Form, Json, Router};
        use std::collections::HashMap;
        let login = Router::new()
            .route(
                "/devicecode",
                post(|| async {
                    Json(serde_json::json!({
                        "device_code": "DEV123", "user_code": "ABCD-EFGH",
                        "verification_uri": "https://example.com/devicelogin",
                        "interval": 2, "expires_in": 600
                    }))
                }),
            )
            .route(
                "/token",
                post(|Form(f): Form<HashMap<String, String>>| async move {
                    let _ = f;
                    Json(serde_json::json!({
                        "access_token": "AT-FAKE", "refresh_token": "RT-FAKE", "expires_in": 3600
                    }))
                }),
            );
        let graph = Router::new()
            .route("/me", get(|| async { Json(serde_json::json!({ "displayName": "Fake Tester" })) }))
            .route(
                "/me/drive",
                get(|| async { Json(serde_json::json!({ "quota": { "used": 1073741824u64, "total": 5368709120u64 } })) }),
            )
            .route(
                "/me/drive/special/approot:/{*rest}",
                post(|AxPath(_rest): AxPath<String>| async {
                    Json(serde_json::json!({ "uploadUrl": "http://127.0.0.1:8483/upload/s" }))
                })
                .delete(|| async { axum::http::StatusCode::NO_CONTENT }),
            )
            .route(
                "/upload/s",
                put(|h: HeaderMap, body: Bytes| async move {
                    let range = h.get("content-range").and_then(|v| v.to_str().ok()).unwrap_or("bytes 0-0/1").to_string();
                    let total: u64 = range.rsplit('/').next().and_then(|t| t.parse().ok()).unwrap_or(0);
                    let end: u64 = range.trim_start_matches("bytes ").split('/').next().and_then(|r| r.split('-').nth(1)).and_then(|e| e.parse().ok()).unwrap_or(0);
                    let _ = body;
                    if end + 1 >= total {
                        (axum::http::StatusCode::CREATED, Json(serde_json::json!({ "id": "ITEM1", "webUrl": "https://1drv.example/item" })))
                    } else {
                        (axum::http::StatusCode::ACCEPTED, Json(serde_json::json!({ "nextExpectedRanges": [] })))
                    }
                }),
            )
            .route(
                "/me/drive/items/{id}/createLink",
                post(|| async { Json(serde_json::json!({ "link": { "webUrl": "https://1drv.example/s/fake-link" } })) }),
            );
        let graph = graph.layer(axum::extract::DefaultBodyLimit::disable());
        let l1 = tokio::net::TcpListener::bind("127.0.0.1:8482")
            .await
            .unwrap();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:8483")
            .await
            .unwrap();
        println!("fake Microsoft: login http://127.0.0.1:8482, graph http://127.0.0.1:8483");
        tokio::join!(async { axum::serve(l1, login).await.unwrap() }, async {
            axum::serve(l2, graph).await.unwrap()
        });
    }
}
