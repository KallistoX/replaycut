//! Optional integrations of the share pipeline. A **storage** turns the
//! encoded file into a link, a **notify** target posts that link. Each is
//! independently enabled in the settings; `--dry-run` replaces both with
//! simulations that never touch the network.

use std::hash::{BuildHasher, Hasher};
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde_json::Value;

use crate::credentials;
use crate::settings::Settings;
use crate::util::encode_path_segment;

#[derive(Debug, Clone)]
pub struct Published {
    /// Share page.
    pub page: String,
    /// Direct download URL (`<page>/download`).
    pub direct: String,
    /// Remote path, `/<folder>/<month>/<file>`.
    pub path: String,
}

/// What a storage may want to know about the share besides the file (since
/// 2.6: YouTube builds title and description from it).
#[derive(Debug, Clone, Default)]
pub struct PublishMeta {
    /// `YYYY-MM` of the clip, the remote folder.
    pub month: String,
    /// The clip's title, may be empty.
    pub title: String,
    /// The clip's base name.
    pub base: String,
    pub display_name: String,
    /// A 9:16 cut.
    pub vertical: bool,
    /// The job's timestamp.
    pub at: String,
}

pub enum Storage {
    DryRun { folder: String },
    Nextcloud(Nextcloud),
    OneDrive(crate::onedrive::OneDrive),
    S3(crate::s3::S3),
    WebDav(crate::dav::WebDav),
    YouTube(crate::youtube::YouTube),
}

pub enum Notify {
    DryRun,
    Discord(Discord),
}

/// A configured storage as a share target (since 2.5). `id` is what
/// `POST /api/share` takes as `target`.
pub struct StorageEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub storage: Storage,
    /// The target of the plain "Share" button.
    pub quick_share: bool,
}

/// A configured notify integration (since 2.5).
pub struct NotifyEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub notify: Notify,
    /// Posts every share without being asked.
    pub auto_post: bool,
}

pub const TARGET_FILE: &str = "file";

/// Every integration replaycut knows, whether configured or not: id, label,
/// kind. `config.targets` lists them with their state so the UI can offer
/// them; adding an integration means one line here plus a settings block,
/// a credential constant, a `Storage`/`Notify` variant, a card and a
/// diagnostics row.
pub const KNOWN_TARGETS: [(&str, &str, &str); 6] = [
    ("nextcloud", "Nextcloud", "storage"),
    ("onedrive", "OneDrive", "storage"),
    ("s3", "S3", "storage"),
    ("webdav", "WebDAV", "storage"),
    ("youtube", "YouTube", "storage"),
    ("discord", "Discord", "notify"),
];

pub struct Integrations {
    pub storages: Vec<StorageEntry>,
    pub notifies: Vec<NotifyEntry>,
}

