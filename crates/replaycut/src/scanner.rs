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
        let retry = if state.scanning_paused() {
            None
        } else {
            match scan(&state).await {
                Ok(retry) => retry,
                Err(e) => {
                    tracing::error!("scan: {e:#}");
                    None
                }
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

/// Where the thumbnail is taken: the moment that made someone press F9 is
/// near the end of the buffer.
fn thumb_at(duration: f64) -> f64 {
    if duration >= 15.0 {
        duration - 10.0
    } else {
        (duration / 2.0).max(0.0)
    }
}

/// Clips whose thumbnail failed once; not retried until the next start.
fn thumb_failed() -> &'static parking_lot::Mutex<std::collections::HashSet<String>> {
    static FAILED: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

/// One scan. Returns how soon a re-scan is worthwhile when a file was
/// skipped for being too young or still open.
async fn scan(state: &Arc<AppState>) -> Result<Option<Duration>> {
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
    // only a changed clip set wakes the tray and the event streams
    let mut changed = false;

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
            let thumb = paths.thumb_of(&base);
            if !thumb.is_file() {
                if let Err(e) = runtime
                    .media
                    .thumbnail(&preview, &thumb, thumb_at(duration))
                    .await
                {
                    tracing::warn!("thumbnail for {base}: {e:#}");
                }
            }
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
                thumb: thumb
                    .is_file()
                    .then(|| format!("/media/{}.jpg", util::encode_path_segment(&base))),
                preview_h264: paths
                    .preview_h264_of(&base)
                    .is_file()
                    .then(|| crate::state::preview_h264_url(&base)),
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
            let wants_h264 = clip.preview_h264.is_none()
                && state.settings().preview_h264 == "always"
                && !state.dry_run;
            let (new_to_seen, ready) = {
                let mut inner = state.inner.lock();
                inner.clips.insert(base.clone(), clip.clone());
                changed = true;
                (inner.seen.insert(base.clone()), inner.seen_ready)
            };
            // `previewH264: always` (since 2.6): the playable copy right away,
            // behind the running jobs, with idle priority
            if wants_h264 {
                match crate::share::start_preview(state, &base, true) {
                    Ok((id, 0)) => {
                        tokio::spawn(crate::share::run(state.clone(), id));
                    }
                    Ok(_) => {}
                    Err(e) => tracing::debug!("preview for {base} not queued: {e:?}"),
                }
            }
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

    // Clips from before thumbnails existed (or whose thumbnail failed once):
    // one per pass, a second apart, so the first scan after the update does
    // not hog ffmpeg.
    let candidate = {
        let failed = thumb_failed().lock();
        let inner = state.inner.lock();
        inner
            .clips
            .values()
            .find(|c| c.thumb.is_none() && !failed.contains(&c.base))
            .map(|c| (c.base.clone(), c.duration))
    };
    if let Some((base, duration)) = candidate {
        let thumb = paths.thumb_of(&base);
        match runtime
            .media
            .thumbnail(&paths.preview_of(&base), &thumb, thumb_at(duration))
            .await
        {
            Ok(()) => {
                if let Some(c) = state.inner.lock().clips.get_mut(&base) {
                    c.thumb = Some(format!("/media/{}.jpg", util::encode_path_segment(&base)));
                    changed = true;
                }
            }
            Err(e) => {
                tracing::warn!("thumbnail for {base}: {e:#}");
                thumb_failed().lock().insert(base);
            }
        }
        let soon = Duration::from_secs(1);
        retry = Some(retry.map_or(soon, |r| r.min(soon)));
    }

    // Clips whose MKV disappeared, orphaned previews and thumbnails, stale seen entries.
    let existing: Vec<String> = files
        .iter()
        .filter_map(|(p, _, _)| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    {
        let mut inner = state.inner.lock();
        let before = inner.clips.len();
        inner.clips.retain(|base, _| existing.contains(base));
        if inner.clips.len() != before {
            changed = true;
        }
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
    if changed {
        state.tray_changed();
    }
    if let Ok(previews) = std::fs::read_dir(&paths.preview_dir) {
        for p in previews.flatten() {
            let path = p.path();
            let is_mp4 = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("mp4") || e.eq_ignore_ascii_case("jpg"));
            // `<base>.h264.mp4` (since 2.6) belongs to `<base>` as well
            let orphan = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| {
                    s.strip_suffix(".h264.part")
                        .or_else(|| s.strip_suffix(".h264"))
                        .unwrap_or(s)
                })
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
