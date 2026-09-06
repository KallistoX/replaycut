//! `settings.json`: everything that is not a secret. Secrets live in the
//! Windows Credential Manager (see `credentials`). Command-line flags override
//! individual fields for development and testing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Folder OBS writes replays to. Scanned non-recursively for `*.mkv`.
    pub clip_dir: PathBuf,
    pub port: u16,
    pub bind: String,
    /// The UI file. A relative path is resolved against the executable's
    /// directory first, then the working directory.
    pub ui_file: PathBuf,
    /// Prefix of the Discord post and webhook user name.
    pub display_name: String,
    /// `auto` or an ffmpeg encoder name such as `h264_nvenc` or `libx264`.
    pub encoder: String,
    /// ffmpeg `-hwaccel` value for decoding, empty for software decoding.
    pub hwaccel: String,
    pub ffmpeg_priority: FfmpegPriority,
    /// `-threads` for ffmpeg. 0 = half of the logical cores, at least 2.
    pub ffmpeg_threads: u32,
    pub log_level: String,
    /// Ask GitHub once a day whether a newer release exists (hint only).
    pub check_updates: bool,
    /// False until the browser setup finished (or `replaycut setup` ran).
    /// Missing in a file from 2.0 means true: that installation was set up.
    pub setup_done: bool,
    /// Theme name: `wardogs` is built in, anything else is
    /// `<data-dir>/themes/<name>.css`.
    pub theme: String,
    /// When the playable H.264 copy of a clip is made (since 2.6):
    /// `onDemand` (the button in the player) or `always` (right after the
    /// scan, with idle priority).
    pub preview_h264: String,
    /// argon2id PHC string of the optional password; never sent to the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password_hash: Option<String>,
    pub integrations: Integrations,
    /// obs-websocket on this PC (the password lives in the Credential Manager).
    pub obs: Obs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Obs {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
}

impl Default for Obs {
    fn default() -> Self {
        Self {
            enabled: true,
            host: "localhost".into(),
            port: 4455,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FfmpegPriority {
    Normal,
    BelowNormal,
    Idle,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Integrations {
    pub nextcloud: Nextcloud,
    pub discord: Discord,
    /// since 2.5
    pub onedrive: OneDrive,
    /// since 2.5
    pub s3: S3,
    /// since 2.5
    pub webdav: WebDav,
    /// since 2.6
    pub youtube: YouTube,
    /// since 2.6
    pub x: X,
    /// since 2.6
    pub telegram: Telegram,
    /// since 2.6
    pub webhook: Webhook,
}

/// Telegram bot (since 2.6): posts the link into a chat or channel. The bot
/// token is the credential `replaycut/telegram`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Telegram {
    pub enabled: bool,
    /// Posts every share without being asked.
    #[serde(default = "default_true")]
    pub auto_post: bool,
    /// A chat or channel id (`-1001234567890`) or `@channelname`.
    pub chat_id: String,
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_post: true,
            chat_id: String::new(),
        }
    }
}

/// Generic webhook (since 2.6): a JSON POST per share, signed with the
/// optional secret (credential `replaycut/webhook-secret`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Webhook {
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub auto_post: bool,
    pub url: String,
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_post: true,
            url: String::new(),
        }
    }
}

/// X (since 2.6): every share is a post with the video attached. The
/// account is the credential `replaycut/x` (refresh token), connected in
/// the browser on this PC.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct X {
    pub enabled: bool,
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
    /// Text template of the post; `{title}`, `{clip}` and `{date}` are replaced.
    pub text: String,
}

impl Default for X {
    fn default() -> Self {
        Self {
            enabled: false,
            quick_share: false,
            max_height: 0,
            max_kbps: 0,
            text: "{title}".into(),
        }
    }
}

/// YouTube (since 2.6): every share is its own video. The user's own Google
/// OAuth client is the credential `replaycut/youtube-client`, the connected
/// channel the credential `replaycut/youtube` (refresh token).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct YouTube {
    pub enabled: bool,
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
    /// `tv` (a "TVs and Limited Input devices" client, connected with a
    /// code from any device) or `desktop` (a "Desktop app" client, connected
    /// in the browser on this PC through the loopback redirect).
    pub client_type: String,
    /// `unlisted` (default), `private` or `public`.
    pub privacy: String,
    /// Description template; `{title}`, `{clip}` and `{date}` are replaced.
    pub description: String,
}