impl Integrations {
    /// Build from settings and the Credential Manager. An enabled integration
    /// without credentials is disabled with a warning, not an error.
    pub fn build(settings: &Settings, dry_run: bool) -> Result<Self> {
        let nc = &settings.integrations.nextcloud;
        let dc = &settings.integrations.discord;
        let od = &settings.integrations.onedrive;
        if dry_run {
            let mut storages = vec![StorageEntry {
                id: "nextcloud",
                label: "Nextcloud",
                storage: Storage::DryRun {
                    folder: nc.folder.clone(),
                },
                quick_share: nc.quick_share,
            }];
            if od.enabled {
                storages.push(StorageEntry {
                    id: "onedrive",
                    label: "OneDrive",
                    storage: Storage::DryRun {
                        folder: "Apps/replaycut".into(),
                    },
                    quick_share: od.quick_share,
                });
            }
            if settings.integrations.s3.enabled {
                storages.push(StorageEntry {
                    id: "s3",
                    label: "S3",
                    storage: Storage::DryRun {
                        folder: settings.integrations.s3.bucket.clone(),
                    },
                    quick_share: settings.integrations.s3.quick_share,
                });
            }
            if settings.integrations.webdav.enabled {
                storages.push(StorageEntry {
                    id: "webdav",
                    label: "WebDAV",
                    storage: Storage::DryRun {
                        folder: settings.integrations.webdav.folder.clone(),
                    },
                    quick_share: settings.integrations.webdav.quick_share,
                });
            }
            if settings.integrations.youtube.enabled {
                storages.push(StorageEntry {
                    id: "youtube",
                    label: "YouTube",
                    storage: Storage::DryRun {
                        folder: "youtube".into(),
                    },
                    quick_share: settings.integrations.youtube.quick_share,
                });
            }
            return Ok(Self {
                storages,
                notifies: vec![NotifyEntry {
                    id: "discord",
                    label: "Discord",
                    notify: Notify::DryRun,
                    auto_post: dc.auto_post,
                }],
            });
        }
        let mut storages = Vec::new();
        if nc.enabled {
            match credentials::read(credentials::NEXTCLOUD)? {
                Some(cred) => storages.push(StorageEntry {
                    id: "nextcloud",
                    label: "Nextcloud",
                    storage: Storage::Nextcloud(Nextcloud::new(settings, cred.user, cred.secret)?),
                    quick_share: nc.quick_share,
                }),
                None => tracing::warn!(
                    "Nextcloud is enabled but has no credentials - run `replaycut setup`"
                ),
            }
        }
        if od.enabled {
            match credentials::read(credentials::ONEDRIVE)? {
                Some(cred) => {
                    let provider = crate::oauth::provider("onedrive")
                        .ok_or_else(|| anyhow!("no OneDrive provider"))?;
                    let tokens = std::sync::Arc::new(crate::oauth::TokenSource::new(
                        provider, cred.user, cred.secret,
                    ));
                    storages.push(StorageEntry {
                        id: "onedrive",
                        label: "OneDrive",
                        storage: Storage::OneDrive(crate::onedrive::OneDrive::new(tokens)?),
                        quick_share: od.quick_share,
                    });
                }
                None => tracing::warn!(
                    "OneDrive is enabled but not connected - connect it under Settings > Integrations"
                ),
            }
        }
        let s3 = &settings.integrations.s3;
        if s3.enabled {
            match credentials::read(credentials::S3)? {
                Some(cred) => storages.push(StorageEntry {
                    id: "s3",
                    label: "S3",
                    storage: Storage::S3(crate::s3::S3::new(
                        &s3.endpoint,
                        &s3.region,
                        &s3.bucket,
                        &s3.prefix,
                        &s3.public_base,
                        s3.presign_days,
                        cred.user,
                        cred.secret,
                    )?),
                    quick_share: s3.quick_share,
                }),
                None => tracing::warn!("S3 is enabled but has no access keys - enter them under Settings > Integrations"),
            }
        }
        let dav = &settings.integrations.webdav;
        if dav.enabled {
            match credentials::read(credentials::WEBDAV)? {
                Some(cred) => storages.push(StorageEntry {
                    id: "webdav",
                    label: "WebDAV",
                    storage: Storage::WebDav(crate::dav::WebDav::new(
                        &dav.url,
                        &dav.folder,
                        &dav.public_base,
                        &cred.user,
                        &cred.secret,
                    )?),
                    quick_share: dav.quick_share,
                }),
                None => tracing::warn!(
                    "WebDAV is enabled but has no login - enter it under Settings > Integrations"
                ),
            }
        }
        let yt = &settings.integrations.youtube;
        if yt.enabled {
            let provider =
                crate::oauth::provider("youtube").ok_or_else(|| anyhow!("no YouTube provider"))?;
            match credentials::read(credentials::YOUTUBE)? {
                _ if provider.client_id.is_empty() => tracing::warn!(
                    "YouTube is enabled but has no Google client - enter client id and secret under Settings > Integrations"
                ),
                Some(cred) => {
                    let tokens = std::sync::Arc::new(crate::oauth::TokenSource::new(
                        provider, cred.user, cred.secret,
                    ));
                    storages.push(StorageEntry {
                        id: "youtube",
                        label: "YouTube",
                        storage: Storage::YouTube(crate::youtube::YouTube::new(
                            tokens,
                            &yt.privacy,
                            &yt.description,
                        )?),
                        quick_share: yt.quick_share,
                    });
                }
                None => tracing::warn!(
                    "YouTube is enabled but not connected - connect the channel under Settings > Integrations"
                ),
            }
        }
        let mut notifies = Vec::new();
        if dc.enabled {
            match credentials::read(credentials::DISCORD_WEBHOOK)? {
                Some(cred) => notifies.push(NotifyEntry {
                    id: "discord",
                    label: "Discord",
                    notify: Notify::Discord(Discord::new(
                        cred.secret,
                        settings.display_name.clone(),
                    )?),
                    auto_post: dc.auto_post,
                }),
                None => {
                    tracing::warn!("Discord is enabled but has no webhook - run `replaycut setup`")
                }
            }
        }
        Ok(Self { storages, notifies })
    }

