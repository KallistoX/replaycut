//! In-memory state plus the store it is read from and written to. Titles,
//! the seen list and the share history lived in three JSON files of the 1.4
//! format until 2.8; since 3.0 they are rows in `replaycut.db` (see `db.rs`),
//! which the first start imports them into.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use std::sync::Arc;

use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::auth::Sessions;
use crate::db::Db;
use crate::integrations::{Integrations, Storage, UserInfo};
use crate::lifecycle::Shutdown;
use crate::media::{Encoder, Media};
use crate::platform;
use crate::settings::Settings;
use crate::tray::TrayHandle;
use crate::update::UpdateStatus;
use crate::util;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_JOBS: usize = 30;
/// Shares waiting behind the running one (since 2.4); more is a misclick.
pub const MAX_QUEUE: usize = 10;
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

/// Since 3.10.1 a clip can also be read back from the document the store
/// kept for it: that is how a recording in a folder the service no longer
/// watches stays in the list. Everything the scanner measures has a default,
/// so a document written by an older build still reads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub name: String,
    pub base: String,
    pub path: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub tracks: u32,
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub preview: String,
    #[serde(skip_deserializing, default = "ready")]
    pub status: &'static str,
    // since 2.1: what the video is, for the setup wizard and browser hints
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub fps: f64,
    // since 2.4: `/media/<base>.jpg`, null until the thumbnail exists
    #[serde(default)]
    pub thumb: Option<String>,
    // since 2.6: `/media/<base>.h264.mp4`, the playable copy for browsers
    // that cannot decode the recording's codec; null until it was made
    #[serde(rename = "previewH264", default)]
    pub preview_h264: Option<String>,
}

/// The only status a clip in the list has ever had; the store keeps it in
/// the document, reading one back always means the recording is there.
fn ready() -> &'static str {
    "ready"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
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
    // since 2.4: `h264` (re-encode, the default) or `copy` (keyframe-accurate, no re-encode)
    #[serde(default = "default_mode")]
    pub mode: String,
    // since 2.5: the storage this job goes to, or `file`; empty in history entries from before
    #[serde(default)]
    pub target: String,
    // since 2.5: a publish job re-uses the file of this finished job
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    // since 2.6: `share` (the default) or `preview` (the playable H.264 copy
    // of a clip, stages `queued -> encode -> done`, never in the history);
    // since 3.0 also `cut` (make the cut file and stop), `render` (encode an
    // existing cut) and `publish` (send a finished file on)
    #[serde(default = "default_kind", skip_serializing_if = "is_share")]
    pub kind: String,
    // since 3.0: the cut this job cut, rendered or published from
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cut: Option<String>,
    // since 2.6: a preview made at scan time runs ffmpeg with idle priority
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub idle: bool,
    // since 2.7: the target's height cap the encode was scaled to (0 = the
    // recording's resolution); `kbps` is its bitrate cap, 0 = quality-driven
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_height: u32,
    // since 2.8: the recording's codec and total bitrate (kbit/s) when the
    // share was made, so the UI can learn the H.264-to-recording size ratio
    // from the history instead of guessing it per codec
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub codec: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub source_kbps: u32,
    // since 2.6: a 9:16 cut (`crop` at `verticalPos`, 0 = left edge, 1 = right edge)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub vertical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vertical_pos: Option<f64>,
    // since 2.4, copy mode: where the file really starts (the keyframe before `start`)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_start: Option<f64>,
    // since 2.4: place in the queue while `queued` (1 = next), absent otherwise
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    // since 2.4: ended by the user (stage `cancelled`)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    // since 3.0: what happens to the clip when this job is done -
    // `keep`, `done` or `recycle` (the recording goes to the recycle bin)
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub after: String,
    // since 3.10: the file was made but sending it failed - the job stays an
    // output "on this PC" and this is why it went nowhere
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_error: Option<String>,
    // since 3.11, a `transcribe` job: which model read the speech, which
    // language it was told to expect (`auto` for detection) and which track
    // it was asked for (`auto`, `mic` or `mix`). Empty on every other kind.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub language: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub track: String,
    // since 3.11: what a rendering does with the subtitles of its cut -
    // `none` (and then absent), `burn` or `track`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitles: String,
    // since 3.12.1: how many lines a job that read the speech got out of it
    // - a `transcribe` job, and a rendering that read first. 0 is a run that
    // worked and heard nothing it could put on the picture; absent until
    // the speech has been read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
}

fn default_mode() -> String {
    "h264".to_string()
}

fn default_kind() -> String {
    KIND_SHARE.to_string()
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn is_share(kind: &str) -> bool {
    // `Job::default()` leaves the kind empty: that is a share too
    kind.is_empty() || kind == KIND_SHARE
}

pub const KIND_SHARE: &str = "share";
pub const KIND_PREVIEW: &str = "preview";
/// Since 3.0: make the cut file and stop (`POST /api/cuts`).
pub const KIND_CUT: &str = "cut";
/// Since 3.0: encode and send a cut that exists (`POST /api/cuts/<id>/render`).
pub const KIND_RENDER: &str = "render";
/// Since 3.0: the finished file of a job to another target (`publish`); it
/// was a share with a `source` until 2.8.
pub const KIND_PUBLISH: &str = "publish";
/// Since 3.11: read the speech of a cut (`POST /api/cuts/<id>/transcribe`).
/// Like a preview it produces no output and never reaches the history.
pub const KIND_TRANSCRIBE: &str = "transcribe";

impl Job {
    pub fn is_preview(&self) -> bool {
        self.kind == KIND_PREVIEW
    }

    /// Housekeeping jobs leave nothing behind that the history should list:
    /// the playable preview and the cut file are not outputs.
    pub fn is_output(&self) -> bool {
        !self.is_preview() && self.kind != KIND_CUT && self.kind != KIND_TRANSCRIBE
    }
}

/// Nextcloud quota of the storage account, refreshed in the background (since 2.4).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Quota {
    pub used_percent: f64,
    pub free: u64,
    pub total: u64,
    pub checked_at: String,
}

impl Job {
    /// History entries are jobs without the transient fields.
    pub fn history_entry(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(map) = v.as_object_mut() {
            for k in ["percent", "stage", "ok", "error", "position"] {
                map.remove(k);
            }
        }
        v
    }
}

pub struct Paths {
    pub clip_dir: PathBuf,
    pub preview_dir: PathBuf,
    /// The cut files, `.cuts\<id>.mkv` (since 3.0).
    pub cuts_dir: PathBuf,
    pub shared_dir: PathBuf,
    pub data_dir: PathBuf,
    pub ui_file: PathBuf,
}

impl Paths {
    pub fn new(clip_dir: &Path, data_dir: &Path, ui_file: PathBuf) -> Self {
        Self {
            clip_dir: clip_dir.to_path_buf(),
            preview_dir: clip_dir.join(".preview"),
            cuts_dir: clip_dir.join(".cuts"),
            shared_dir: clip_dir.join("shared"),
            data_dir: data_dir.to_path_buf(),
            ui_file,
        }
    }

    /// The file of a cut; its name in the API is `<id>.mkv`.
    pub fn cut_of(&self, id: &str) -> PathBuf {
        self.cuts_dir.join(cut_file_name(id))
    }

    pub fn preview_of(&self, base: &str) -> PathBuf {
        self.preview_dir.join(format!("{base}.mp4"))
    }

    pub fn thumb_of(&self, base: &str) -> PathBuf {
        self.preview_dir.join(format!("{base}.jpg"))
    }

    /// The playable H.264 copy (since 2.6); served as `/media/<base>.h264.mp4`.
    pub fn preview_h264_of(&self, base: &str) -> PathBuf {
        self.preview_dir.join(format!("{base}.h264.mp4"))
    }

