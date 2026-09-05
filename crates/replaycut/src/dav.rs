//! Generic WebDAV storage (since 2.5): MKCOL and PUT with basic auth into
//! `<folder>/<month>/`, the link is `<publicBase>/<month>/<file>` - a
//! plain DAV server has no public links of its own, so the same folder must
//! be served publicly somewhere (nginx, a Storage Box, rclone serve http).

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;

use crate::integrations::Published;
use crate::util::encode_path_segment;

pub struct WebDav {
    url: String,
    folder: String,
    public_base: String,
    client: reqwest::Client,
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl WebDav {
    pub fn new(
        url: &str,
        folder: &str,
        public_base: &str,
        user: &str,
        password: &str,
    ) -> Result<Self> {
        anyhow::ensure!(
            url.starts_with("http://") || url.starts_with("https://"),
            "the WebDAV URL must start with http:// or https://"
        );
        anyhow::ensure!(
            public_base.starts_with("http://") || public_base.starts_with("https://"),
            "the public base URL must start with http:// or https:// (where the uploaded files can be opened)"
        );
        let mut headers = HeaderMap::new();
        let mut auth = HeaderValue::from_str(&format!(
            "Basic {}",
            base64(format!("{user}:{password}").as_bytes())
        ))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            folder: folder.trim_matches('/').to_string(),
            public_base: public_base.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(600))
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build()?,
        })
    }

    /// Remote path below the DAV root: `/<folder>/<month>/<file>`.
    pub fn remote_path(&self, month: &str, file_name: &str) -> String {
        if self.folder.is_empty() {
            format!("/{month}/{file_name}")
        } else {
            format!("/{}/{month}/{file_name}", self.folder)
        }
    }

    fn encoded(path: &str) -> String {
        path.split('/')
            .filter(|s| !s.is_empty())
            .map(encode_path_segment)
            .collect::<Vec<_>>()
            .join("/")
    }

    fn dav_url(&self, path: &str) -> String {
        format!("{}/{}", self.url, Self::encoded(path))
    }

    /// The public link: the folder part is dropped, the base serves it.
    pub fn link(&self, month: &str, file_name: &str) -> String {
        format!(
            "{}/{}",
            self.public_base,
            Self::encoded(&format!("{month}/{file_name}"))
        )
    }

    async fn mkcol(&self, dir: &str) -> Result<()> {
        let res = self
            .client
            .request(reqwest::Method::from_bytes(b"MKCOL")?, self.dav_url(dir))
            .send()
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::METHOD_NOT_ALLOWED {
            bail!("MKCOL {dir}: HTTP {}", res.status());
        }
        Ok(())
    }

    pub async fn publish(&self, file: &Path, month: &str) -> Result<Published> {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if !self.folder.is_empty() {
            self.mkcol(&format!("/{}", self.folder)).await?;
        }
        let dir = if self.folder.is_empty() {
            format!("/{month}")
        } else {
            format!("/{}/{month}", self.folder)
        };
        self.mkcol(&dir).await?;
        let path = self.remote_path(month, &name);
        let f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let res = self
            .client
            .put(self.dav_url(&path))
            .header(reqwest::header::CONTENT_TYPE, "video/mp4")
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await
            .context("WebDAV upload")?;
        if !res.status().is_success() {
            bail!("WebDAV upload: HTTP {}", res.status());
        }
        let link = self.link(month, &name);
        Ok(Published {
            page: link.clone(),
            direct: link,
            path,
        })
    }

    /// Delete by remote path; missing files do not count as errors.
    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        let mut n = 0;
        for p in paths {
            let res = self.client.delete(self.dav_url(p)).send().await?;
            match res.status().as_u16() {
                200 | 204 => n += 1,
                404 => {}
                s => bail!("WebDAV delete {p}: HTTP {s}"),
            }
        }
        Ok(n)
    }

    /// PROPFIND the root: server reachable and the login accepted (the diagnostics row).
    pub async fn check(&self) -> Result<()> {
        let res = self
            .client
            .request(reqwest::Method::from_bytes(b"PROPFIND")?, self.dav_url("/"))
            .header("Depth", "0")
            .send()
            .await
            .context("WebDAV server")?;
        if !res.status().is_success() {
            bail!(
                "PROPFIND: HTTP {} - check URL, user and password",
                res.status()
            );
        }
        Ok(())
    }

    /// The connection test: PROPFIND the root, then PUT and DELETE a probe.
    pub async fn probe(&self) -> Result<()> {
        self.check().await?;
        if !self.folder.is_empty() {
            self.mkcol(&format!("/{}", self.folder)).await?;
        }
        let probe = self.remote_path("probe", ".replaycut-probe");
        self.mkcol(&format!("/{}/probe", self.folder).replace("//", "/"))
            .await?;
        let res = self
            .client
            .put(self.dav_url(&probe))
            .body("replaycut")
            .send()
            .await?;
        if !res.status().is_success() {
            bail!("probe upload: HTTP {}", res.status());
        }
        self.delete(std::slice::from_ref(&probe)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_links_are_encoded() {
        let d = WebDav::new(
            "https://dav.example.com/remote.php/dav/files/me/",
            "/Clips/",
            "https://files.example.com/clips/",
            "me",
            "pw",
        )
        .unwrap();
        assert_eq!(
            d.remote_path("2026-09", "a b.mp4"),
            "/Clips/2026-09/a b.mp4"
        );
        assert_eq!(
            d.dav_url("/Clips/2026-09/a b.mp4"),
            "https://dav.example.com/remote.php/dav/files/me/Clips/2026-09/a%20b.mp4"
        );
        assert_eq!(
            d.link("2026-09", "a b.mp4"),
            "https://files.example.com/clips/2026-09/a%20b.mp4"
        );
        assert!(WebDav::new("dav://x", "", "https://x", "u", "p").is_err());
        assert!(WebDav::new("https://x", "", "", "u", "p").is_err());
    }
}
