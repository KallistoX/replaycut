//! In-memory state plus the persisted JSON files (titles, seen list, share
//! history). File formats are those of the 1.4 service so that a migration
//! only has to copy files.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::auth::Sessions;
use crate::integrations::Integrations;
use crate::lifecycle::Shutdown;
use crate::media::{Encoder, Media};
use crate::platform;
use crate::settings::Settings;
use crate::tray::TrayHandle;
use crate::update::UpdateStatus;
use crate::util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_JOBS: usize = 30;
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
    // since 2.1: what the video is, for the setup wizard and browser hints
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
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

/// What a settings change may rebuild: the tools with their resource
/// limits, the detected encoder and the integrations.
pub struct Runtime {
    pub media: Media,
    pub encoder: Encoder,
    /// The `encoder` setting the detection ran for (`auto` or a name).
    pub encoder_setting: String,
    pub integrations: Integrations,
}

impl Runtime {
    /// Build from settings. `base` is the located ffmpeg without limits;
    /// `previous` lets an unchanged encoder setting skip the test encode.
    pub async fn build(
        base: &Media,
        settings: &Settings,
        dry_run: bool,
        previous: Option<&Runtime>,
    ) -> Result<Self> {
        let media = base
            .clone()
            .with_resource_limits(settings.ffmpeg_priority, settings.ffmpeg_threads());
        let encoder = match previous {
            Some(p) if p.encoder_setting == settings.encoder => p.encoder.clone(),
            _ => media.detect_encoder(&settings.encoder).await?,
        };
        let integrations = Integrations::build(settings, dry_run)?;
        Ok(Self {
            media,
            encoder,
            encoder_setting: settings.encoder.clone(),
            integrations,
        })
    }
}

/// Command-line overrides that must not be written back to settings.json.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub clip_dir: Option<PathBuf>,
    pub port: Option<u16>,
    pub bind: Option<String>,
    pub ui_file: Option<PathBuf>,
    pub log_level: Option<String>,
}

impl Overrides {
    /// Apply to settings read from the file.
    pub fn apply(&self, settings: &mut Settings) {
        if let Some(d) = &self.clip_dir {
            settings.clip_dir = d.clone();
        }
        if let Some(p) = self.port {
            settings.port = p;
        }
        if let Some(b) = &self.bind {
            settings.bind = b.clone();
        }
        if let Some(u) = &self.ui_file {
            settings.ui_file = u.clone();
        }
        if let Some(l) = &self.log_level {
            settings.log_level = l.clone();
        }
    }
}