pub const YOUTUBE_PRIVACY: [&str; 3] = ["unlisted", "private", "public"];
pub const YOUTUBE_CLIENT_TYPES: [&str; 2] = ["tv", "desktop"];

impl Default for YouTube {
    fn default() -> Self {
        Self {
            enabled: false,
            quick_share: false,
            max_height: 0,
            max_kbps: 0,
            client_type: "tv".into(),
            privacy: "unlisted".into(),
            description: "{title}\n\nClip from {date}, shared with replaycut.".into(),
        }
    }
}

/// S3-compatible object storage (since 2.5): AWS, R2, B2, MinIO, Wasabi. Keys
/// are the credential `replaycut/s3`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct S3 {
    pub enabled: bool,
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
    /// `https://<account>.r2.cloudflarestorage.com`, `https://s3.eu-central-1.amazonaws.com`, `http://minio:9000`.
    pub endpoint: String,
    /// `auto` for R2, the AWS region otherwise.
    pub region: String,
    pub bucket: String,
    /// Key prefix inside the bucket, may be empty.
    pub prefix: String,
    /// Public URL under which the keys are served (custom domain, public bucket); empty = presigned links.
    pub public_base: String,
    /// Lifetime of presigned links in days (1-7) when `publicBase` is empty.
    pub presign_days: u32,
}

impl Default for S3 {
    fn default() -> Self {
        Self {
            enabled: false,
            quick_share: false,
            max_height: 0,
            max_kbps: 0,
            endpoint: String::new(),
            region: "auto".into(),
            bucket: String::new(),
            prefix: "replaycut".into(),
            public_base: String::new(),
            presign_days: 7,
        }
    }
}

/// Generic WebDAV (since 2.5): any DAV server plus a public URL that serves
/// the same folder. Login is the credential `replaycut/webdav`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WebDav {
    pub enabled: bool,
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
    /// The DAV root, for example `https://u123.your-storagebox.de` or `https://dav.example.com/remote.php/dav/files/me`.
    pub url: String,
    /// Folder below the root, may be empty.
    pub folder: String,
    /// Public URL that serves `<folder>`; the link is `<publicBase>/<month>/<file>`.
    pub public_base: String,
}

impl Default for WebDav {
    fn default() -> Self {
        Self {
            enabled: false,
            quick_share: false,
            max_height: 0,
            max_kbps: 0,
            url: String::new(),
            folder: "replaycut".into(),
            public_base: String::new(),
        }
    }
}

/// OneDrive through Microsoft Graph (since 2.5). The account itself is a
/// refresh token in the Credential Manager, connected through the device flow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OneDrive {
    pub enabled: bool,
    /// The target of the plain "Share" button (one storage at most).
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Nextcloud {
    pub enabled: bool,
    pub url: String,
    pub folder: String,
    /// Public links expire after this many days; 0 = never.
    pub expire_days: u32,
    /// The target of the plain "Share" button (since 2.5; one storage at most).
    #[serde(default = "default_true")]
    pub quick_share: bool,
    /// Limits of this target (since 2.7): 0 = none, else the share is
    /// scaled to at most this height and capped at this bitrate.
    pub max_height: u32,
    pub max_kbps: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Discord {
    pub enabled: bool,
    /// Posts every share without being asked (since 2.5).
    #[serde(default = "default_true")]
    pub auto_post: bool,
}

impl Default for Discord {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_post: true,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for Nextcloud {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "https://cloud.example.com".into(),
            folder: "Clips".into(),
            expire_days: 0,
            quick_share: true,
            max_height: 0,
            max_kbps: 0,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            clip_dir: default_clip_dir(),
            port: 8420,
            bind: "0.0.0.0".into(),
            ui_file: PathBuf::from("ui/index.html"),
            display_name: "replaycut".into(),
            encoder: "auto".into(),
            hwaccel: String::new(),
            ffmpeg_priority: FfmpegPriority::BelowNormal,
            ffmpeg_threads: 0,
            log_level: "info".into(),
            check_updates: true,
            setup_done: true,
            theme: "wardogs".into(),
            preview_h264: "onDemand".into(),
            password_hash: None,
            integrations: Integrations::default(),
            obs: Obs::default(),
        }
    }
}