    /// The playable copies of cuts (since 3.12), `.cuts\play\`. A folder of
    /// its own because the sweep of `.cuts\` recycles every file whose name
    /// is not a cut id - an older build's sweep as well - and passes over
    /// folders.
    pub fn play_dir(&self) -> PathBuf {
        self.cuts_dir.join("play")
    }

    /// The remux of a cut for the browser; `/media/cuts/<id>.mp4`.
    pub fn cut_copy_of(&self, id: &str) -> PathBuf {
        self.play_dir().join(format!("{id}.mp4"))
    }

    /// The 720p H.264 copy of a cut; `/media/cuts/<id>.h264.mp4`.
    pub fn cut_copy_h264_of(&self, id: &str) -> PathBuf {
        self.play_dir().join(format!("{id}.h264.mp4"))
    }
}

/// What `.cuts\play\` holds for one cut (since 3.12). The status document
/// reads this instead of the disk; the sweep puts it right after every scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CutCopies {
    pub remux: bool,
    pub h264: bool,
    /// The remux is being made right now.
    pub preparing: bool,
}

/// The id of a cut as it appears in a file name of `.cuts\play\`: eight hex
/// characters, nothing that could leave the folder.
pub fn is_cut_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The file name of a cut in `.cuts\` (since 3.0).
pub fn cut_file_name(id: &str) -> String {
    format!("{id}.mkv")
}

/// The `/media/...` URL of the H.264 preview.
pub fn preview_h264_url(base: &str) -> String {
    format!("/media/{}.h264.mp4", util::encode_path_segment(base))
}

/// The URL of a cut's remux for the browser (since 3.12).
pub fn cut_copy_url(id: &str) -> String {
    format!("/media/cuts/{id}.mp4")
}

/// The URL of a cut's 720p H.264 copy (since 3.12).
pub fn cut_copy_h264_url(id: &str) -> String {
    format!("/media/cuts/{id}.h264.mp4")
}

/// A cut's playable copies in the document: the URL of each that exists,
/// and `preparing` while the remux runs (since 3.12).
fn put_copies(v: &mut Value, id: &str, c: CutCopies) {
    v["preview"] = if c.remux {
        Value::String(cut_copy_url(id))
    } else {
        Value::Null
    };
    v["previewH264"] = if c.h264 {
        Value::String(cut_copy_h264_url(id))
    } else {
        Value::Null
    };
    if c.preparing {
        v["preparing"] = Value::Bool(true);
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
    /// Jobs waiting for the worker, in order (since 2.4).
    pub queue: VecDeque<String>,
    pub scan_at: Option<String>,
}

/// What a settings change may rebuild: the tools with their resource
/// limits, the detected encoder and the integrations.
pub struct Runtime {
    pub media: Media,
    pub encoder: Encoder,
    /// The `encoder` and `hwaccel` settings the detection ran for.
    pub encoder_setting: String,
    pub integrations: Integrations,
    /// Whether this ffmpeg can transcribe (since 3.11): it is a property of
    /// the binary, so it is found once and carried along.
    pub whisper: bool,
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
        let encoder_setting = format!("{}|{}", settings.encoder, settings.hwaccel);
        let encoder = match previous {
            Some(p) if p.encoder_setting == encoder_setting => p.encoder.clone(),
            _ => {
                let sample = crate::media::detection_sample(&settings.clip_dir);
                media
                    .detect_encoder(
                        &settings.encoder,
                        &crate::media::hwaccel_mode(&settings.hwaccel),
                        sample.as_deref(),
                    )
                    .await?
            }
        };
        let integrations = Integrations::build(settings, dry_run)?;
        let whisper = match previous {
            Some(p) => p.whisper,
            None => media.has_filter("whisper").await,
        };
        Ok(Self {
            media,
            encoder,
            encoder_setting,
            integrations,
            whisper,
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
    /// Clips, cuts and jobs; the truth behind the maps in `inner` (since 3.0).
    pub db: Db,
    paths: RwLock<Arc<Paths>>,
    /// ffmpeg as located, without resource limits.
    pub media_base: Media,
    runtime: RwLock<Arc<Runtime>>,
    pub dry_run: bool,
    /// What TLS is doing (since 3.4): the scheme of every address the
    /// service hands out, and what the diagnostics and the settings page say
    /// about the certificate.
    pub tls: crate::tls::Info,
    /// obs-websocket client (status, requests, reconfigure).
    pub obs: Arc<crate::obs_ws::ObsHandle>,
    /// When this process started (uptime and the diagnostics header).
    pub started: std::time::Instant,
    pub started_at: String,
    pub sessions: Sessions,
    /// The device login: devices waiting for this PC (since 2.8).
    pub pairing: crate::pairing::Pairing,
    /// Set by main once the shutdown handle exists (for `POST /api/restart`).
    pub shutdown: std::sync::OnceLock<Shutdown>,
    /// The settings the process started with; restart-only fields are
    /// compared against these, so changing a value back clears the warning.
    boot_settings: Settings,
    /// Restart-only fields that differ from `boot_settings`.
    pub pending_restart: Mutex<Vec<&'static str>>,
    pub inner: Mutex<Inner>,
    /// One token per known job; cancelling it ends the running pipeline.
    pub cancels: Mutex<HashMap<String, CancellationToken>>,
    /// Device-code flows in progress or just finished (since 2.5).
    pub oauth: crate::oauth::Flows,
    /// Bumped on every change the UI cares about; `GET /api/events` listens (since 2.4).
    pub events: tokio::sync::watch::Sender<u64>,
    /// Open event streams, capped in the handler.
    pub sse_clients: std::sync::atomic::AtomicUsize,
    /// Shares that had to retry with software decoding since start (since 2.4).
    pub encoder_fallbacks: std::sync::atomic::AtomicU32,
    /// The storage quota, when the account has one (since 2.4).
    pub quota: Mutex<Option<Quota>>,
    /// Asks the quota loop to refresh now (after an upload, a settings change).
    pub quota_wake: Notify,
    /// Wakes the scanner early (after a delete, for example).
    pub scan_wake: Notify,
    /// "Pause scanning" in the tray: new replays wait in the folder. RAM only.
    pub scanning_paused: std::sync::atomic::AtomicBool,
    /// Set once the tray exists; poked whenever clips or jobs change.
    pub tray: std::sync::OnceLock<TrayHandle>,
    /// The update check and the one-click update (see `update.rs`).
    pub update: Mutex<UpdateStatus>,
    /// Model downloads in flight (since 3.11), by model name. A model is
    /// not a job: it takes no place in the queue and the UI polls the list.
    pub model_downloads: Mutex<HashMap<String, crate::subtitles::Download>>,
    /// The playable copies of the cuts, by cut id (since 3.12).
    pub cut_copies: Mutex<BTreeMap<String, CutCopies>>,
    /// One remux of a cut at a time; it is disk work and waits for no job.
    pub cut_copy_slot: tokio::sync::Semaphore,
}

#[derive(Debug)]
pub enum StateError {
    UnknownClip(String),
    ClipBusy,
    UnknownJob,
    /// The job is past the point where cancelling makes sense.
    TooLate(String),
    /// The store could not be written (since 3.0).
    Store(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StateError::UnknownClip(b) => write!(f, "unknown clip: {b}"),
            StateError::ClipBusy => write!(f, "this clip is being shared right now - please wait"),
            StateError::UnknownJob => write!(f, "unknown job"),
            StateError::TooLate(stage) => write!(f, "too late to cancel - the job is {stage}"),
            StateError::Store(e) => write!(f, "the state store refused: {e}"),
        }
    }
}

/// What a clip's state is: what the store says, or - for a clip nobody has
/// touched - whether it has cuts (since 3.0).
pub fn clip_state(row: Option<&crate::db::ClipRow>, cuts: &[Value]) -> &'static str {
    match row.map(|r| r.state.as_str()) {
        Some(crate::db::CLIP_DONE) => crate::db::CLIP_DONE,
        Some(crate::db::CLIP_ACTIVE) if !cuts.is_empty() => crate::db::CLIP_ACTIVE,
        _ if !cuts.is_empty() => crate::db::CLIP_ACTIVE,
        _ => crate::db::CLIP_NEW,
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
    pub tls: crate::tls::Info,
    pub obs: Arc<crate::obs_ws::ObsHandle>,
}

/// What a change of the clip folder did before 3.10.1 (issue #40), undone
/// once, on the start that brings the store to schema 3: every clip it put
/// away although its recording is still there comes back.
///
/// Two shapes of the same damage. A clip the scan never met again is simply
/// marked as having no recording - if the file is there, it was the folder
/// that changed, not the file that went. A clip the scan did meet again
/// stayed `done` with its cuts `missing`; that pair - recording there, cut
/// file there, store saying otherwise - is what only this bug produced.
fn heal_after_a_folder_change(db: &Db, clip_dir: &Path) {
    let rows = match db.clips() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("cannot look for clips to bring back: {e:#}");
            return;
        }
    };
    let cuts = db.cuts().unwrap_or_default();
    let mut back = 0usize;
    for (base, row) in &rows {
        let dir = row
            .dir
            .as_deref()
            .map_or_else(|| clip_dir.to_path_buf(), std::path::PathBuf::from);
        let recording = row
            .doc
            .as_ref()
            .and_then(|d| d.get("path").and_then(|p| p.as_str()))
            .filter(|p| !p.is_empty())
            .map_or_else(|| dir.join(format!("{base}.mkv")), std::path::PathBuf::from);
        if !recording.is_file() {
            continue;
        }
        let own_cuts: Vec<_> = cuts.iter().filter(|c| &c.base == base).collect();
        let cut_is_there = own_cuts.iter().any(|c| {
            c.state == crate::db::CUT_MISSING
                && dir.join(".cuts").join(cut_file_name(&c.id)).is_file()
        });
        if row.has_file && !cut_is_there {
            continue;
        }
        if let Err(e) = db.set_clip_file(base, true, false) {
            tracing::warn!("cannot bring {base} back: {e:#}");
            continue;
        }
        if row.state == crate::db::CLIP_DONE {
            let state = if own_cuts.is_empty() {
                crate::db::CLIP_NEW
            } else {
                crate::db::CLIP_ACTIVE
            };
            if let Err(e) = db.set_clip_state(base, state, None) {
                tracing::warn!("cannot bring {base} back into the list: {e:#}");
                continue;
            }
        }
        back += 1;
    }
    if back > 0 {
        tracing::info!("{back} clip(s) brought back: their recording was never gone, the clip folder had changed");
    }
}

/// The folders a start needs. `.cuts\` and `shared\` come with the first cut
/// and the first share (since 3.10): a first start, before the setup chose
/// a folder, put both into the default one - `shared` visibly into Videos.
fn create_dirs(paths: &Paths) -> Result<()> {
    for d in [&paths.data_dir, &paths.clip_dir, &paths.preview_dir] {
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
            tls,
            obs,
        } = boot;
        let paths = Paths::new(&settings.clip_dir, &data_dir, ui_file);
        create_dirs(&paths)?;
        let sessions = Sessions::load(&data_dir.join("sessions.json"));
        let db = Db::open(&data_dir.join(crate::db::FILE))?;
        match db.import_2x(&data_dir, &paths.clip_dir) {
            Ok(Some(i)) => tracing::info!(
                "state of 2.x imported: {} titles, {} clips announced, {} history entries under {} cuts, {} clips without their recording (listed as done); the files are now in {}",
                i.titles,
                i.seen,
                i.jobs,
                i.cuts,
                i.gone,
                i.backup.display()
            ),
            Ok(None) => {}
            Err(e) => tracing::warn!("cannot import the state files of 2.x: {e:#}"),
        }
        if db.upgraded_to_3() {
            heal_after_a_folder_change(&db, &paths.clip_dir);
        }
        let inner = Inner {
            names: db.titles().unwrap_or_else(|e| {
                tracing::warn!("cannot read the titles: {e:#}");
                BTreeMap::new()
            }),
            seen: db.seen().unwrap_or_else(|e| {
                tracing::warn!("cannot read the seen list: {e:#}");
                BTreeSet::new()
            }),
            seen_ready: db.seen_ready(),
            history: db.recent_jobs(MAX_HISTORY).unwrap_or_else(|e| {
                tracing::warn!("cannot read the history: {e:#}");
                Vec::new()
            }),
            ..Inner::default()
        };
        Ok(Self {
            boot_settings: settings.clone(),
            settings: RwLock::new(settings),
            settings_path,
            overrides,
            data_dir,
            db,
            paths: RwLock::new(Arc::new(paths)),
            media_base,
            runtime: RwLock::new(Arc::new(runtime)),
            dry_run,
            tls,
            obs,
            started: std::time::Instant::now(),
            started_at: util::now_local(),
            sessions,
            pairing: crate::pairing::Pairing::new(),
            shutdown: std::sync::OnceLock::new(),
            pending_restart: Mutex::new(Vec::new()),
            inner: Mutex::new(inner),
            cancels: Mutex::new(HashMap::new()),
            quota: Mutex::new(None),
            encoder_fallbacks: std::sync::atomic::AtomicU32::new(0),
            events: tokio::sync::watch::channel(0u64).0,
            oauth: parking_lot::Mutex::new(HashMap::new()),
            sse_clients: std::sync::atomic::AtomicUsize::new(0),
            quota_wake: Notify::new(),
            scan_wake: Notify::new(),
            scanning_paused: std::sync::atomic::AtomicBool::new(false),
            tray: std::sync::OnceLock::new(),
            update: Mutex::new(UpdateStatus::default()),
            model_downloads: Mutex::new(HashMap::new()),
            cut_copies: Mutex::new(BTreeMap::new()),
            cut_copy_slot: tokio::sync::Semaphore::new(1),
        })
    }

    /// A copy of the effective settings (file plus command-line overrides).
    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    /// The paths of one clip (since 3.10.1, issue #40). A recording in a
    /// folder the service no longer watches keeps its preview, its cuts and
    /// its outputs where they are, so changing the folder moves nothing and
    /// hides nothing. For everything in the current folder - which is every
    /// clip until somebody changes it - this is the current `Paths`.
    pub fn paths_for(&self, base: &str) -> Arc<Paths> {
        let current = self.paths();
        // The store, not the clip cache: this is called from places that
        // already hold the cache lock, and the store is where the folder is
        // written anyway.
        let dir = self
            .db
            .clip(base)
            .ok()
            .flatten()
            .and_then(|r| r.dir)
            .map(PathBuf::from);
        match dir {
            Some(d) if !util::same_folder(&d, &current.clip_dir) => {
                Arc::new(Paths::new(&d, &current.data_dir, current.ui_file.clone()))
            }
            _ => current,
        }
    }

    /// The paths of the clip a cut belongs to.
    pub fn paths_of_cut(&self, cut: &crate::db::Cut) -> Arc<Paths> {
        self.paths_for(&cut.base)
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

    /// The extra host names of the settings, for the Host check on every
    /// request (since 2.8): cheaper than cloning all settings.
    pub fn allowed_hosts(&self) -> Vec<String> {
        self.settings.read().allowed_hosts.clone()
    }

    /// Whether this PC has to sign in as well (since 2.8).
    pub fn require_login_on_loopback(&self) -> bool {
        self.settings.read().require_login_on_loopback
    }

    /// What `GET /api/session` reports as `network` (since 2.8): the bind
    /// address as the network card of the settings sees it.
    pub fn network_mode(&self) -> &'static str {
        match self.settings.read().bind.as_str() {
            "127.0.0.1" | "::1" => "loopback",
            "0.0.0.0" | "::" => "lan",
            _ => "custom",
        }
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
            // The clips of the old folder stay in the list (since 3.10.1,
            // issue #40): each one knows where its recording is, and the
            // next scan checks that file instead of the folder listing.
            // Until 3.10 the cache was emptied here, which made every one of
            // them look like a recording that had disappeared.
            let mut inner = self.inner.lock();
            inner.scan_at = None;
            tracing::info!("clip folder is now {}", next.clip_dir.display());
        }
        if current.obs != next.obs {
            self.obs.reconfigure(crate::obs_link::config_from(&next));
        }
        *self.settings.write() = next;
        *self.pending_restart.lock() = restart.clone();
        self.scan_wake.notify_one();
        self.quota_wake.notify_one();
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
    /// `https` once TLS is actually running, `http` otherwise (since 3.4).
    /// Everything that hands out an address goes through here, so the toasts,
    /// the tray and the QR code follow the switch on their own.
    pub fn scheme(&self) -> &'static str {
        if self.tls.active {
            "https"
        } else {
            "http"
        }
    }

    pub fn ui_url(&self) -> String {
        format!(
            "{}://localhost:{}/",
            self.scheme(),
            self.settings.read().port
        )
    }

    /// The UI address for other devices in the network (the tray's "Copy
    /// address").
    #[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
    pub fn lan_url(&self) -> String {
        format!(
            "{}://{}:{}/",
            self.scheme(),
            platform::lan_host(),
            self.settings.read().port
        )
    }

    pub fn scanning_paused(&self) -> bool {
        self.scanning_paused
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Pause or resume the folder scan (tray and `POST /api/scanning`).
    pub fn set_scanning_paused(&self, paused: bool) {
        let was = self
            .scanning_paused
            .swap(paused, std::sync::atomic::Ordering::Relaxed);
        if was != paused {
            tracing::info!("scanning {}", if paused { "paused" } else { "resumed" });
            self.scan_wake.notify_one();
            self.tray_changed();
        }
    }

    /// Remember what the storage account reports; `None` clears it.
    pub fn set_quota(&self, info: Option<&UserInfo>) {
        let quota = info.and_then(|i| match (i.free, i.total) {
            (Some(free), Some(total)) if total > 0 => Some(Quota {
                used_percent: ((1.0 - free as f64 / total as f64) * 1000.0).round() / 10.0,
                free,
                total,
                checked_at: util::now_local(),
            }),
            _ => None,
        });
        *self.quota.lock() = quota;
        self.tray_changed();
    }

    /// Ask the storage account for its quota (one request; errors clear it).
    pub async fn refresh_quota(&self) {
        let runtime = self.runtime();
        let info = match runtime
            .integrations
            .storage("nextcloud")
            .map(|e| &e.storage)
        {
            Some(Storage::Nextcloud(nc)) => match nc.user_info().await {
                Ok(i) => Some(i),
                Err(e) => {
                    tracing::debug!("quota: {e:#}");
                    None
                }
            },
            _ => None,
        };
        self.set_quota(info.as_ref());
    }

    /// Tell the tray and the event streams that clips, jobs or status changed.
    pub fn tray_changed(&self) {
        if let Some(tray) = self.tray.get() {
            tray.refresh();
        }
        self.events.send_modify(|v| *v = v.wrapping_add(1));
    }

    // --- persistence (called with the lock held; the writes are tiny) ---

    pub fn save_names(&self, inner: &Inner) {
        if let Err(e) = self.db.save_titles(&inner.names) {
            tracing::warn!("cannot store the titles: {e:#}");
        }
    }

    pub fn save_seen(&self, inner: &Inner) {
        if let Err(e) = self.db.save_seen(&inner.seen) {
            tracing::warn!("cannot store the seen list: {e:#}");
        }
    }

    /// Keep a finished job: in the store and at the front of the cache the
    /// status document and `GET /api/history` are served from.
    fn record_job(&self, inner: &mut Inner, entry: Value) {
        let cut = entry["cut"].as_str().map(str::to_string);
        if let Err(e) = self.db.put_job(&entry, cut.as_deref()) {
            tracing::warn!("cannot store the job: {e:#}");
        }
        inner.history.insert(0, entry);
        inner.history.truncate(MAX_HISTORY);
    }

    // --- queries and mutations used by the HTTP layer ---

    /// `PUT /api/clips/<base>/state` (since 3.0): take a clip out of the list
    /// or bring it back. A running job is not touched by it.
    pub fn set_clip_state(&self, base: &str, state: &str) -> Result<Value, StateError> {
        let known =
            self.inner.lock().clips.contains_key(base) || matches!(self.db.clip(base), Ok(Some(_)));
        if !known {
            return Err(StateError::UnknownClip(base.to_string()));
        }
        let done_at = (state == crate::db::CLIP_DONE).then(util::now_local);
        // "active" without cuts is "new": the state follows what is there
        let state = if state == crate::db::CLIP_ACTIVE && !self.has_cuts(base) {
            crate::db::CLIP_NEW
        } else {
            state
        };
        self.db
            .set_clip_state(base, state, done_at.as_deref())
            .map_err(|e| StateError::Store(format!("{e:#}")))?;
        tracing::info!("clip {base} is {state}");
        self.tray_changed();
        Ok(json!({ "ok": true, "base": base, "state": state, "doneAt": done_at }))
    }

    /// A job of this cut is running or waiting (since 3.0).
    pub fn cut_busy(&self, cut: &str) -> bool {
        let inner = self.inner.lock();
        inner
            .current_job
            .iter()
            .chain(inner.queue.iter())
            .any(|id| {
                inner
                    .jobs
                    .get(id)
                    .is_some_and(|j| j.cut.as_deref() == Some(cut))
            })
    }

    pub fn has_cuts(&self, base: &str) -> bool {
        self.db
            .cuts_of(base)
            .map(|c| !c.is_empty())
            .unwrap_or(false)
    }

    /// A clip that lost its last cut is a fresh clip again (since 3.0).
    pub fn reset_state_without_cuts(&self, base: &str) {
        if self.has_cuts(base) {
            return;
        }
        if let Err(e) = self.db.set_clip_state(base, crate::db::CLIP_NEW, None) {
            tracing::warn!("cannot reset the state of {base}: {e:#}");
        }
    }

    /// The cuts of every clip with their outputs, for the status document
    /// (since 3.0). Two queries, whatever the number of clips.
    fn cuts_by_base(&self) -> BTreeMap<String, Vec<Value>> {
        let cuts = self.db.cuts().unwrap_or_else(|e| {
            tracing::warn!("cannot read the cuts: {e:#}");
            Vec::new()
        });
        let mut outputs = self.db.outputs_by_cut().unwrap_or_else(|e| {
            tracing::warn!("cannot read the outputs: {e:#}");
            BTreeMap::new()
        });
        // since 3.11: what a cut says about its subtitles here is the
        // summary, never the text - this document is polled and pushed
        let mut subtitles = self.db.subtitle_summaries().unwrap_or_else(|e| {
            tracing::warn!("cannot read the subtitle summaries: {e:#}");
            BTreeMap::new()
        });
        let copies = self.cut_copies.lock().clone();
        let mut by_base: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for cut in cuts {
            let mut v = serde_json::to_value(&cut).unwrap_or(Value::Null);
            v["outputs"] = Value::Array(outputs.remove(&cut.id).unwrap_or_default());
            if let Some(s) = subtitles.remove(&cut.id) {
                v["subtitles"] = serde_json::to_value(s).unwrap_or(Value::Null);
            }
            put_copies(
                &mut v,
                &cut.id,
                copies.get(&cut.id).copied().unwrap_or_default(),
            );
            by_base.entry(cut.base).or_default().push(v);
        }
        by_base
    }

    /// What `.cuts\play\` holds for one cut (since 3.12).
    pub fn cut_copies_of(&self, id: &str) -> CutCopies {
        self.cut_copies.lock().get(id).copied().unwrap_or_default()
    }

    /// Change what is known about one cut's copies and tell the pages.
    pub fn update_cut_copies(&self, id: &str, f: impl FnOnce(&mut CutCopies)) {
        {
            let mut all = self.cut_copies.lock();
            let entry = all.entry(id.to_string()).or_default();
            f(entry);
            if *entry == CutCopies::default() {
                all.remove(id);
            }
        }
        self.tray_changed();
    }

    /// Look at `.cuts\play\` again for these cuts (since 3.12): on start and
    /// after every scan, so a copy that was removed by hand or appeared with
    /// a folder that came back is what the page is told. A remux that is
    /// running stays `preparing`. Cuts that are gone are forgotten.
    pub fn refresh_cut_copies(&self, cuts: &[crate::db::Cut]) {
        let found: Vec<(String, bool, bool)> = cuts
            .iter()
            .map(|c| {
                let paths = self.paths_of_cut(c);
                (
                    c.id.clone(),
                    paths.cut_copy_of(&c.id).is_file(),
                    paths.cut_copy_h264_of(&c.id).is_file(),
                )
            })
            .collect();
        let changed = {
            let mut all = self.cut_copies.lock();
            let before = all.clone();
            let mut next = BTreeMap::new();
            for (id, remux, h264) in found {
                let preparing = before.get(&id).is_some_and(|c| c.preparing);
                let c = CutCopies {
                    remux,
                    h264,
                    preparing,
                };
                if c != CutCopies::default() {
                    next.insert(id, c);
                }
            }
            let changed = next != before;
            *all = next;
            changed
        };
        if changed {
            self.tray_changed();
        }
    }

    /// Remove a cut's playable copies, finished or not (since 3.12). They
    /// are made from the cut, so they go with it and never to the recycle
    /// bin.
    pub fn remove_cut_copies(&self, paths: &Paths, id: &str) {
        let (remux, h264) = (paths.cut_copy_of(id), paths.cut_copy_h264_of(id));
        for f in [
            crate::share::part_of(&remux),
            crate::share::part_of(&h264),
            remux,
            h264,
        ] {
            if f.is_file() {
                if let Err(e) = std::fs::remove_file(&f) {
                    tracing::warn!("cannot remove {}: {e}", f.display());
                }
            }
        }
        self.cut_copies.lock().remove(id);
    }

    /// The `/api/clips` document without the clips that are done.
    pub fn status(&self) -> Value {
        self.status_for(false)
    }

    /// The `/api/clips` document. `done` clips are out of the list unless
    /// `include_done` (since 3.0, `GET /api/clips?done=1`); a clip whose
    /// recording is gone stays in it as long as it has cuts.
    pub fn status_for(&self, include_done: bool) -> Value {
        let mut cuts = self.cuts_by_base();
        let rows = self.db.clips().unwrap_or_else(|e| {
            tracing::warn!("cannot read the clips: {e:#}");
            BTreeMap::new()
        });
        let inner = self.inner.lock();
        let (mut active, mut done) = (0usize, 0usize);
        let mut clips: Vec<Value> = Vec::new();
        let mut push = |mut v: Value, base: &str, cuts: Vec<Value>| {
            let row = rows.get(base);
            let state = clip_state(row, &cuts);
            if state == crate::db::CLIP_DONE {
                done += 1;
            } else {
                active += 1;
            }
            if state == crate::db::CLIP_DONE && !include_done {
                return;
            }
            v["title"] = Value::String(inner.names.get(base).cloned().unwrap_or_default());
            // since 3.0: the state, and the ranges with what came out of them
            v["state"] = Value::String(state.to_string());
            v["doneAt"] = row
                .and_then(|r| r.done_at.clone())
                .map_or(Value::Null, Value::String);
            v["firstSeen"] = row
                .and_then(|r| r.first_seen.clone())
                .map_or(Value::Null, Value::String);
            v["cuts"] = Value::Array(cuts);
            clips.push(v);
        };
        for c in inner.clips.values() {
            let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
            v["file"] = Value::String(c.name.clone());
            push(v, &c.base, cuts.remove(&c.base).unwrap_or_default());
        }
        // Clips whose recording is gone: they live on for their cuts, with
        // everything the scanner last knew about them (since 3.0).
        for (base, row) in rows.iter().filter(|(b, r)| {
            !r.has_file && r.doc.is_some() && !inner.clips.contains_key(b.as_str())
        }) {
            let Some(cuts) = cuts.remove(base).filter(|c| !c.is_empty()) else {
                continue;
            };
            let mut v = row.doc.clone().unwrap_or(Value::Null);
            v["file"] = Value::Null;
            v["size"] = json!(0);
            v["previewH264"] = Value::Null;
            push(v, base, cuts);
        }
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
        let nextcloud = runtime.integrations.default_storage().is_some();
        let webhook = runtime.integrations.auto_notifies().next().is_some();
        let targets = runtime.integrations.targets(&settings);
        json!({
            "clips": clips,
            // since 3.0: what the filter above the list says
            "counts": { "active": active, "done": done },
            "last": inner.last,
            "busy": inner.current_job.is_some(),
            "job": inner.current_job,
            "queue": inner.queue,
            "scanAt": inner.scan_at,
            "history": history,
            "config": {
                "shareKbps": 0,
                "expireDays": settings.integrations.nextcloud.expire_days,
                "version": VERSION,
                "encoder": runtime.encoder.name,
                // since 3.2: the page words things for the platform
                "platform": std::env::consts::OS,
                // since 3.10.1: the folder being watched, so the page can
                // say which clips are recorded somewhere else
                "clipDir": settings.clip_dir.to_string_lossy(),
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
                // since 3.4: whether the page runs in a secure context, so it
                // can drop the clipboard fallback
                "https": self.tls.active,
                // since 2.3
                "scanning": { "paused": self.scanning_paused() },
                // since 2.4
                "quota": *self.quota.lock(),
                // since 2.5
                "targets": targets,
                // since 3.0: what the "Afterwards" menu of the share row starts with
                "cleanup": settings.cleanup,
                // since 3.11 (beta, off by default): with this false the page
                // shows no subtitle block and no subtitle choice at all
                "subtitles": settings.subtitles.enabled,
                // since 2.8: with `lan` and no password every device in the
                // network may use this replaycut - the page says so
                "network": match settings.bind.as_str() {
                    "127.0.0.1" | "::1" => "loopback",
                    "0.0.0.0" | "::" => "lan",
                    _ => "custom",
                },
            }
        })
    }

    /// `GET /api/cuts/<id>`: the cut with its outputs (since 3.0).
    pub fn cut_document(&self, id: &str) -> Option<Value> {
        let cut = match self.db.cut(id) {
            Ok(c) => c?,
            Err(e) => {
                tracing::warn!("cannot read cut {id}: {e:#}");
                return None;
            }
        };
        let outputs = self
            .db
            .outputs_by_cut()
            .unwrap_or_default()
            .remove(&cut.id)
            .unwrap_or_default();
        let mut v = serde_json::to_value(&cut).unwrap_or(Value::Null);
        v["outputs"] = Value::Array(outputs);
        // the summary, as in the clip list; the segments have their own
        // endpoint (since 3.11)
        if let Ok(Some(s)) = self.db.subtitles(&cut.id).map(|s| s.map(|s| s.summary())) {
            v["subtitles"] = serde_json::to_value(s).unwrap_or(Value::Null);
        }
        put_copies(&mut v, &cut.id, self.cut_copies_of(&cut.id));
        Some(v)
    }

    /// `GET /api/history[?limit=&before=]`. Since 3.0 the store keeps every
    /// entry; the cache in `inner` is only what the status document shows.
    pub fn history_page(&self, limit: usize, before: Option<&str>) -> Value {
        match self.db.jobs_before(before, limit) {
            Ok(history) => json!({ "history": history }),
            Err(e) => {
                tracing::warn!("cannot read the history: {e:#}");
                json!({ "history": self.inner.lock().history })
            }
        }
    }

    pub fn job(&self, id: &str) -> Option<Job> {
        self.inner.lock().jobs.get(id).cloned()
    }

    /// The finished file of a job, from the jobs or the store (since 2.6),
    /// with the clip it belongs to: since 3.10.1 the folder of that clip is
    /// where the file is.
    pub fn job_output(&self, id: &str) -> Option<(String, String)> {
        if let Some(j) = self.inner.lock().jobs.get(id) {
            return j
                .file
                .clone()
                .filter(|_| j.ok == Some(true))
                .map(|f| (j.base.clone(), f));
        }
        let entry = self.db.entry(id).ok().flatten()?;
        let file = entry["file"].as_str()?.to_string();
        Some((entry["base"].as_str().unwrap_or_default().to_string(), file))
    }

    /// A finished job read back from its stored entry (since 2.6.1). The
    /// store keeps every one of them, however old.
    pub fn history_job(&self, id: &str) -> Option<Job> {
        self.db
            .entry(id)
            .ok()
            .flatten()
            .and_then(|e| serde_json::from_value::<Job>(e).ok())
            .map(|mut j| {
                j.ok = Some(true);
                j
            })
    }

    /// Append a manual post's status to the job's `discord` field, in the
    /// jobs and in the history (since 2.7).
    pub fn note_post(&self, id: &str, note: &str) {
        let mut inner = self.inner.lock();
        let join = |cur: Option<&str>| match cur {
            Some(c) if !c.is_empty() => format!("{c} · {note}"),
            _ => note.to_string(),
        };
        if let Some(j) = inner.jobs.get_mut(id) {
            j.discord = Some(join(j.discord.as_deref()));
        }
        // `last` is a copy of the finished job and what the result card shows
        // after a reload; without the note it would offer "Post to" again
        if let Some(l) = inner.last.as_mut().filter(|l| l.id == id) {
            l.discord = Some(join(l.discord.as_deref()));
        }
        for e in inner.history.iter_mut() {
            if e["id"] == id {
                let cur = e["discord"].as_str().map(str::to_string);
                e["discord"] = json!(join(cur.as_deref()));
            }
        }
        // The store keeps every job, the cache only the newest ones.
        match self.db.entry(id) {
            Ok(Some(mut e)) => {
                let cur = e["discord"].as_str().map(str::to_string);
                e["discord"] = json!(join(cur.as_deref()));
                if let Err(e) = self.db.update_entry(id, &e) {
                    tracing::warn!("cannot store the post: {e:#}");
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("cannot read the job: {e:#}"),
        }
        drop(inner);
        self.tray_changed();
    }

    /// On start: the cuts shares reserved and never made (since 3.10).
    pub fn drop_pending_cuts(&self) {
        match self.db.delete_pending_cuts() {
            Ok(ids) if !ids.is_empty() => tracing::info!(
                "dropped {} cut(s) that were never made: {}",
                ids.len(),
                ids.join(", ")
            ),
            Ok(_) => {}
            Err(e) => tracing::warn!("cannot drop the cuts that were never made: {e:#}"),
        }
    }

    /// A cut whose job never made its file is no cut at all (since 3.0).
    pub fn drop_pending_cut(&self, id: &str) {
        match self.db.delete_pending_cut(id) {
            Ok(true) => {
                tracing::debug!("cut {id} dropped - its job left no file");
                self.tray_changed();
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("cannot drop cut {id}: {e:#}"),
        }
    }

    /// Record that a clip's H.264 preview exists (or is gone).
    pub fn set_preview_h264(&self, base: &str, url: Option<String>) {
        let mut inner = self.inner.lock();
        if let Some(c) = inner.clips.get_mut(base) {
            c.preview_h264 = url;
        }
        drop(inner);
        self.tray_changed();
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
        self.tray_changed();
        Ok(title)
    }

    /// What a delete has to touch, once it is sure no job of this clip runs.
    /// Since 3.0 a clip may be listed without its recording, so the path is
    /// optional; `cuts` are the ranges that would go with `scope=all`.
    pub fn take_clip_for_delete(&self, base: &str) -> Result<DeleteTarget, StateError> {
        let inner = self.inner.lock();
        let path = inner.clips.get(base).map(|c| PathBuf::from(&c.path));
        let known = path.is_some() || matches!(self.db.clip(base), Ok(Some(_)));
        if !known {
            return Err(StateError::UnknownClip(base.to_string()));
        }
        let busy = inner
            .current_job
            .iter()
            .chain(inner.queue.iter())
            .any(|id| inner.jobs.get(id).is_some_and(|j| j.base == base));
        if busy {
            return Err(StateError::ClipBusy);
        }
        drop(inner);
        Ok(DeleteTarget {
            path: path.filter(|p| p.is_file()),
            cuts: self.db.cuts_of(base).unwrap_or_default(),
        })
    }

    /// Forget a clip with everything that hangs off it (`scope=all`).
    pub fn forget_clip(&self, base: &str) {
        let mut inner = self.inner.lock();
        inner.clips.remove(base);
        self.events.send_modify(|v| *v = v.wrapping_add(1));
        if inner.names.remove(base).is_some() {
            self.save_names(&inner);
        }
        if inner.last.as_ref().is_some_and(|j| j.base == base) {
            inner.last = None;
        }
        drop(inner);
        if let Err(e) = self.db.delete_cuts_of(base) {
            tracing::warn!("cannot drop the cuts of {base}: {e:#}");
        }
        if let Err(e) = self.db.delete_clip(base) {
            tracing::warn!("cannot drop the clip {base}: {e:#}");
        }
    }

    /// The recording went to the recycle bin, the cuts stay (`scope=clip`,
    /// `after: recycle` and the cleanup rule, all since 3.0). The clip keeps
    /// its row - it is done, and its cuts are still there to render.
    pub fn recording_recycled(&self, base: &str) {
        self.recording_gone(base, false);
    }

    /// The recording is not where it should be and nobody asked for that
    /// (since 3.10.1): the clip is done like a recycled one, but it is
    /// marked `lost`, so finding the file again brings it back.
    pub fn recording_lost(&self, base: &str) {
        self.recording_gone(base, true);
    }

    fn recording_gone(&self, base: &str, lost: bool) {
        self.inner.lock().clips.remove(base);
        if let Err(e) = self.db.set_clip_file(base, false, lost) {
            tracing::warn!("cannot mark {base} as recycled: {e:#}");
        }
        if let Err(e) = self
            .db
            .set_clip_state(base, crate::db::CLIP_DONE, Some(&util::now_local()))
        {
            tracing::warn!("cannot mark {base} done: {e:#}");
        }
        self.tray_changed();
    }

    /// A recording the service had given up on is back (since 3.10.1, issue
    /// #40): the clip returns to the list exactly as it was. A clip somebody
    /// marked done, or whose recording was recycled on purpose, is left
    /// alone. Returns whether anything was brought back.
    pub fn recording_found(&self, base: &str) -> bool {
        let Ok(Some(row)) = self.db.clip(base) else {
            return false;
        };
        if !row.lost {
            return false;
        }
        if let Err(e) = self.db.set_clip_file(base, true, false) {
            tracing::warn!("cannot mark the recording of {base} as back: {e:#}");
            return false;
        }
        if row.state == crate::db::CLIP_DONE {
            let state = if self.has_cuts(base) {
                crate::db::CLIP_ACTIVE
            } else {
                crate::db::CLIP_NEW
            };
            if let Err(e) = self.db.set_clip_state(base, state, None) {
                tracing::warn!("cannot bring {base} back into the list: {e:#}");
            }
        }
        self.tray_changed();
        true
    }
}

/// A clip a delete is about to work on.
pub struct DeleteTarget {
    /// The recording, when it is still there.
    pub path: Option<PathBuf>,
    pub cuts: Vec<crate::db::Cut>,
}

impl AppState {
    /// Mutate a job in place (no-op when the id is unknown).
    pub fn with_job<F: FnOnce(&mut Job)>(&self, id: &str, f: F) {
        if let Some(job) = self.inner.lock().jobs.get_mut(id) {
            f(job);
        }
        self.tray_changed();
    }

    /// Register a new job: it runs at once when nothing runs, else it waits
    /// in the queue. Returns its position (0 = running). Keeps only the
    /// newest `MAX_JOBS` finished jobs.
    pub fn register_job(&self, inner: &mut Inner, mut job: Job) -> usize {
        let id = job.id.clone();
        let position = if inner.current_job.is_none() {
            inner.current_job = Some(id.clone());
            0
        } else {
            inner.queue.push_back(id.clone());
            inner.queue.len()
        };
        job.position = (position > 0).then_some(position);
        inner.jobs.insert(id.clone(), job);
        self.cancels.lock().insert(id, CancellationToken::new());
        self.tray_changed();
        if inner.jobs.len() > MAX_JOBS {
            let mut by_age: Vec<(String, String)> = inner
                .jobs
                .values()
                .map(|j| (j.at.clone(), j.id.clone()))
                .collect();
            by_age.sort();
            let keep: Vec<String> = inner
                .current_job
                .iter()
                .chain(inner.queue.iter())
                .cloned()
                .collect();
            let surplus = inner.jobs.len() - MAX_JOBS;
            for (_, old) in by_age
                .iter()
                .filter(|(_, id)| !keep.contains(id))
                .take(surplus)
            {
                inner.jobs.remove(old);
                self.cancels.lock().remove(old);
            }
        }
        position
    }

    /// The token of a job, for the pipeline and for `cancel_job`.
    pub fn cancel_token(&self, id: &str) -> CancellationToken {
        self.cancels.lock().get(id).cloned().unwrap_or_default()
    }

    fn renumber_queue(inner: &mut Inner) {
        let ids: Vec<String> = inner.queue.iter().cloned().collect();
        for (i, id) in ids.iter().enumerate() {
            if let Some(j) = inner.jobs.get_mut(id) {
                j.position = Some(i + 1);
            }
        }
    }

    /// Finish the running job: `done` goes to history and becomes `last`;
    /// a job whose token was cancelled ends as `cancelled`. Returns the next
    /// queued job, now the running one, for the caller to spawn.
    pub fn complete_job(&self, id: &str, result: Result<(), String>) -> Option<String> {
        let cancelled = self
            .cancels
            .lock()
            .get(id)
            .is_some_and(|t| t.is_cancelled());
        let shared = self.paths().shared_dir.clone();
        let mut inner = self.inner.lock();
        let job = inner.jobs.get_mut(id)?;
        match result {
            Ok(()) => {
                job.ok = Some(true);
                job.error = Some(String::new());
                job.stage = "done".into();
            }
            Err(_) if cancelled => {
                job.ok = Some(false);
                job.error = Some("cancelled".into());
                job.stage = "cancelled".into();
                job.cancelled = true;
            }
            Err(msg) => {
                job.ok = Some(false);
                // Since 3.10 a share whose file was made but not sent stays an
                // output: until 3.9 the file was left in shared\ with nothing
                // pointing at it, and "Publish to" could not reach it.
                if job.source.is_none()
                    && job.is_output()
                    && job
                        .file
                        .as_deref()
                        .is_some_and(|f| shared.join(f).is_file())
                {
                    job.upload_error = Some(msg.clone());
                }
                job.error = Some(msg);
                job.stage = "error".into();
            }
        }
        job.finished = Some(util::now_local());
        job.position = None;
        let job = job.clone();
        // the playable preview is housekeeping: no history entry, not the
        // "last share". A cut is no output either, but the page shows it.
        if !job.is_preview() {
            let kept = job.ok == Some(true) || job.cancelled || job.upload_error.is_some();
            if job.is_output() && kept {
                let entry = job.history_entry();
                self.record_job(&mut inner, entry);
            }
            inner.last = Some(job);
        }
        let mut next = None;
        if inner.current_job.as_deref() == Some(id) {
            inner.current_job = inner.queue.pop_front();
            Self::renumber_queue(&mut inner);
            if let Some(n) = inner.current_job.clone() {
                if let Some(j) = inner.jobs.get_mut(&n) {
                    j.position = None;
                }
                next = Some(n);
            }
        }
        drop(inner);
        self.tray_changed();
        next
    }

    /// `POST /api/jobs/<id>/cancel`: a waiting job leaves the queue at once
    /// (returns `true`); a running one gets its token cancelled and ends
    /// through `complete_job` (returns `false`). Past `upload` it is too late.
    pub fn cancel_job(&self, id: &str) -> Result<bool, StateError> {
        let mut inner = self.inner.lock();
        let Some(job) = inner.jobs.get(id).cloned() else {
            return Err(StateError::UnknownJob);
        };
        // A job that has just been taken off the queue - or was never on it,
        // because nothing ran - says `queued` until its pipeline names the
        // first stage. It is running: taking it "off the queue" marked it
        // cancelled while it went on and wrote its result anyway (seen on a
        // transcription that was cancelled in its first instant).
        let running = inner.current_job.as_deref() == Some(id);
        match job.stage.as_str() {
            "queued" if running => {
                drop(inner);
                if let Some(t) = self.cancels.lock().get(id) {
                    t.cancel();
                }
                tracing::info!("{} [{id}] cancel requested as it starts", job.kind);
                Ok(false)
            }
            "queued" => {
                inner.queue.retain(|q| q != id);
                Self::renumber_queue(&mut inner);
                if let Some(j) = inner.jobs.get_mut(id) {
                    j.ok = Some(false);
                    j.error = Some("cancelled".into());
                    j.stage = "cancelled".into();
                    j.cancelled = true;
                    j.position = None;
                    j.finished = Some(util::now_local());
                    // Since 3.10 a job that never ran is no history entry and
                    // not `last`: Activity listed it as "cancelled", and every
                    // open page showed its result card, for a click on
                    // "Remove from queue".
                }
                drop(inner);
                self.cancels.lock().remove(id);
                // the cut this share reserved stays only if it has a file
                if let Some(cut) = job.cut.as_deref() {
                    self.drop_pending_cut(cut);
                }
                self.tray_changed();
                tracing::info!("share [{id}] cancelled while queued");
                Ok(true)
            }
            // since 3.12 also while the speech is read: a transcription of
            // a long cut runs for minutes, and a rendering that reads first
            // spends most of its time there
            "encode" | "upload" | "transcribe" => {
                drop(inner);
                if let Some(t) = self.cancels.lock().get(id) {
                    t.cancel();
                }
                tracing::info!("{} [{id}] cancel requested", job.kind);
                Ok(false)
            }
            stage => Err(StateError::TooLate(stage.to_string())),
        }
    }

    /// Every stored job of a clip (since 3.0 the store answers, not the
    /// 200-entry cache: an old share's remote copy is still its own).
    fn jobs_of(&self, base: &str) -> Vec<Value> {
        self.db.jobs_of_base(base).unwrap_or_else(|e| {
            tracing::warn!("cannot read the jobs of {base}: {e:#}");
            Vec::new()
        })
    }

    /// Remote paths recorded in history for a clip.
    /// Entries from before 2.5 carry no target: they were Nextcloud uploads.
    pub fn history_paths_for(&self, base: &str) -> Vec<String> {
        self.jobs_of(base)
            .iter()
            .filter(|e| {
                e["target"]
                    .as_str()
                    .is_none_or(|t| t.is_empty() || t == "nextcloud")
            })
            .filter_map(|e| e["ncPath"].as_str().map(str::to_string))
            .collect()
    }

    /// Remote paths a clip's jobs recorded for one storage target (since 2.5).
    pub fn history_paths_for_target(&self, base: &str, target: &str) -> Vec<String> {
        self.jobs_of(base)
            .iter()
            .filter(|e| e["target"] == target)
            .filter_map(|e| e["ncPath"].as_str().map(str::to_string))
            .collect()
    }

    /// Remote paths one cut's outputs recorded for a storage target (since 3.0).
    pub fn cut_paths_for_target(&self, cut: &str, target: &str) -> Vec<String> {
        self.db
            .jobs_of_cut(cut)
            .unwrap_or_default()
            .iter()
            .filter(|e| e["target"] == target)
            .filter_map(|e| e["ncPath"].as_str().map(str::to_string))
            .collect()
    }

    /// Drop a clip's history entries (after its remote copies were deleted).
    pub fn remove_history_for(&self, base: &str) {
        let mut inner = self.inner.lock();
        inner.history.retain(|e| e["base"] != base);
        if let Err(e) = self.db.delete_jobs_for_base(base) {
            tracing::warn!("cannot drop the jobs of {base}: {e:#}");
        }
    }

    /// Drop one cut's outputs from the history (it is being deleted).
    pub fn remove_history_of_cut(&self, cut: &str) {
        let mut inner = self.inner.lock();
        inner.history.retain(|e| e["cut"] != cut);
        if let Err(e) = self.db.delete_jobs_of_cut(cut) {
            tracing::warn!("cannot drop the jobs of cut {cut}: {e:#}");
        }
    }
}

/// Move a clip's recording and its playable copies to the recycle bin and
/// mark the clip done (since 3.0). The thumbnail stays: the page still shows
/// the clip for its cuts. Returns how many files were moved.
pub async fn recycle_recording(state: &AppState, base: &str) -> usize {
    let paths = state.paths();
    let recording = state
        .inner
        .lock()
        .clips
        .get(base)
        .map(|c| PathBuf::from(&c.path));
    let files: Vec<PathBuf> = recording
        .into_iter()
        .chain([paths.preview_of(base), paths.preview_h264_of(base)])
        .filter(|f| f.is_file())
        .collect();
    let moved = tokio::task::spawn_blocking(move || {
        let mut n = 0;
        for f in files {
            match platform::recycle(&f) {
                Ok(()) => n += 1,
                Err(e) => tracing::warn!("cannot recycle {}: {e:#}", f.display()),
            }
        }
        n
    })
    .await
    .unwrap_or(0);
    state.recording_recycled(base);
    state.scan_wake.notify_one();
    tracing::info!("recording of {base} recycled ({moved} file(s)), its cuts stay");
    moved
}

/// Background task: the quota five seconds after start, then every 15
/// minutes or when `quota_wake` says so.
pub async fn quota_loop(state: Arc<AppState>) {
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    loop {
        state.refresh_quota().await;
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(15 * 60)) => {}
            _ = state.quota_wake.notified() => {
                // let a burst of changes settle
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one-off repair of issue #40: what a change of the clip folder put
    /// away before 3.10.1 comes back, and nothing else does.
    #[test]
    fn the_repair_brings_back_what_the_folder_change_put_away() {
        let dir = std::env::temp_dir().join(format!("rc-heal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".cuts")).unwrap();
        let db = Db::memory().unwrap();

        let clip = |base: &str, with_file: bool| {
            let path = dir.join(format!("{base}.mkv"));
            if with_file {
                std::fs::write(&path, b"x").unwrap();
            }
            let doc = json!({ "base": base, "name": format!("{base}.mkv"), "path": path });
            db.remember_clip(base, &doc, "2026-09-19T10:00:00", &dir)
                .unwrap();
        };
        let cut = |id: &str, base: &str, state: &str, with_file: bool| {
            if with_file {
                std::fs::write(dir.join(".cuts").join(cut_file_name(id)), b"x").unwrap();
            }
            db.put_cut(&crate::db::Cut {
                id: id.into(),
                base: base.into(),
                start: 1.0,
                end: 5.0,
                audio: "mix".into(),
                vertical: false,
                vertical_pos: None,
                file: Some(cut_file_name(id)),
                actual_start: None,
                created: "2026-09-19T10:01:00".into(),
                state: state.into(),
                title: String::new(),
                look: Default::default(),
            })
            .unwrap();
        };

        // the damage: the scan met the recording again, but the clip stayed
        // done and its cut missing although both files are there
        clip("hurt", true);
        cut("aaaa0001", "hurt", crate::db::CUT_MISSING, true);
        db.set_clip_state("hurt", crate::db::CLIP_DONE, Some("2026-09-19T11:00:00"))
            .unwrap();
        // the other shape: the store says the recording is gone, it is not
        clip("away", true);
        cut("aaaa0002", "away", crate::db::CUT_READY, true);
        db.set_clip_file("away", false, false).unwrap();
        db.set_clip_state("away", crate::db::CLIP_DONE, Some("2026-09-19T11:00:00"))
            .unwrap();
        // done by hand, everything where it belongs: not our business
        clip("byhand", true);
        cut("aaaa0003", "byhand", crate::db::CUT_READY, true);
        db.set_clip_state("byhand", crate::db::CLIP_DONE, Some("2026-09-19T11:00:00"))
            .unwrap();
        // really gone, with a cut that is really missing: stays done
        clip("gone", false);
        cut("aaaa0004", "gone", crate::db::CUT_MISSING, false);
        db.set_clip_file("gone", false, false).unwrap();
        db.set_clip_state("gone", crate::db::CLIP_DONE, Some("2026-09-19T11:00:00"))
            .unwrap();

        heal_after_a_folder_change(&db, &dir);

        for base in ["hurt", "away"] {
            let row = db.clip(base).unwrap().unwrap();
            assert_eq!(row.state, crate::db::CLIP_ACTIVE, "{base} is back");
            assert!(row.has_file, "{base} has its recording");
            assert!(!row.lost);
        }
        for base in ["byhand", "gone"] {
            assert_eq!(
                db.clip(base).unwrap().unwrap().state,
                crate::db::CLIP_DONE,
                "{base} was left alone"
            );
        }
        assert!(!db.clip("gone").unwrap().unwrap().has_file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_entry_reads_back_as_a_job() {
        let job = Job {
            id: "abc".into(),
            base: "Replay 1".into(),
            target: "nextcloud".into(),
            file: Some("Replay_1_0-5.mp4".into()),
            stage: "done".into(),
            ok: Some(true),
            ..Job::default()
        };
        let back: Job = serde_json::from_value(job.history_entry()).unwrap();
        assert_eq!(back.id, "abc");
        assert_eq!(back.file.as_deref(), Some("Replay_1_0-5.mp4"));
        assert_eq!(back.target, "nextcloud");
        assert!(!back.is_preview());
        assert!(back.stage.is_empty() && back.ok.is_none());
        // an entry from 2.4 without the fields of later versions
        let old: Job = serde_json::from_value(serde_json::json!({
            "id": "old1", "base": "Replay 2", "file": "Replay_2_0-5.mp4", "seconds": 5.0
        }))
        .unwrap();
        assert_eq!(old.mode, "h264");
        assert_eq!(old.target, "");
    }

    #[test]
    fn a_failed_upload_keeps_its_error_in_the_entry() {
        let job = Job {
            id: "up1".into(),
            stage: "error".into(),
            ok: Some(false),
            error: Some("upload: connection refused".into()),
            upload_error: Some("upload: connection refused".into()),
            file: Some("Replay_1_0-5.mp4".into()),
            target: "nextcloud".into(),
            ..Job::default()
        };
        let e = job.history_entry();
        assert_eq!(e["uploadError"], "upload: connection refused");
        assert_eq!(e["file"], "Replay_1_0-5.mp4");
        assert!(e.get("ok").is_none() && e.get("error").is_none());
        assert!(e.get("link").is_none());
        let back: Job = serde_json::from_value(e).unwrap();
        assert!(back.upload_error.is_some());
        // a finished share says nothing about it
        let fine = Job::default().history_entry();
        assert!(fine.get("uploadError").is_none());
    }

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
