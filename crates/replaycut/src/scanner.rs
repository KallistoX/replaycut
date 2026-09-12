//! Watches the clip folder. A change notification or the poll interval
//! triggers a scan; a scan applies the contract rules: a `*.mkv` becomes a
//! clip once it is at least 2 s old and can be opened exclusively, the
//! preview is remuxed, duration and audio tracks are probed.
//!
//! The folder can change at runtime (settings page): every round re-reads
//! the paths and re-creates the watcher when the folder differs.

use std::collections::BTreeMap;
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
            // since 3.0: the store keeps what we know, so the clip stays
            // listed for its cuts once the recording is gone
            if let Err(e) = state.db.remember_clip(
                &base,
                &serde_json::to_value(&clip).unwrap_or_default(),
                &clip.created,
            ) {
                tracing::warn!("cannot remember {base}: {e:#}");
            }
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
                    Ok(s) if s.position == 0 => {
                        tokio::spawn(crate::share::run(state.clone(), s.job));
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
        let gone: Vec<String> = inner
            .clips
            .keys()
            .filter(|b| !existing.contains(b))
            .cloned()
            .collect();
        inner.clips.retain(|base, _| existing.contains(base));
        if !gone.is_empty() {
            changed = true;
        }
        drop(inner);
        // A recording that disappeared from outside: with cuts the clip lives
        // on as done, without them it is forgotten as before (since 3.0).
        for base in gone {
            if state.has_cuts(&base) {
                tracing::info!("{base} is gone from the folder - its cuts stay");
                state.recording_recycled(&base);
            } else {
                state.forget_clip(&base);
            }
        }
        // The same may have happened while the service was not running. This
        // run never knew those clips, so the loop above cannot reach them and
        // the store still says they have their recording - which is what the
        // clips page needs to be false before it lists a clip for its cuts
        // alone. Without this they and their cuts fell off the page on the
        // next start (since 3.8). Rows without cuts are left as they are:
        // nothing lists them either way, and forgetting reaches into the
        // store on the strength of one folder listing.
        for base in stale_rows(&state.db.clips().unwrap_or_default(), &existing) {
            if state.has_cuts(&base) {
                tracing::info!("{base} went before this start - its cuts stay");
                state.recording_recycled(&base);
            }
        }
        let mut inner = state.inner.lock();
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
    // Clips the store still lists although their recording is gone (since
    // 3.0): their thumbnail is what the page shows for them, so it stays.
    let listed: Vec<String> = state
        .db
        .clips()
        .unwrap_or_default()
        .into_values()
        .filter(|r| !r.has_file)
        .map(|r| r.base)
        .collect();
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
                .is_none_or(|b| !existing.iter().any(|e| e == b) && !listed.iter().any(|e| e == b));
            if is_mp4 && orphan {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    sweep_cuts(state).await;
    recycle_done(state).await;
    Ok(retry)
}

/// The cut files against the store (since 3.0): a file nobody knows about
/// goes to the recycle bin, a cut whose file is gone becomes `missing`.
async fn sweep_cuts(state: &Arc<AppState>) {
    let cuts = match state.db.cuts() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("cannot read the cuts: {e:#}");
            return;
        }
    };
    let paths = state.paths();
    for cut in &cuts {
        if cut.state == crate::db::CUT_READY && !paths.cut_of(&cut.id).is_file() {
            tracing::warn!("the file of cut {} is gone", cut.id);
            if let Err(e) = state.db.set_cut_state(&cut.id, crate::db::CUT_MISSING) {
                tracing::warn!("cannot mark cut {} as missing: {e:#}", cut.id);
            }
            state.tray_changed();
        }
    }
    let Ok(entries) = std::fs::read_dir(&paths.cuts_dir) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        let known = path
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|id| cuts.iter().any(|c| c.id == id));
        if known || !path.is_file() {
            continue;
        }
        tracing::info!("cut file {} belongs to no cut - recycled", path.display());
        let _ = tokio::task::spawn_blocking(move || crate::platform::recycle(&path)).await;
    }
}

/// `cleanup.recycleDoneAfterDays`: the recordings of clips that have been
/// done for that long go to the recycle bin. Cuts and outputs stay.
async fn recycle_done(state: &Arc<AppState>) {
    let days = state.settings().cleanup.recycle_done_after_days;
    if days == 0 {
        return;
    }
    let cutoff = (chrono::Local::now() - chrono::Duration::days(days as i64))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let bases = match state.db.done_before(&cutoff) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("cannot look for clips to tidy up: {e:#}");
            return;
        }
    };
    for base in bases {
        tracing::info!("{base} has been done for {days} day(s) - recycling the recording");
        crate::state::recycle_recording(state, &base).await;
    }
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

/// Unix has no exclusive open that a writer would block, so this only says
/// the file can be read at all; the age rule (`MIN_AGE` since the last
/// write) is what keeps a replay OBS is still flushing out of the list, and
/// the `ReplayBufferSaved` event from obs-websocket is the definite word.
#[cfg(not(windows))]
fn file_ready(path: &Path) -> bool {
    std::fs::File::open(path).is_ok()
}

/// Clip rows whose recording the store still believes in although the folder
/// listing does not carry it. The scan reconciles them so that a clip whose
/// recording went while the service was down is listed for its cuts again.
fn stale_rows(rows: &BTreeMap<String, crate::db::ClipRow>, existing: &[String]) -> Vec<String> {
    rows.values()
        .filter(|r| r.has_file && !existing.iter().any(|e| e == &r.base))
        .map(|r| r.base.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::stale_rows;
    use std::collections::BTreeMap;

    fn row(base: &str, has_file: bool) -> crate::db::ClipRow {
        crate::db::ClipRow {
            base: base.to_string(),
            state: "active".into(),
            done_at: None,
            first_seen: None,
            has_file,
            doc: None,
        }
    }

    #[test]
    fn a_row_whose_recording_is_not_in_the_folder_is_stale() {
        let rows: BTreeMap<String, crate::db::ClipRow> = ["here", "gone", "known-gone"]
            .iter()
            .map(|b| (b.to_string(), row(b, *b != "known-gone")))
            .collect();
        let existing = vec!["here".to_string()];
        assert_eq!(stale_rows(&rows, &existing), vec!["gone".to_string()]);
        // nothing is stale while every recording is there
        let all = vec!["here".to_string(), "gone".to_string()];
        assert!(stale_rows(&rows, &all).is_empty());
    }
}
