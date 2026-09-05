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
    /// Video bitrate of the shared H.264 file in kbit/s.
    pub share_kbps: u32,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Nextcloud {
    pub enabled: bool,
    pub url: String,
    pub folder: String,
    /// Public links expire after this many days; 0 = never.
    pub expire_days: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Discord {
    pub enabled: bool,
}

impl Default for Nextcloud {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "https://cloud.example.com".into(),
            folder: "Clips".into(),
            expire_days: 0,
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
            share_kbps: 6000,
            encoder: "auto".into(),
            hwaccel: String::new(),
            ffmpeg_priority: FfmpegPriority::BelowNormal,
            ffmpeg_threads: 0,
            log_level: "info".into(),
            check_updates: true,
            setup_done: true,
            theme: "wardogs".into(),
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
    "clipDir",
    "port",
    "bind",
    "uiFile",
    "displayName",
    "shareKbps",
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
const NEXTCLOUD_KEYS: [&str; 4] = ["enabled", "url", "folder", "expireDays"];
const DISCORD_KEYS: [&str; 1] = ["enabled"];
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
                        _ => return Err(format!("unknown integration: {group}")),
                    };
                    let Some(fields) = fields.as_object() else {
                        return Err(format!("integrations.{group} must be an object"));
                    };
                    for (field, v) in fields {
                        if !allowed.contains(&field.as_str()) {
                            return Err(format!("unknown field: integrations.{group}.{field}"));
                        }
                        current["integrations"][group][field] = v.clone();
                    }
                }
            } else {
                current[key] = value.clone();
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
        anyhow::ensure!(
            (500..=100_000).contains(&self.share_kbps),
            "shareKbps must be between 500 and 100000"
        );
        anyhow::ensure!(
            !self.encoder.trim().is_empty(),
            "encoder must be 'auto' or an encoder name"
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
        assert_eq!(s.share_kbps, 6000);
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
                "shareKbps": 8000,
                "integrations": { "nextcloud": { "enabled": true, "url": "https://cloud.example.com" } }
            }))
            .unwrap();
        assert_eq!(next.share_kbps, 8000);
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
    fn restart_fields() {
        let s = Settings::default();
        let next = s
            .with_patch(&serde_json::json!({ "port": 9000, "shareKbps": 700 }))
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