pub struct AppState {
    settings: RwLock<Settings>,
    /// Where settings.json lives; saves go there minus the overrides.
    pub settings_path: PathBuf,
    pub overrides: Overrides,
    pub data_dir: PathBuf,
    paths: RwLock<Arc<Paths>>,
    /// ffmpeg as located, without resource limits.
    pub media_base: Media,
    runtime: RwLock<Arc<Runtime>>,
    pub dry_run: bool,
    /// obs-websocket client (status, requests, reconfigure).
    pub obs: Arc<crate::obs_ws::ObsHandle>,
    /// When this process started (uptime and the diagnostics header).
    pub started: std::time::Instant,
    pub started_at: String,
    pub sessions: Sessions,
    /// Set by main once the shutdown handle exists (for `POST /api/restart`).
    pub shutdown: std::sync::OnceLock<Shutdown>,
    /// The settings the process started with; restart-only fields are
    /// compared against these, so changing a value back clears the warning.
    boot_settings: Settings,
    /// Restart-only fields that differ from `boot_settings`.
    pub pending_restart: Mutex<Vec<&'static str>>,
    pub inner: Mutex<Inner>,
    /// Wakes the scanner early (after a delete, for example).
    pub scan_wake: Notify,
    /// Set once the tray exists; poked whenever clips or jobs change.
    pub tray: std::sync::OnceLock<TrayHandle>,
    /// The update check and the one-click update (see `update.rs`).
    pub update: Mutex<UpdateStatus>,
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

/// Everything `AppState::load` needs besides the state files.
pub struct Boot {
    pub settings: Settings,
    pub settings_path: PathBuf,
    pub overrides: Overrides,
    pub data_dir: PathBuf,
    pub ui_file: PathBuf,
    pub media_base: Media,
    pub runtime: Runtime,
    pub dry_run: bool,
    pub obs: Arc<crate::obs_ws::ObsHandle>,
}

fn create_dirs(paths: &Paths) -> Result<()> {
    for d in [
        &paths.data_dir,
        &paths.clip_dir,
        &paths.preview_dir,
        &paths.shared_dir,
    ] {
        std::fs::create_dir_all(d).with_context(|| format!("cannot create {}", d.display()))?;
    }
    Ok(())
}

impl AppState {
    pub fn load(boot: Boot) -> Result<Self> {
        let Boot {
            settings,
            settings_path,
            overrides,
            data_dir,
            ui_file,
            media_base,
            runtime,
            dry_run,
            obs,
        } = boot;
        let paths = Paths::new(&settings.clip_dir, &data_dir, ui_file);
        create_dirs(&paths)?;
        let sessions = Sessions::load(&data_dir.join("sessions.json"));
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
            boot_settings: settings.clone(),
            settings: RwLock::new(settings),
            settings_path,
            overrides,
            data_dir,
            paths: RwLock::new(Arc::new(paths)),
            media_base,
            runtime: RwLock::new(Arc::new(runtime)),
            dry_run,
            obs,
            started: std::time::Instant::now(),
            started_at: util::now_local(),
            sessions,
            shutdown: std::sync::OnceLock::new(),
            pending_restart: Mutex::new(Vec::new()),
            inner: Mutex::new(inner),
            scan_wake: Notify::new(),
            tray: std::sync::OnceLock::new(),
            update: Mutex::new(UpdateStatus::default()),
        })
    }

    /// A copy of the effective settings (file plus command-line overrides).
    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn paths(&self) -> Arc<Paths> {
        self.paths.read().clone()
    }

    pub fn runtime(&self) -> Arc<Runtime> {
        self.runtime.read().clone()
    }

    pub fn password_set(&self) -> bool {
        self.settings.read().password_hash.is_some()
    }

    /// Make new settings effective: rebuild what depends on them, note the
    /// fields that need a restart, wake the scanner. The caller has already
    /// validated and saved them.
    pub async fn apply_settings(&self, next: Settings) -> Result<Vec<&'static str>> {
        let current = self.settings();
        let restart = self.boot_settings.restart_needed(&next);
        let rebuild = current.encoder != next.encoder
            || current.hwaccel != next.hwaccel
            || current.ffmpeg_priority != next.ffmpeg_priority
            || current.ffmpeg_threads != next.ffmpeg_threads
            || current.display_name != next.display_name
            || serde_json::to_value(&current.integrations).ok()
                != serde_json::to_value(&next.integrations).ok();
        if rebuild {
            let previous = self.runtime();
            let runtime = Runtime::build(&self.media_base, &next, self.dry_run, Some(&previous))
                .await
                .context("cannot apply the new settings")?;
            tracing::info!(
                "settings applied: encoder {}, {}",
                runtime.encoder.name,
                runtime.integrations.describe()
            );
            *self.runtime.write() = Arc::new(runtime);
        }
        if current.clip_dir != next.clip_dir {
            let old = self.paths();
            let paths = Paths::new(&next.clip_dir, &old.data_dir, old.ui_file.clone());
            create_dirs(&paths)?;
            *self.paths.write() = Arc::new(paths);
            let mut inner = self.inner.lock();
            inner.clips.clear();
            inner.scan_at = None;
            tracing::info!("clip folder is now {}", next.clip_dir.display());
        }
        if current.obs != next.obs {
            self.obs.reconfigure(crate::obs_link::config_from(&next));
        }
        *self.settings.write() = next;
        *self.pending_restart.lock() = restart.clone();
        self.scan_wake.notify_one();
        self.tray_changed();
        Ok(restart)
    }

    /// Rebuild the integrations after credentials changed (settings unchanged).
    pub async fn rebuild_runtime(&self) -> Result<()> {
        let settings = self.settings();
        let previous = self.runtime();
        let runtime =
            Runtime::build(&self.media_base, &settings, self.dry_run, Some(&previous)).await?;
        *self.runtime.write() = Arc::new(runtime);
        Ok(())
    }

    /// The UI address on this machine.
    pub fn ui_url(&self) -> String {
        format!("http://localhost:{}/", self.settings.read().port)
    }

    /// The UI address for other devices in the network.
    pub fn lan_url(&self) -> String {
        format!(
            "http://{}:{}/",
            platform::hostname(),
            self.settings.read().port
        )
    }

    /// Tell the tray that clips or jobs changed.
    pub fn tray_changed(&self) {
        if let Some(tray) = self.tray.get() {
            tray.refresh();
        }
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
        if let Err(e) = util::write_atomic(&self.paths().names_file, v.to_string().as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths().names_file.display());
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
        if let Err(e) = util::write_atomic(&self.paths().seen_file, v.to_string().as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths().seen_file.display());
        }
    }

    pub fn save_history(&self, inner: &Inner) {
        let text = serde_json::to_string_pretty(&inner.history).unwrap_or_else(|_| "[]".into());
        if let Err(e) = util::write_atomic(&self.paths().history_file, text.as_bytes()) {
            tracing::warn!("cannot write {}: {e}", self.paths().history_file.display());
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
        let runtime = self.runtime();
        let obs = self.obs.status();
        let settings = self.settings.read();
        let nextcloud = runtime.integrations.storage.is_some();
        let webhook = runtime.integrations.notify.is_some();
        json!({
            "clips": clips,
            "last": inner.last,
            "busy": inner.current_job.is_some(),
            "job": inner.current_job,
            "scanAt": inner.scan_at,
            "history": history,
            "config": {
                "shareKbps": settings.share_kbps,
                "expireDays": settings.integrations.nextcloud.expire_days,
                "version": VERSION,
                "encoder": runtime.encoder.name,
                "audio": audio,
                "webhook": webhook,
                "nextcloud": nextcloud,
                "update": self.update.lock().latest.as_ref().map(|l| json!({ "version": l.version, "url": l.url })),
                // since 2.1
                "setupDone": settings.setup_done,
                "theme": settings.theme,
                "passwordSet": settings.password_hash.is_some(),
                "localMode": !nextcloud,
                "displayName": settings.display_name,
                // since 2.2
                "obs": { "connected": obs.connected, "replayActive": obs.replay_active, "enabled": obs.enabled },
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

impl AppState {
    /// Mutate a job in place (no-op when the id is unknown).
    pub fn with_job<F: FnOnce(&mut Job)>(&self, id: &str, f: F) {
        if let Some(job) = self.inner.lock().jobs.get_mut(id) {
            f(job);
        }
        self.tray_changed();
    }

    /// Register a new job as the running one and keep only the newest `MAX_JOBS`.
    pub fn register_job(&self, inner: &mut Inner, job: Job) {
        let id = job.id.clone();
        inner.jobs.insert(id.clone(), job);
        inner.current_job = Some(id);
        self.tray_changed();
        if inner.jobs.len() > MAX_JOBS {
            let mut by_age: Vec<(String, String)> = inner
                .jobs
                .values()
                .map(|j| (j.at.clone(), j.id.clone()))
                .collect();
            by_age.sort();
            for (_, old) in by_age.iter().take(inner.jobs.len() - MAX_JOBS) {
                if inner.current_job.as_deref() != Some(old.as_str()) {
                    inner.jobs.remove(old);
                }
            }
        }
    }

    /// Finish the running job: `done` goes to history and becomes `last`.
    pub fn complete_job(&self, id: &str, result: Result<(), String>) {
        let mut inner = self.inner.lock();
        let Some(job) = inner.jobs.get_mut(id) else {
            return;
        };
        match result {
            Ok(()) => {
                job.ok = Some(true);
                job.error = Some(String::new());
                job.stage = "done".into();
            }
            Err(msg) => {
                job.ok = Some(false);
                job.error = Some(msg);
                job.stage = "error".into();
            }
        }
        job.finished = Some(util::now_local());
        let job = job.clone();
        if job.ok == Some(true) {
            inner.history.insert(0, job.history_entry());
            inner.history.truncate(MAX_HISTORY);
            self.save_history(&inner);
        }
        inner.last = Some(job);
        if inner.current_job.as_deref() == Some(id) {
            inner.current_job = None;
        }
        drop(inner);
        self.tray_changed();
    }

    /// Remote paths recorded in history for a clip.
    pub fn history_paths_for(&self, base: &str) -> Vec<String> {
        self.inner
            .lock()
            .history
            .iter()
            .filter(|e| e["base"] == base)
            .filter_map(|e| e["ncPath"].as_str().map(str::to_string))
            .collect()
    }

    /// Drop a clip's history entries (after its remote copies were deleted).
    pub fn remove_history_for(&self, base: &str) {
        let mut inner = self.inner.lock();
        let before = inner.history.len();
        inner.history.retain(|e| e["base"] != base);
        if inner.history.len() != before {
            self.save_history(&inner);
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
