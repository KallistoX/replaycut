//! In-memory state plus the persisted JSON files (titles, seen list, share
//! history). File formats are those of the 1.4 service so that a migration
//! only has to copy files.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::media::{Encoder, Media};
use crate::settings::Settings;
use crate::util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
#[allow(dead_code)] // used by the share pipeline (R3b)
pub const MAX_JOBS: usize = 30;
#[allow(dead_code)]
pub const MAX_HISTORY: usize = 200;
pub const HISTORY_IN_STATUS: usize = 50;

/// Audio modes of the share pipeline: id, label, ffmpeg mapping, tracks needed.
pub struct AudioMode {
    pub id: &'static str,
    pub label: &'static str,
    pub need: u32,
}

pub const AUDIO_MODES: [AudioMode; 4] = [
    AudioMode {
        id: "mix",
        label: "Mix (all)",
        need: 1,
    },
    AudioMode {
        id: "gamemic",
        label: "Game + microphone (no voice chat)",
        need: 4,
    },
    AudioMode {
        id: "game",
        label: "Game only",
        need: 3,
    },
    AudioMode {
        id: "gamediscord",
        label: "Game + voice chat (no microphone)",
        need: 4,
    },
];

#[derive(Debug, Clone, Serialize)]
pub struct Clip {
    pub name: String,
    pub base: String,
    pub path: String,
    pub size: u64,
    pub duration: f64,
    pub tracks: u32,
    pub created: String,
    pub preview: String,
    pub status: &'static str,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: String,
    pub base: String,
    pub start: f64,
    pub end: f64,
    pub seconds: f64,
    pub audio: String,
    pub kbps: u32,
    pub stage: String,
    pub percent: u8,
    pub ok: Option<bool>,
    pub error: Option<String>,
    pub at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(rename = "sizeMB", skip_serializing_if = "Option::is_none")]
    pub size_mb: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nc_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<String>,
}

impl Job {
    /// History entries are jobs without the transient fields.
    #[allow(dead_code)]
    pub fn history_entry(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(map) = v.as_object_mut() {
            for k in ["percent", "stage", "ok", "error"] {
                map.remove(k);
            }
        }
        v
    }
}

pub struct Paths {
    pub clip_dir: PathBuf,
    pub preview_dir: PathBuf,
    pub shared_dir: PathBuf,
    pub data_dir: PathBuf,
    pub names_file: PathBuf,
    pub seen_file: PathBuf,
    pub history_file: PathBuf,
    pub ui_file: PathBuf,
}

impl Paths {
    pub fn new(clip_dir: &Path, data_dir: &Path, ui_file: PathBuf) -> Self {
        Self {
            clip_dir: clip_dir.to_path_buf(),
            preview_dir: clip_dir.join(".preview"),
            shared_dir: clip_dir.join("shared"),
            data_dir: data_dir.to_path_buf(),
            names_file: data_dir.join("clip-names.json"),
            seen_file: data_dir.join("clip-seen.json"),
            history_file: data_dir.join("clip-history.json"),
            ui_file,
        }
    }

    pub fn preview_of(&self, base: &str) -> PathBuf {
        self.preview_dir.join(format!("{base}.mp4"))
    }
}

#[derive(Default)]
pub struct Inner {
    pub clips: BTreeMap<String, Clip>,
    pub names: BTreeMap<String, String>,
    pub seen: BTreeSet<String>,
    /// `false` on the very first run: existing clips are recorded without a notification.
    pub seen_ready: bool,
    pub history: Vec<Value>,
    pub jobs: HashMap<String, Job>,
    pub last: Option<Job>,
    pub current_job: Option<String>,
    pub scan_at: Option<String>,
}

pub struct AppState {
    pub settings: Settings,
    pub paths: Paths,
    pub media: Media,
    pub encoder: Encoder,
    pub dry_run: bool,
    pub inner: Mutex<Inner>,
    /// Wakes the scanner early (after a delete, for example).
    pub scan_wake: Notify,
}

#[derive(Debug)]
pub enum StateError {
    UnknownClip(String),
    ClipBusy,
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::UnknownClip(b) => write!(f, "unknown clip: {b}"),
            StateError::ClipBusy => write!(f, "this clip is being shared right now - please wait"),
        }
    }
}
impl std::error::Error for StateError {}

impl AppState {
    pub fn load(
        settings: Settings,
        paths: Paths,
        media: Media,
        encoder: Encoder,
        dry_run: bool,
    ) -> Result<Self> {
        for d in [
            &paths.data_dir,
            &paths.clip_dir,
            &paths.preview_dir,
            &paths.shared_dir,
        ] {
            std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
        }
        let mut inner = Inner::default();
        if paths.names_file.is_file() {
            match read_json(&paths.names_file) {
                Ok(Value::Object(map)) => {
                    for (k, v) in map {
                        if let Some(s) = v.as_str() {
                            inner.names.insert(k, s.to_string());
                        }
                    }
                }
                Ok(_) => tracing::warn!(
                    "{} has an unexpected shape - ignored",
                    paths.names_file.display()
                ),
                Err(e) => tracing::warn!("{} unreadable: {e}", paths.names_file.display()),
            }
        }
        inner.seen_ready = paths.seen_file.is_file();
        if inner.seen_ready {
            match read_json(&paths.seen_file) {
                Ok(Value::Array(items)) => {
                    inner
                        .seen
                        .extend(items.iter().filter_map(|v| v.as_str().map(str::to_string)));
                }
                Ok(_) => tracing::warn!(
                    "{} has an unexpected shape - ignored",
                    paths.seen_file.display()
                ),
                Err(e) => {
                    tracing::warn!("{} unreadable: {e}", paths.seen_file.display());
                    inner.seen_ready = false;
                }
            }
        }
        if paths.history_file.is_file() {
            match read_json(&paths.history_file) {
                Ok(Value::Array(items)) => inner.history = items,
                Ok(_) => tracing::warn!(
                    "{} has an unexpected shape - ignored",
                    paths.history_file.display()
                ),
                Err(e) => tracing::warn!("{} unreadable: {e}", paths.history_file.display()),
            }
        }
        Ok(Self {
            settings,
            paths,
            media,
            encoder,
            dry_run,
            inner: Mutex::new(inner),
            scan_wake: Notify::new(),
        })
    }