    /// The storage behind the plain "Share" button, if one is configured.
    pub fn default_storage(&self) -> Option<&StorageEntry> {
        self.storages.iter().find(|s| s.quick_share)
    }

    pub fn storage(&self, id: &str) -> Option<&StorageEntry> {
        self.storages.iter().find(|s| s.id == id)
    }

    /// Resolve a `target` from the API: empty means the default (or `file`
    /// when none), `file` means no upload, anything else must be configured.
    pub fn resolve_target(&self, target: &str) -> Option<String> {
        if target.is_empty() {
            return Some(
                self.default_storage()
                    .map(|s| s.id.to_string())
                    .unwrap_or_else(|| TARGET_FILE.to_string()),
            );
        }
        if target == TARGET_FILE || self.storage(target).is_some() {
            return Some(target.to_string());
        }
        None
    }

    /// Notify integrations that post every share.
    pub fn auto_notifies(&self) -> impl Iterator<Item = &NotifyEntry> {
        self.notifies.iter().filter(|n| n.auto_post)
    }

    /// `config.targets`: every known integration with its state.
    pub fn targets(&self, settings: &Settings) -> Value {
        let enabled = |id: &str| match id {
            "nextcloud" => settings.integrations.nextcloud.enabled,
            "onedrive" => settings.integrations.onedrive.enabled,
            "s3" => settings.integrations.s3.enabled,
            "webdav" => settings.integrations.webdav.enabled,
            "youtube" => settings.integrations.youtube.enabled,
            "discord" => settings.integrations.discord.enabled,
            _ => false,
        };
        Value::Array(
            KNOWN_TARGETS
                .iter()
                .map(|(id, label, kind)| {
                    let mut v = serde_json::json!({
                        "id": id, "label": label, "kind": kind, "enabled": enabled(id),
                    });
                    if *kind == "storage" {
                        let s = self.storage(id);
                        v["connected"] = Value::Bool(s.is_some());
                        v["quickShare"] = Value::Bool(s.is_some_and(|s| s.quick_share));
                    } else {
                        let n = self.notifies.iter().find(|n| n.id == *id);
                        v["connected"] = Value::Bool(n.is_some());
                        v["autoPost"] = Value::Bool(n.is_some_and(|n| n.auto_post));
                    }
                    v
                })
                .collect(),
        )
    }

    pub fn describe(&self) -> String {
        let name = |id: &str, dry: bool| {
            if dry {
                format!("{id} (dry run)")
            } else {
                id.to_string()
            }
        };
        let s: Vec<String> = self
            .storages
            .iter()
            .map(|e| {
                let mut n = name(e.label, matches!(e.storage, Storage::DryRun { .. }));
                if e.quick_share {
                    n.push_str(" [quick share]");
                }
                n
            })
            .collect();
        let n: Vec<String> = self
            .notifies
            .iter()
            .map(|e| {
                let mut n = name(e.label, matches!(e.notify, Notify::DryRun));
                if e.auto_post {
                    n.push_str(" [auto]");
                }
                n
            })
            .collect();
        format!(
            "storage: {}, notify: {}",
            if s.is_empty() {
                "none".to_string()
            } else {
                s.join(", ")
            },
            if n.is_empty() {
                "none".to_string()
            } else {
                n.join(", ")
            }
        )
    }
}