fn default_clip_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join("Videos")
}

/// Default data directory: `%LOCALAPPDATA%\replaycut` on Windows,
/// `$XDG_DATA_HOME/replaycut` or `~/.local/share/replaycut` elsewhere.
pub fn default_data_dir() -> PathBuf {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("replaycut");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(xdg).join("replaycut");
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local").join("share").join("replaycut")
}

pub const LOG_LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
pub const PREVIEW_H264_MODES: [&str; 2] = ["onDemand", "always"];

/// Theme names are file names: lower-case letters, digits and dashes only.
pub fn is_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Top-level keys `PUT /api/settings` accepts, and the nested ones below
/// `integrations`. Anything else is a 400 with the offending name.
const PATCH_KEYS: [&str; 16] = [
    "obs",
    "previewH264",
    "clipDir",
    "port",
    "bind",
    "uiFile",
    "displayName",
    "encoder",
    "hwaccel",
    "ffmpegPriority",
    "ffmpegThreads",
    "logLevel",
    "checkUpdates",
    "setupDone",
    "theme",
    "integrations",
];
const NEXTCLOUD_KEYS: [&str; 5] = ["enabled", "url", "folder", "expireDays", "quickShare"];
const DISCORD_KEYS: [&str; 2] = ["enabled", "autoPost"];
const TELEGRAM_KEYS: [&str; 3] = ["enabled", "autoPost", "chatId"];
const WEBHOOK_KEYS: [&str; 3] = ["enabled", "autoPost", "url"];
const ONEDRIVE_KEYS: [&str; 2] = ["enabled", "quickShare"];
const S3_KEYS: [&str; 8] = [
    "enabled",
    "quickShare",
    "endpoint",
    "region",
    "bucket",
    "prefix",
    "publicBase",
    "presignDays",
];
const WEBDAV_KEYS: [&str; 5] = ["enabled", "quickShare", "url", "folder", "publicBase"];
const YOUTUBE_KEYS: [&str; 5] = [
    "enabled",
    "quickShare",
    "clientType",
    "privacy",
    "description",
];
/// Storage integrations, for the "one quick-share target" rule.
const X_KEYS: [&str; 3] = ["enabled", "quickShare", "text"];
const STORAGE_GROUPS: [&str; 6] = ["nextcloud", "onedrive", "s3", "webdav", "youtube", "x"];
/// Every storage takes these (since 2.7).
const LIMIT_KEYS: [&str; 2] = ["maxHeight", "maxKbps"];

/// The limits of a storage target (since 2.7): 0 means none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    pub max_height: u32,
    pub max_kbps: u32,
}

impl Settings {
    /// The limits of a storage target by id; unknown ids have none.
    pub fn limits(&self, target: &str) -> Limits {
        let i = &self.integrations;
        let (h, k) = match target {
            "nextcloud" => (i.nextcloud.max_height, i.nextcloud.max_kbps),
            "onedrive" => (i.onedrive.max_height, i.onedrive.max_kbps),
            "s3" => (i.s3.max_height, i.s3.max_kbps),
            "webdav" => (i.webdav.max_height, i.webdav.max_kbps),
            "youtube" => (i.youtube.max_height, i.youtube.max_kbps),
            "x" => (i.x.max_height, i.x.max_kbps),
            _ => (0, 0),
        };
        Limits {
            max_height: h,
            max_kbps: k,
        }
    }

    fn validate_limits(&self) -> Result<()> {
        for id in STORAGE_GROUPS {
            let l = self.limits(id);
            anyhow::ensure!(
                l.max_height == 0 || (240..=4320).contains(&l.max_height),
                "integrations.{id}.maxHeight must be 0 (none) or between 240 and 4320"
            );
            anyhow::ensure!(
                l.max_kbps == 0 || (500..=200_000).contains(&l.max_kbps),
                "integrations.{id}.maxKbps must be 0 (none) or between 500 and 200000"
            );
        }
        Ok(())
    }
}
const OBS_KEYS: [&str; 3] = ["enabled", "host", "port"];