    // --- persistence (called with the lock held; the files are tiny) ---

    pub fn save_names(&self, inner: &Inner) {
        let v = Value::Object(
            inner
                .names
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
        );
        if let Err(e) = util::write_atomic(&self.paths.names_file, v.to_string().as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths.names_file.display());
        }
    }

    pub fn save_seen(&self, inner: &Inner) {
        let v = Value::Array(
            inner
                .seen
                .iter()
                .map(|s| Value::String(s.clone()))
                .collect(),
        );
        if let Err(e) = util::write_atomic(&self.paths.seen_file, v.to_string().as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths.seen_file.display());
        }
    }

    #[allow(dead_code)]
    pub fn save_history(&self, inner: &Inner) {
        let text = serde_json::to_string_pretty(&inner.history).unwrap_or_else(|_| "[]".into());
        if let Err(e) = util::write_atomic(&self.paths.history_file, text.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths.history_file.display());
        }
    }

    // --- queries and mutations used by the HTTP layer ---

    /// The `/api/clips` document.
    pub fn status(&self) -> Value {
        let inner = self.inner.lock();
        let mut clips: Vec<Value> = inner
            .clips
            .values()
            .map(|c| {
                let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
                v["title"] = Value::String(inner.names.get(&c.base).cloned().unwrap_or_default());
                v
            })
            .collect();
        clips.sort_by(|a, b| b["created"].as_str().cmp(&a["created"].as_str()));
        let history: Vec<Value> = inner
            .history
            .iter()
            .take(HISTORY_IN_STATUS)
            .cloned()
            .collect();
        let audio: Vec<Value> = AUDIO_MODES
            .iter()
            .map(|m| json!({ "id": m.id, "label": m.label, "need": m.need }))
            .collect();
        let nextcloud = self.dry_run || self.settings.integrations.nextcloud.enabled;
        let webhook = self.dry_run || self.settings.integrations.discord.enabled;
        json!({
            "clips": clips,
            "last": inner.last,
            "busy": inner.current_job.is_some(),
            "job": inner.current_job,
            "scanAt": inner.scan_at,
            "history": history,
            "config": {
                "shareKbps": self.settings.share_kbps,
                "expireDays": self.settings.integrations.nextcloud.expire_days,
                "version": VERSION,
                "encoder": self.encoder.name,
                "audio": audio,
                "webhook": webhook,
                "nextcloud": nextcloud,
            }
        })
    }

    pub fn history(&self) -> Value {
        json!({ "history": self.inner.lock().history })
    }

    pub fn job(&self, id: &str) -> Option<Job> {
        self.inner.lock().jobs.get(id).cloned()
    }

    pub fn set_title(&self, base: &str, name: &str) -> Result<String, StateError> {
        let mut inner = self.inner.lock();
        if !inner.clips.contains_key(base) {
            return Err(StateError::UnknownClip(base.to_string()));
        }
        let title = util::normalize_title(name);
        if title.is_empty() {
            inner.names.remove(base);
        } else {
            inner.names.insert(base.to_string(), title.clone());
        }
        self.save_names(&inner);
        tracing::info!("title for {base}: {title:?}");
        Ok(title)
    }

    /// Forget a clip after its files were removed. Returns the clip.
    pub fn take_clip_for_delete(&self, base: &str) -> Result<Clip, StateError> {
        let inner = self.inner.lock();
        let clip = inner
            .clips
            .get(base)
            .cloned()
            .ok_or_else(|| StateError::UnknownClip(base.to_string()))?;
        if let Some(id) = &inner.current_job {
            if inner.jobs.get(id).is_some_and(|j| j.base == base) {
                return Err(StateError::ClipBusy);
            }
        }
        Ok(clip)
    }

    pub fn forget_clip(&self, base: &str) {
        let mut inner = self.inner.lock();
        inner.clips.remove(base);
        if inner.names.remove(base).is_some() {
            self.save_names(&inner);
        }
        if inner.last.as_ref().is_some_and(|j| j.base == base) {
            inner.last = None;
        }
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)?;
    let text = text.trim_start_matches('\u{feff}');
    Ok(serde_json::from_str(text)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_entry_drops_transient_fields() {
        let job = Job {
            id: "abc".into(),
            stage: "done".into(),
            ok: Some(true),
            error: Some(String::new()),
            size_mb: Some(0.3),
            ..Job::default()
        };
        let e = job.history_entry();
        assert_eq!(e["id"], "abc");
        assert_eq!(e["sizeMB"], 0.3);
        for k in ["percent", "stage", "ok", "error"] {
            assert!(e.get(k).is_none(), "{k} present");
        }
    }

    #[test]
    fn job_serialises_like_the_contract() {
        let job = Job {
            id: "x".into(),
            stage: "encode".into(),
            ..Job::default()
        };
        let v = serde_json::to_value(&job).unwrap();
        assert!(v["ok"].is_null(), "ok is null while running");
        assert!(v.get("link").is_none(), "unset optional fields are absent");
        assert!(v.get("ncPath").is_none());
    }
}