impl Storage {
    /// Where the file will live, before the upload. Empty for YouTube: the
    /// video id only exists once the upload is through.
    pub fn remote_path(&self, month: &str, file_name: &str) -> String {
        match self {
            Storage::DryRun { folder } => format!("/{folder}/{month}/{file_name}"),
            Storage::Nextcloud(nc) => format!("/{}/{month}/{file_name}", nc.folder),
            Storage::OneDrive(_) => crate::onedrive::OneDrive::remote_path(month, file_name),
            Storage::S3(s3) => s3.key(month, file_name),
            Storage::WebDav(d) => d.remote_path(month, file_name),
            Storage::YouTube(_) => String::new(),
        }
    }

    pub async fn publish(&self, file: &Path, meta: &PublishMeta) -> Result<Published> {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let month = meta.month.as_str();
        let path = self.remote_path(month, &name);
        match self {
            Storage::DryRun { .. } => {
                tracing::info!("dry run: upload to {path} skipped");
                let page = format!("https://dry-run.invalid/s/{}", random_token(16));
                Ok(Published {
                    direct: format!("{page}/download"),
                    page,
                    path,
                })
            }
            Storage::Nextcloud(nc) => nc.publish(file, month, &path).await,
            Storage::OneDrive(od) => od.publish(file, month).await,
            Storage::S3(s3) => s3.publish(file, month).await,
            Storage::WebDav(d) => d.publish(file, month).await,
            Storage::YouTube(yt) => yt.publish(file, meta).await,
        }
    }

    /// Delete remote files; missing ones do not count as errors. Returns how many were deleted.
    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        match self {
            Storage::DryRun { .. } => {
                tracing::info!("dry run: remote delete of {} path(s) skipped", paths.len());
                Ok(paths.len())
            }
            Storage::Nextcloud(nc) => nc.delete(paths).await,
            Storage::OneDrive(od) => od.delete(paths).await,
            Storage::S3(s3) => s3.delete(paths).await,
            Storage::WebDav(d) => d.delete(paths).await,
            Storage::YouTube(yt) => yt.delete(paths).await,
        }
    }
}