impl Settings {
    /// Apply a partial JSON object (the body of `PUT /api/settings`) and
    /// return the new settings, validated. Secrets are not settings and
    /// must be stripped by the caller first.
    pub fn with_patch(&self, patch: &serde_json::Value) -> std::result::Result<Self, String> {
        let Some(obj) = patch.as_object() else {
            return Err("body must be a JSON object".into());
        };
        let mut current = serde_json::to_value(self).map_err(|e| e.to_string())?;
        // a storage switched to quick share takes it from the others
        let mut quick: Option<String> = None;
        for (key, value) in obj {
            if !PATCH_KEYS.contains(&key.as_str()) {
                return Err(format!("unknown field: {key}"));
            }
            if key == "obs" {
                let Some(fields) = value.as_object() else {
                    return Err("obs must be an object".into());
                };
                for (field, v) in fields {
                    if !OBS_KEYS.contains(&field.as_str()) {
                        return Err(format!("unknown field: obs.{field}"));
                    }
                    current["obs"][field] = v.clone();
                }
            } else if key == "integrations" {
                let Some(groups) = value.as_object() else {
                    return Err("integrations must be an object".into());
                };
                for (group, fields) in groups {
                    let allowed: &[&str] = match group.as_str() {
                        "nextcloud" => &NEXTCLOUD_KEYS,
                        "discord" => &DISCORD_KEYS,
                        "onedrive" => &ONEDRIVE_KEYS,
                        "s3" => &S3_KEYS,
                        "webdav" => &WEBDAV_KEYS,
                        "youtube" => &YOUTUBE_KEYS,
                        "x" => &X_KEYS,
                        "telegram" => &TELEGRAM_KEYS,
                        "webhook" => &WEBHOOK_KEYS,
                        _ => return Err(format!("unknown integration: {group}")),
                    };
                    let Some(fields) = fields.as_object() else {
                        return Err(format!("integrations.{group} must be an object"));
                    };
                    let storage = STORAGE_GROUPS.contains(&group.as_str());
                    for (field, v) in fields {
                        if !allowed.contains(&field.as_str())
                            && !(storage && LIMIT_KEYS.contains(&field.as_str()))
                        {
                            return Err(format!("unknown field: integrations.{group}.{field}"));
                        }
                        if field == "quickShare" && v == &serde_json::Value::Bool(true) {
                            quick = Some(group.clone());
                        }
                        current["integrations"][group][field] = v.clone();
                    }
                }
            } else {
                current[key] = value.clone();
            }
        }
        if let Some(winner) = &quick {
            for g in STORAGE_GROUPS {
                if g != winner {
                    current["integrations"][g]["quickShare"] = serde_json::Value::Bool(false);
                }
            }
        }
        let next: Settings = serde_json::from_value(current).map_err(|e| {
            // serde's message names the field: "invalid type: string, expected u16" etc.
            format!("invalid value: {e}")
        })?;
        next.validate().map_err(|e| e.to_string())?;
        Ok(next)
    }

    /// Which restart-only fields differ between two settings.
    pub fn restart_needed(&self, other: &Settings) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.port != other.port {
            out.push("port");
        }
        if self.bind != other.bind {
            out.push("bind");
        }
        if self.ui_file != other.ui_file {
            out.push("uiFile");
        }
        out
    }

    /// The settings as the UI may see them: everything but the password hash.
    pub fn public_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(map) = v.as_object_mut() {
            map.remove("passwordHash");
        }
        v
    }

    /// Load the file, or write the defaults when it does not exist yet.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let settings: Settings = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .with_context(|| format!("{} is not valid settings JSON", path.display()))?;
            return Ok(settings);
        }
        // A brand-new file: nothing is set up yet.
        let settings = Settings {
            setup_done: false,
            ..Settings::default()
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&settings)? + "\n")
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(settings)
    }

    /// Write the file (pretty JSON, trailing newline).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self)?
                + "
