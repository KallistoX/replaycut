//! Watches the clip folder. A change notification or the poll interval
//! triggers a scan; a scan applies the contract rules: a `*.mkv` becomes a
//! clip once it is at least 2 s old and can be opened exclusively, the
//! preview is remuxed, duration and audio tracks are probed.
//!
//! The folder can change at runtime (settings page): every round re-reads
//! the paths and re-creates the watcher when the folder differs.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use tokio::sync::mpsc;

use crate::state::{AppState, Clip};
use crate::toast::{self, Toast};
use crate::util;

const POLL: Duration = Duration::from_secs(5);
const MIN_AGE: Duration = Duration::from_secs(2);
const DEBOUNCE: Duration = Duration::from_millis(300);

fn watch(dir: &Path, tx: mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
    match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.try_send(());
        }
    }) {
        Ok(mut w) => match w.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => Some(w),
            Err(e) => {
                tracing::warn!("cannot watch {}: {e} - polling only", dir.display());
                None
            }
        },
        Err(e) => {
            tracing::warn!("file watcher unavailable: {e} - polling only");
            None
        }
    }
}

pub async fn run(state: Arc<AppState>) {
    let (tx, mut rx) = mpsc::channel::<()>(4);
    let mut watched = state.paths().clip_dir.clone();
    // Kept alive for its Drop; re-created when the folder changes.
    let mut _watcher = watch(&watched, tx.clone());

    loop {
        let paths = state.paths();
        if paths.clip_dir != watched {
            watched = paths.clip_dir.clone();
            _watcher = watch(&watched, tx.clone());
        }
        let retry = match scan(&state).await {
            Ok(retry) => retry,
            Err(e) => {
                tracing::error!("scan: {e:#}");
                None
            }
        };
        let wait = retry.map_or(POLL, |d| d.min(POLL));
        tokio::select! {
            _ = rx.recv() => { tokio::time::sleep(DEBOUNCE).await; while rx.try_recv().is_ok() {} }
            _ = tokio::time::sleep(wait) => {}
            _ = state.scan_wake.notified() => {}
        }
    }
}

/// One scan. Returns how soon a re-scan is worthwhile when a file was
/// skipped for being too young or still open.
async fn scan(state: &AppState) -> Result<Option<Duration>> {
    let paths = state.paths();
    let runtime = state.runtime();
    let mut files: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    for entry in std::fs::read_dir(&paths.clip_dir)?.flatten() {
        let path = entry.path();
        if !path.is_file()
            || path
                .extension()
                .and_then(|e| e.to_str())
                .is_none_or(|e| !e.eq_ignore_ascii_case("mkv"))
        {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            files.push((
                path,
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            ));
        }
    }
    files.sort_by_key(|(_, mtime, _)| *mtime);

    let known: Vec<String> = state.inner.lock().clips.keys().cloned().collect();
    let mut retry: Option<Duration> = None;
    let mut seen_dirty = false;

    for (path, mtime, size) in &files {
        let Some(base) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if known.contains(&base) {
            continue;
        }
        let age = SystemTime::now().duration_since(*mtime).unwrap_or_default();
        if age < MIN_AGE {
            retry = Some(
                retry.map_or(MIN_AGE - age, |r| r.min(MIN_AGE - age)) + Duration::from_millis(100),
            );
            continue;
        }
        if !file_ready(path) {
            retry = Some(retry.map_or(Duration::from_secs(1), |r| r.min(Duration::from_secs(1))));
            continue;
        }
        let preview = paths.preview_of(&base);
        let result: Result<()> = async {
            if !preview.is_file() {
                runtime.media.remux_preview(path, &preview).await?;
            }
            let duration = runtime.media.duration(&preview).await?;
            let tracks = runtime.media.audio_tracks(path).await;
            let video = runtime.media.video_info(path).await;
            let clip = Clip {
                name: path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
                base: base.clone(),
                path: path.to_string_lossy().into_owned(),
                size: *size,
                duration,
                tracks,
                created: util::system_time_local(*mtime),
                preview: format!("/media/{}.mp4", util::encode_path_segment(&base)),
                status: "ready",
                codec: video.codec,
                width: video.width,
                height: video.height,
                fps: video.fps,
            };
            tracing::info!(
                "new clip: {} ({:.1} MB, {duration} s, {tracks} audio tracks) - preview ready",
                clip.name,
                *size as f64 / 1_048_576.0
            );
            let (new_to_seen, ready) = {
                let mut inner = state.inner.lock();
                inner.clips.insert(base.clone(), clip.clone());
                (inner.seen.insert(base.clone()), inner.seen_ready)
            };
            if new_to_seen {
                seen_dirty = true;
                // Announced exactly once per clip; the first scan after a
                // fresh start only records what is already there.
                if ready {
                    toast::show(state, Toast::clip_saved(&clip, &state.ui_url()));
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::error!("preview for {} failed: {e:#}", path.display());
        }
    }

    // Clips whose MKV disappeared, orphaned previews, stale seen entries.
    let existing: Vec<String> = files
        .iter()
        .filter_map(|(p, _, _)| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    {
        let mut inner = state.inner.lock();
        inner.clips.retain(|base, _| existing.contains(base));
        let before = inner.seen.len();
        inner.seen.retain(|base| existing.contains(base));
        if inner.seen.len() != before {
            seen_dirty = true;
        }
        if seen_dirty {
            state.save_seen(&inner);
        }
        inner.seen_ready = true;
        inner.scan_at = Some(util::now_local());
    }
    state.tray_changed();
    if let Ok(previews) = std::fs::read_dir(&paths.preview_dir) {
        for p in previews.flatten() {
            let path = p.path();
            let is_mp4 = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
            let orphan = path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_none_or(|b| !existing.iter().any(|e| e == b));
            if is_mp4 && orphan {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    Ok(retry)
}

/// OBS still writing means the exclusive open fails.
#[cfg(windows)]
fn file_ready(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(path)
        .is_ok()
}

#[cfg(not(windows))]
fn file_ready(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}
