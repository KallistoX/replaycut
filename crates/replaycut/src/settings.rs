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
    pub integrations: Integrations,
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
            integrations: Integrations::default(),
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

impl Settings {
    /// Load the file, or write the defaults when it does not exist yet.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.is_file() {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            let settings: Settings = serde_json::from_str(text.trim_start_matches('\u{feff}'))
                .with_context(|| format!("{} is not valid settings JSON", path.display()))?;
            return Ok(settings);
        }
        let settings = Settings::default();
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
            !self.clip_dir.as_os_str().is_empty(),
            "clipDir must not be empty"
        );
        anyhow::ensure!(self.share_kbps >= 500, "shareKbps must be at least 500");
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
    fn validate_rejects_zero_port() {
        let s = Settings {
            port: 0,
            ..Settings::default()
        };
        assert!(s.validate().is_err());
    }
}