",
        )
        .with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.port != 0, "port must not be 0");
        anyhow::ensure!(
            self.bind.parse::<std::net::IpAddr>().is_ok(),
            "bind must be an IP address such as 0.0.0.0 or 127.0.0.1"
        );
        anyhow::ensure!(
            LOG_LEVELS.contains(&self.log_level.as_str()),
            "logLevel must be one of error, warn, info, debug, trace"
        );
        anyhow::ensure!(
            is_theme_name(&self.theme),
            "theme must be a name of lower-case letters, digits and dashes"
        );
        anyhow::ensure!(self.obs.port != 0, "obs.port must not be 0");
        anyhow::ensure!(
            !self.obs.host.trim().is_empty(),
            "obs.host must not be empty"
        );
        anyhow::ensure!(
            crate::media::HWACCEL_VALUES.contains(&self.hwaccel.trim()),
            "hwaccel must be auto, none, cuda, d3d11va or qsv"
        );
        anyhow::ensure!(
            self.ffmpeg_threads <= 256,
            "ffmpegThreads must be 0 (auto) or at most 256"
        );
        anyhow::ensure!(
            !self.clip_dir.as_os_str().is_empty(),
            "clipDir must not be empty"
        );
        self.validate_limits()?;
        anyhow::ensure!(
            !self.encoder.trim().is_empty(),
            "encoder must be 'auto' or an encoder name"
        );
        anyhow::ensure!(
            YOUTUBE_PRIVACY.contains(&self.integrations.youtube.privacy.as_str()),
            "integrations.youtube.privacy must be unlisted, private or public"
        );
        anyhow::ensure!(
            PREVIEW_H264_MODES.contains(&self.preview_h264.as_str()),
            "previewH264 must be onDemand or always"
        );
        anyhow::ensure!(
            self.integrations.webhook.url.trim().is_empty()
                || crate::notify::is_http_url(self.integrations.webhook.url.trim()),
            "integrations.webhook.url must start with http:// or https://"
        );
        anyhow::ensure!(
            self.integrations.x.text.chars().count() <= 280,
            "integrations.x.text must be at most 280 characters"
        );
        anyhow::ensure!(
            YOUTUBE_CLIENT_TYPES.contains(&self.integrations.youtube.client_type.as_str()),
            "integrations.youtube.clientType must be tv or desktop"
        );
        anyhow::ensure!(
            self.integrations.youtube.description.chars().count() <= 4000,
            "integrations.youtube.description must be at most 4000 characters"
        );
        Ok(())
    }

    /// Effective `-threads` value for ffmpeg.
    pub fn ffmpeg_threads(&self) -> u32 {
        if self.ffmpeg_threads > 0 {
            return self.ffmpeg_threads;
        }
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4) as u32;
        (cores / 2).max(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        let text = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.port, 8420);
        assert_eq!(back.ffmpeg_priority, FfmpegPriority::BelowNormal);
        assert!(text.contains("\"ffmpegPriority\":\"belowNormal\""));
    }

    #[test]
    fn partial_file_uses_defaults() {
        let s: Settings = serde_json::from_str(r#"{"port": 9000}"#).unwrap();
        assert_eq!(s.port, 9000);
        assert_eq!(s.limits("nextcloud"), Limits::default());
        assert!(!s.integrations.nextcloud.enabled);
    }

    #[test]
    fn auto_threads_is_at_least_two() {
        let s = Settings::default();
        assert!(s.ffmpeg_threads() >= 2);
        let s = Settings {
            ffmpeg_threads: 3,
            ..Settings::default()
        };
        assert_eq!(s.ffmpeg_threads(), 3);
    }

    #[test]
    fn patch_merges_and_validates() {
        let s = Settings::default();
        let next = s
            .with_patch(&serde_json::json!({
                "previewH264": "always",
                "integrations": { "nextcloud": { "enabled": true, "url": "https://cloud.example.com" } }
            }))
            .unwrap();
        assert_eq!(next.preview_h264, "always");
        assert!(next.integrations.nextcloud.enabled);
        assert_eq!(next.integrations.nextcloud.folder, "Clips");
        assert_eq!(next.port, 8420);

        let err = s
            .with_patch(&serde_json::json!({ "passwordHash": "x" }))
            .unwrap_err();
        assert!(err.contains("unknown field: passwordHash"), "{err}");
        let err = s.with_patch(&serde_json::json!({ "port": 0 })).unwrap_err();
        assert!(err.contains("port"), "{err}");
        let err = s
            .with_patch(&serde_json::json!({ "port": "abc" }))
            .unwrap_err();
        assert!(err.contains("invalid value"), "{err}");
        let err = s
            .with_patch(&serde_json::json!({ "integrations": { "dropbox": {} } }))
            .unwrap_err();
        assert!(err.contains("unknown integration"), "{err}");
        assert!(s
            .with_patch(&serde_json::json!({ "theme": "Bad Name" }))
            .is_err());
    }

    #[test]
    fn youtube_block_validates_and_takes_quick_share() {
        let s = Settings::default();
        assert_eq!(s.integrations.youtube.privacy, "unlisted");
        let next = s
            .with_patch(&serde_json::json!({
                "integrations": { "youtube": { "enabled": true, "quickShare": true, "privacy": "public" } }
            }))
            .unwrap();
        assert!(next.integrations.youtube.quick_share);
        assert!(!next.integrations.nextcloud.quick_share);
        assert_eq!(next.integrations.youtube.privacy, "public");
        let err = s
            .with_patch(
                &serde_json::json!({ "integrations": { "youtube": { "privacy": "secret" } } }),
            )
            .unwrap_err();
        assert!(err.contains("privacy"), "{err}");
        let err = s
            .with_patch(&serde_json::json!({ "integrations": { "youtube": { "channel": "x" } } }))
            .unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        let err = s
            .with_patch(
                &serde_json::json!({ "integrations": { "youtube": { "clientType": "phone" } } }),
            )
            .unwrap_err();
        assert!(err.contains("clientType"), "{err}");
        assert_eq!(s.integrations.youtube.client_type, "tv");
    }

    #[test]
    fn restart_fields() {
        let s = Settings::default();
        let next = s
            .with_patch(&serde_json::json!({ "port": 9000, "displayName": "x" }))
            .unwrap();
        assert_eq!(s.restart_needed(&next), vec!["port"]);
        assert!(s.restart_needed(&s).is_empty());
    }

    #[test]
    fn public_json_hides_the_hash() {
        let s = Settings {
            password_hash: Some("$argon2id$...".into()),
            ..Settings::default()
        };
        let v = s.public_json();
        assert!(v.get("passwordHash").is_none());
        assert_eq!(v["theme"], "wardogs");
        assert_eq!(v["setupDone"], true);
    }

    #[test]
    fn fresh_file_is_not_set_up() {
        let dir = std::env::temp_dir().join(format!("rc-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("settings.json");
        let s = Settings::load_or_create(&path).unwrap();
        assert!(!s.setup_done);
        let again = Settings::load_or_create(&path).unwrap();
        assert!(!again.setup_done);
        // a file without the field (2.0) counts as set up
        std::fs::write(&path, r#"{"port": 8420}"#).unwrap();
        assert!(Settings::load_or_create(&path).unwrap().setup_done);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_rejects_zero_port() {
        let s = Settings {
            port: 0,
            ..Settings::default()
        };
        assert!(s.validate().is_err());
    }
}

#[cfg(test)]
mod limit_tests {
    use super::*;

    #[test]
    fn limits_are_per_storage_and_validated() {
        let s = Settings::default();
        let next = s
            .with_patch(&serde_json::json!({
                "integrations": { "youtube": { "maxHeight": 1080, "maxKbps": 12000 }, "s3": { "maxKbps": 4000 } }
            }))
            .unwrap();
        assert_eq!(
            next.limits("youtube"),
            Limits {
                max_height: 1080,
                max_kbps: 12000
            }
        );
        assert_eq!(next.limits("s3").max_kbps, 4000);
        assert_eq!(next.limits("nextcloud"), Limits::default());
        assert_eq!(next.limits("file"), Limits::default());
        let err = s
            .with_patch(
                &serde_json::json!({ "integrations": { "onedrive": { "maxHeight": 100 } } }),
            )
            .unwrap_err();
        assert!(err.contains("maxHeight"), "{err}");
        let err = s
            .with_patch(&serde_json::json!({ "integrations": { "discord": { "maxKbps": 4000 } } }))
            .unwrap_err();
        assert!(err.contains("unknown field"), "{err}");
        // the global bitrate of 2.6 is gone
        let err = s
            .with_patch(&serde_json::json!({ "shareKbps": 6000 }))
            .unwrap_err();
        assert!(err.contains("unknown field: shareKbps"), "{err}");
        // an old settings.json with the field still loads
        let old: Settings = serde_json::from_str(r#"{"shareKbps": 6000, "port": 8420}"#).unwrap();
        assert_eq!(old.port, 8420);
    }
}