impl Notify {
    /// Post a message; returns a human-readable status for the job's `discord` field.
    pub async fn post(&self, text: &str) -> Result<String> {
        match self {
            Notify::DryRun => {
                tracing::info!("dry run: post skipped: {text}");
                Ok("dry run: not posted".to_string())
            }
            Notify::Discord(d) => d.post(text).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Nextcloud: WebDAV upload plus an OCS public link

/// What the Nextcloud login check reports.
#[derive(Debug, Clone)]
pub struct UserInfo {
    pub display_name: String,
    pub free: Option<u64>,
    pub total: Option<u64>,
}

pub struct Nextcloud {
    pub url: String,
    pub folder: String,
    pub expire_days: u32,
    user: String,
    client: reqwest::Client,
}

impl Nextcloud {
    pub fn new(settings: &Settings, user: String, password: String) -> Result<Self> {
        let nc = &settings.integrations.nextcloud;
        let mut headers = HeaderMap::new();
        headers.insert("OCS-APIRequest", HeaderValue::from_static("true"));
        let token = base64(format!("{user}:{password}").as_bytes());
        let mut auth = HeaderValue::from_str(&format!("Basic {token}"))?;
        auth.set_sensitive(true);
        headers.insert(reqwest::header::AUTHORIZATION, auth);
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            url: nc.url.trim_end_matches('/').to_string(),
            folder: nc.folder.trim_matches('/').to_string(),
            expire_days: nc.expire_days,
            user,
            client,
        })
    }

    /// Build from explicit values (used by `setup` before anything is saved).
    pub fn with_values(
        url: &str,
        folder: &str,
        expire_days: u32,
        user: String,
        password: String,
    ) -> Result<Self> {
        let mut settings = Settings::default();
        settings.integrations.nextcloud.url = url.to_string();
        settings.integrations.nextcloud.folder = folder.to_string();
        settings.integrations.nextcloud.expire_days = expire_days;
        Self::new(&settings, user, password)
    }

    fn dav_url(&self, remote_path: &str) -> String {
        let segments: Vec<String> = remote_path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(encode_path_segment)
            .collect();
        format!(
            "{}/remote.php/dav/files/{}/{}",
            self.url,
            encode_path_segment(&self.user),
            segments.join("/")
        )
    }

    /// Login check; returns the display name reported by the server.
    pub async fn test(&self) -> Result<String> {
        Ok(self.user_info().await?.display_name)
    }

    /// Login check with the quota (`OCS cloud/user`).
    pub async fn user_info(&self) -> Result<UserInfo> {
        let res = self
            .client
            .get(format!("{}/ocs/v2.php/cloud/user?format=json", self.url))
            .send()
            .await?;
        let status = res.status();
        if !status.is_success() {
            bail!("login failed: HTTP {status} - check user name and app password");
        }
        let body: Value = res.json().await.unwrap_or(Value::Null);
        let data = &body["ocs"]["data"];
        let display_name = data["display-name"]
            .as_str()
            .unwrap_or(&self.user)
            .to_string();
        // quota.total/free are -3 for "unlimited"; only positive numbers count.
        let positive = |v: &Value| v.as_i64().filter(|n| *n >= 0).map(|n| n as u64);
        Ok(UserInfo {
            display_name,
            free: positive(&data["quota"]["free"]),
            total: positive(&data["quota"]["total"]),
        })
    }

    async fn mkcol(&self, remote_dir: &str) -> Result<()> {
        let res = self
            .client
            .request(
                reqwest::Method::from_bytes(b"MKCOL")?,
                self.dav_url(remote_dir),
            )
            .send()
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::METHOD_NOT_ALLOWED {
            bail!("MKCOL {remote_dir}: HTTP {}", res.status());
        }
        Ok(())
    }

    pub async fn publish(&self, file: &Path, month: &str, remote_path: &str) -> Result<Published> {
        self.mkcol(&format!("/{}", self.folder)).await?;
        self.mkcol(&format!("/{}/{month}", self.folder)).await?;
        let f = tokio::fs::File::open(file)
            .await
            .with_context(|| format!("open {}", file.display()))?;
        let len = f.metadata().await?.len();
        let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(f));
        let res = self
            .client
            .put(self.dav_url(remote_path))
            .header(reqwest::header::CONTENT_TYPE, "video/mp4")
            .header(reqwest::header::CONTENT_LENGTH, len)
            .body(body)
            .send()
            .await
            .context("upload")?;
        if !res.status().is_success() {
            bail!("upload: HTTP {}", res.status());
        }
        // Reuse an existing public link, otherwise every share would create another one.
        let shares_url = format!("{}/ocs/v2.php/apps/files_sharing/api/v1/shares", self.url);
        let res = self
            .client
            .get(&shares_url)
            .query(&[
                ("format", "json"),
                ("path", remote_path),
                ("reshares", "true"),
            ])
            .send()
            .await?;
        if res.status().is_success() {
            let body: Value = res.json().await.unwrap_or(Value::Null);
            let existing = body["ocs"]["data"]
                .as_array()
                .and_then(|a| a.iter().find(|s| s["share_type"] == 3))
                .and_then(|s| s["url"].as_str());
            if let Some(url) = existing {
                tracing::info!("Nextcloud: existing public link reused");
                return Ok(Published {
                    page: url.to_string(),
                    direct: format!("{url}/download"),
                    path: remote_path.into(),
                });
            }
        }
        let mut form: Vec<(&str, String)> = vec![
            ("path", remote_path.to_string()),
            ("shareType", "3".into()),
            ("permissions", "1".into()),
        ];
        if self.expire_days > 0 {
            let date = chrono::Local::now() + chrono::Duration::days(i64::from(self.expire_days));
            form.push(("expireDate", date.format("%Y-%m-%d").to_string()));
        }
        let res = self
            .client
            .post(&shares_url)
            .query(&[("format", "json")])
            .form(&form)
            .send()
            .await?;
        let status = res.status();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("create share: HTTP {status} {body}");
        }
        let url = body["ocs"]["data"]["url"]
            .as_str()
            .ok_or_else(|| anyhow!("share response without url: {body}"))?;
        Ok(Published {
            page: url.to_string(),
            direct: format!("{url}/download"),
            path: remote_path.into(),
        })
    }

    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        let mut n = 0;
        for p in paths {
            let res = self.client.delete(self.dav_url(p)).send().await?;
            if res.status().is_success() {
                n += 1;
            } else if res.status() != StatusCode::NOT_FOUND {
                bail!("DELETE {p}: HTTP {}", res.status());
            }
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// Discord webhook

pub struct Discord {
    webhook: String,
    username: String,
    client: reqwest::Client,
}

impl Discord {
    pub fn new(webhook: String, username: String) -> Result<Self> {
        anyhow::ensure!(
            is_webhook_url(&webhook),
            "this does not look like a Discord webhook URL"
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            webhook,
            username,
            client,
        })
    }

    /// Only the bare link is posted; Discord embeds the MP4 inline, so clip
    /// length and server upload limits do not matter.
    pub async fn post(&self, text: &str) -> Result<String> {
        let body = serde_json::json!({ "content": text, "username": self.username });
        let res = self
            .client
            .post(&self.webhook)
            .query(&[("wait", "true")])
            .json(&body)
            .send()
            .await?;
        let status = res.status();
        if status.is_success() {
            return Ok("Link posted".to_string());
        }
        let detail = res.text().await.unwrap_or_default();
        tracing::warn!("Discord webhook: HTTP {status} {detail}");
        Ok(format!("webhook HTTP {}", status.as_u16()))
    }
}

pub fn is_webhook_url(url: &str) -> bool {
    let rest = [
        "https://discord.com/",
        "https://discordapp.com/",
        "https://ptb.discord.com/",
        "https://canary.discord.com/",
    ]
    .iter()
    .find_map(|p| url.strip_prefix(p));
    match rest {
        Some(r) => {
            r.starts_with("api/webhooks/")
                && r.len() > "api/webhooks/".len() + 10
                && !r.contains(char::is_whitespace)
        }
        None => false,
    }
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Lowercase hex from the standard library's randomly seeded hasher; good
/// enough for ids and fake links, no crate needed.
pub fn random_token(len: usize) -> String {
    let mut out = String::new();
    while out.len() < len {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(out.len() as u64);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_hex_of_requested_length() {
        let t = random_token(8);
        assert_eq!(t.len(), 8);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_token(16), random_token(16));
    }

    #[test]
    fn base64_matches_rfc() {
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"a"), "YQ==");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn webhook_urls() {
        assert!(is_webhook_url(
            "https://discord.com/api/webhooks/123456789/abcDEF-ghi"
        ));
        assert!(is_webhook_url(
            "https://canary.discord.com/api/webhooks/123456789/abcDEF-ghi"
        ));
        assert!(!is_webhook_url("https://example.com/api/webhooks/1/x"));
        assert!(!is_webhook_url("https://discord.com/channels/1/2"));
    }

    #[test]
    fn dav_urls_are_escaped_per_segment() {
        let nc = Nextcloud::with_values(
            "https://cloud.example.com/",
            "My Clips",
            0,
            "me".into(),
            "pw".into(),
        )
        .unwrap();
        assert_eq!(
            nc.dav_url("/My Clips/2026-09/a b.mp4"),
            "https://cloud.example.com/remote.php/dav/files/me/My%20Clips/2026-09/a%20b.mp4"
        );
    }
}
