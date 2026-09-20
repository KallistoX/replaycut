//! The share pipeline: `queued` -> `encode` -> `upload` -> `discord` ->
//! `done`, `error` or (since 2.4) `cancelled`, exactly as `docs/api.md`
//! describes it. Stages whose integration is disabled are skipped. One job
//! runs at a time; the others wait in the queue and start as the running
//! one ends.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::db::{Cut, CUT_PENDING, CUT_READY};
use crate::integrations::random_token;
use crate::platform;
use crate::state::{
    cut_file_name, AppState, Job, AUDIO_MODES, KIND_CUT, KIND_PUBLISH, KIND_RENDER,
    KIND_TRANSCRIBE, MAX_QUEUE,
};
use crate::toast::{self, Toast};
use crate::util;

const ENCODE_TIMEOUT: Duration = Duration::from_secs(900);

pub struct ShareRequest {
    pub base: String,
    pub start: f64,
    pub end: f64,
    pub audio: String,
    /// `h264` (default) or `copy` (since 2.4).
    pub mode: String,
    /// A storage id, `file`, or empty for the default (since 2.5).
    pub target: String,
    /// A 9:16 cut for Shorts (since 2.6); `vertical_pos` 0..1 is where the
    /// window sits, 0.5 = centre.
    pub vertical: bool,
    pub vertical_pos: f64,
    /// What happens to the clip afterwards (since 3.0): `keep`, `done` or
    /// `recycle`; empty takes `cleanup.afterShare` from the settings.
    pub after: String,
    /// What this rendering does with the subtitles of its cut (since 3.11):
    /// `none` (the default), `burn` or `track`.
    pub subtitles: String,
}

/// The "Afterwards" of a job: what the body said, or the settings default.
fn after_of(state: &AppState, wanted: &str) -> Result<String, ShareError> {
    if wanted.is_empty() {
        return Ok(state.settings().cleanup.after_share);
    }
    if !crate::settings::AFTER_VALUES.contains(&wanted) {
        return Err(ShareError::Invalid(format!(
            "unknown after: {wanted} (keep, done or recycle)"
        )));
    }
    Ok(wanted.to_string())
}

pub const SHARE_MODES: [&str; 2] = ["h264", "copy"];

/// The `-vf` chain of a vertical cut: a 9:16 window of full height at
/// `pos` (0 = left edge, 1 = right edge), scaled to 1080x1920.
pub fn vertical_filter(pos: f64) -> String {
    let pos = pos.clamp(0.0, 1.0);
    format!("crop=ih*9/16:ih:(iw-ih*9/16)*{pos:.3}:0,scale=1080:1920")
}

#[derive(Debug)]
pub enum ShareError {
    UnknownClip(String),
    /// `publish` of a job id nobody knows.
    UnknownJob(String),
    /// `render` of a cut id nobody knows (since 3.0).
    UnknownCut(String),
    /// The same share is already running or waiting; carries its id.
    Busy(String),
    /// This range is already a cut of this clip; carries the cut id (since 3.0).
    CutExists(String),
    Invalid(String),
    /// Something this service cannot do right now, with the one word that
    /// says which (since 3.11): `disabled`, `filter` or `model`.
    Unmet(&'static str, String),
    /// `MAX_QUEUE` jobs are waiting already.
    QueueFull,
}

/// ffmpeg audio mapping per mode (tracks are 0-based: a:0 mix, a:1 mic, a:2 game, a:3 voice chat).
pub fn audio_args(mode: &str) -> Option<&'static [&'static str]> {
    Some(match mode {
        "mix" => &["-map", "0:a:0"],
        "gamemic" => &[
            "-filter_complex",
            "[0:a:2][0:a:1]amix=inputs=2:normalize=0[a]",
            "-map",
            "[a]",
        ],
        "game" => &["-map", "0:a:2"],
        "gamediscord" => &[
            "-filter_complex",
            "[0:a:2][0:a:3]amix=inputs=2:normalize=0[a]",
            "-map",
            "[a]",
        ],
        _ => return None,
    })
}

/// Title -> file name slug: runs of characters other than word characters
/// and `-` become one `-`, trimmed, at most 40 characters.
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for c in title.chars() {
        if c.is_alphanumeric() || c == '_' || c == '-' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(c);
        } else {
            pending_dash = true;
        }
    }
    let trimmed: String = out.trim_matches('-').chars().take(40).collect();
    trimmed.trim_matches('-').to_string()
}

/// `YYYY-MM` from the first `YYYY-MM-DD` in the base name, else `unsorted`.
pub fn month_of(base: &str) -> String {
    let b = base.as_bytes();
    if b.len() >= 10 {
        for i in 0..=b.len() - 10 {
            let w = &b[i..i + 10];
            let digits = [0, 1, 2, 3, 5, 6, 8, 9]
                .iter()
                .all(|&k| w[k].is_ascii_digit());
            if digits && w[4] == b'-' && w[7] == b'-' {
                return String::from_utf8_lossy(&w[..7]).into_owned();
            }
        }
    }
    "unsorted".to_string()
}

pub fn share_file_name(base: &str, start: f64, end: f64, slug: &str) -> String {
    let mut name = base.split_whitespace().collect::<Vec<_>>().join("_");
    name.push_str(&format!("_{}-{}", start.round() as i64, end.round() as i64));
    if !slug.is_empty() {
        name.push('_');
        name.push_str(slug);
    }
    name.push_str(".mp4");
    name
}

/// A vertical cut of the same range gets its own file (since 2.6).
pub fn vertical_file_name(name: &str) -> String {
    match name.strip_suffix(".mp4") {
        Some(stem) => format!("{stem}_9x16.mp4"),
        None => format!("{name}_9x16"),
    }
}

/// The nth name in a row: `X.mp4`, `X_2.mp4`, `X_3.mp4` (since 3.8).
fn numbered_file_name(name: &str, n: u32) -> String {
    match name.strip_suffix(".mp4") {
        Some(stem) => format!("{stem}_{n}.mp4"),
        None => format!("{name}_{n}"),
    }
}

/// A name no other output carries (since 3.8). Two renderings of the same
/// range are one file name, so the second used to overwrite the first - here
/// and on the storage - while the first stayed in the history with the size
/// of a file that was no longer its own.
fn free_file_name(state: &AppState, id: &str, wanted: &str) -> String {
    let taken = |name: &str| {
        let running = state
            .inner
            .lock()
            .jobs
            .values()
            .any(|j| j.id != id && j.file.as_deref() == Some(name));
        running
            || state.db.file_taken_by_other(name, id).unwrap_or_else(|e| {
                tracing::warn!("cannot look up the file name {name}: {e:#}");
                false
            })
    };
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..=MAX_FILE_NAMES)
        .map(|n| numbered_file_name(wanted, n))
        .find(|c| !taken(c))
        .unwrap_or_else(|| numbered_file_name(wanted, MAX_FILE_NAMES))
}

/// How far the counter of `free_file_name` counts before it gives up and
/// writes over the last one. A range rendered a thousand times is a bug
/// somewhere else.
const MAX_FILE_NAMES: u32 = 1000;

/// Text of the post: `[<title> - ]<base without the "<prefix> " part>`.
/// The prefix is only removed when it is a whole word at the start.
pub fn post_label(prefix: &str, base: &str, title: &str) -> String {
    let label = match base.strip_prefix(prefix) {
        Some(rest) if !prefix.is_empty() && rest.starts_with(char::is_whitespace) => {
            rest.trim_start()
        }
        _ => base,
    };
    if title.is_empty() {
        label.to_string()
    } else {
        format!("{title} - {label}")
    }
}

/// A registered job: its id, its place in the queue (0 = runs at once, the
/// caller spawns `run` for it) and the cut it works on (since 3.0).
pub struct Started {
    pub job: String,
    pub position: usize,
    pub cut: Option<String>,
}

/// The cut of this job's range: the one that is already there, whatever
/// state it is in, or a new pending row. The file itself is made by the
/// `cut` stage of the job (since 3.0).
fn cut_for(state: &AppState, job: &Job) -> Result<Cut, ShareError> {
    let existing = state
        .db
        .cut_of_range(&job.base, job.start, job.end)
        .map_err(|e| ShareError::Invalid(format!("cannot read the cuts: {e:#}")))?;
    if let Some(cut) = existing {
        return Ok(cut);
    }
    let cut = Cut {
        id: crate::auth::random_hex(4),
        base: job.base.clone(),
        start: job.start,
        end: job.end,
        audio: job.audio.clone(),
        vertical: job.vertical,
        vertical_pos: job.vertical_pos,
        file: None,
        actual_start: None,
        created: util::now_local(),
        state: CUT_PENDING.to_string(),
    };
    state
        .db
        .put_cut(&cut)
        .map_err(|e| ShareError::Invalid(format!("cannot store the cut: {e:#}")))?;
    Ok(cut)
}

/// Validate and register a job. Holds the state lock for the whole check so
/// two concurrent requests cannot both pass the busy check.
/// Validate and register a share.
pub fn start(state: &AppState, req: ShareRequest) -> Result<Started, ShareError> {
    let mut inner = state.inner.lock();
    let clip = inner
        .clips
        .get(&req.base)
        .ok_or_else(|| ShareError::UnknownClip(req.base.clone()))?;
    let start = req.start.max(0.0);
    let end = req.end.min(clip.duration);
    let seconds = ((end - start) * 100.0).round() / 100.0;
    if seconds < 1.0 {
        return Err(ShareError::Invalid(format!(
            "selection too short ({seconds} s)"
        )));
    }
    let audio = if req.audio.is_empty() {
        "mix".to_string()
    } else {
        req.audio
    };
    let mode = AUDIO_MODES
        .iter()
        .find(|m| m.id == audio)
        .ok_or_else(|| ShareError::Invalid(format!("unknown audio mode: {audio}")))?;
    if clip.tracks < mode.need {
        return Err(ShareError::Invalid(format!(
            "clip has only {} audio track(s) - '{}' needs {}",
            clip.tracks, mode.label, mode.need
        )));
    }
    let share_mode = if req.mode.is_empty() {
        "h264".to_string()
    } else if SHARE_MODES.contains(&req.mode.as_str()) {
        req.mode
    } else {
        return Err(ShareError::Invalid(format!(
            "unknown mode: {} (h264 or copy)",
            req.mode
        )));
    };
    if req.vertical && share_mode == "copy" {
        return Err(ShareError::Invalid(
            "a vertical cut needs the h264 mode (copy keeps the frame as recorded)".to_string(),
        ));
    }
    if req.vertical && !req.vertical_pos.is_finite() {
        return Err(ShareError::Invalid(
            "verticalPos must be a number between 0 and 1".to_string(),
        ));
    }
    let vertical_pos = req
        .vertical
        .then(|| (req.vertical_pos.clamp(0.0, 1.0) * 1000.0).round() / 1000.0);
    // the same cut twice (a double click) attaches to the first one
    let duplicate = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| {
            j.base == clip.base
                && (j.start - start).abs() < 0.005
                && (j.end - end).abs() < 0.005
                && j.audio == audio
                && j.mode == share_mode
                && j.vertical == req.vertical
                && j.source.is_none()
                && !j.is_preview()
        })
        .map(|j| j.id.clone());
    if let Some(id) = duplicate {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let target = state
        .runtime()
        .integrations
        .resolve_target(&req.target)
        .ok_or_else(|| {
            ShareError::Invalid(format!(
                "unknown or unconfigured target: {} (a storage id or 'file')",
                req.target
            ))
        })?;
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    // since 2.7: best quality unless the target has limits
    let limits = state.settings().limits(&target);
    let after = after_of(state, &req.after)?;
    // Since 3.11. A share cuts its range on the way, so the transcript this
    // will use is the one of the cut that range already is - if it is one.
    let existing = state
        .db
        .cut_of_range(&clip.base, start, end)
        .ok()
        .flatten()
        .and_then(|c| state.db.subtitles(&c.id).ok().flatten());
    let subtitles = subtitles_for(
        state,
        Some(req.subtitles.as_str()),
        existing.as_ref().map(|s| s.mode.as_str()),
        existing.as_ref().is_some_and(|s| !s.segments.is_empty()),
        share_mode == "copy",
    )?;
    let mut job = Job {
        id: id.clone(),
        subtitles,
        base: clip.base.clone(),
        target,
        start,
        end,
        seconds,
        audio,
        mode: share_mode,
        kbps: limits.max_kbps,
        max_height: limits.max_height,
        codec: clip.codec.clone(),
        source_kbps: if clip.duration > 0.0 {
            (clip.size as f64 * 8.0 / clip.duration / 1000.0).round() as u32
        } else {
            0
        },
        vertical: req.vertical,
        vertical_pos,
        after,
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    // since 3.0 every share goes through a cut; the page has its id at once
    let cut = cut_for(state, &job)?;
    job.cut = Some(cut.id.clone());
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut: Some(cut.id),
    })
}

/// `POST /api/cuts` (since 3.0): save a range without rendering anything.
/// The job has the stages `queued -> cut -> done`.
pub fn start_cut(state: &AppState, req: ShareRequest) -> Result<Started, ShareError> {
    let mut inner = state.inner.lock();
    let clip = inner
        .clips
        .get(&req.base)
        .ok_or_else(|| ShareError::UnknownClip(req.base.clone()))?;
    let start = req.start.max(0.0);
    let end = req.end.min(clip.duration);
    let seconds = ((end - start) * 100.0).round() / 100.0;
    if seconds < 1.0 {
        return Err(ShareError::Invalid(format!(
            "selection too short ({seconds} s)"
        )));
    }
    let audio = if req.audio.is_empty() {
        "mix".to_string()
    } else {
        req.audio
    };
    let mode = AUDIO_MODES
        .iter()
        .find(|m| m.id == audio)
        .ok_or_else(|| ShareError::Invalid(format!("unknown audio mode: {audio}")))?;
    if clip.tracks < mode.need {
        return Err(ShareError::Invalid(format!(
            "clip has only {} audio track(s) - '{}' needs {}",
            clip.tracks, mode.label, mode.need
        )));
    }
    // the same range twice: the cut that is there answers, no second file
    if let Some(cut) = state
        .db
        .cut_of_range(&req.base, start, end)
        .map_err(|e| ShareError::Invalid(format!("cannot read the cuts: {e:#}")))?
    {
        if cut.state == CUT_READY && state.paths_of_cut(&cut).cut_of(&cut.id).is_file() {
            return Err(ShareError::CutExists(cut.id));
        }
    }
    let duplicate = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| {
            j.kind == KIND_CUT
                && j.base == req.base
                && (j.start - start).abs() < 0.005
                && (j.end - end).abs() < 0.005
        })
        .map(|j| j.id.clone());
    if let Some(id) = duplicate {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    let mut job = Job {
        id: id.clone(),
        kind: KIND_CUT.to_string(),
        base: clip.base.clone(),
        start,
        end,
        seconds,
        audio,
        vertical: req.vertical,
        vertical_pos: req
            .vertical
            .then(|| (req.vertical_pos.clamp(0.0, 1.0) * 1000.0).round() / 1000.0),
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    let cut = cut_for(state, &job)?;
    job.cut = Some(cut.id.clone());
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut: Some(cut.id),
    })
}

/// What `POST /api/cuts/<id>/render` may say; everything it leaves out comes
/// from the cut (since 3.0).
#[derive(Default)]
pub struct RenderRequest {
    pub target: String,
    pub mode: String,
    pub audio: Option<String>,
    pub vertical: Option<bool>,
    pub vertical_pos: Option<f64>,
    /// What happens to the clip afterwards, as in `POST /api/share`.
    pub after: String,
    /// What this rendering does with the subtitles of the cut (since 3.11):
    /// `none`, `burn` or `track`. Empty takes what the cut remembers.
    pub subtitles: Option<String>,
}

/// What a rendering may do about subtitles. `cut` is the transcript the cut
/// has, if any; `None` means the rendering would have to read the speech
/// first, which it can only do when everything is in place for it.
pub fn subtitles_for(
    state: &AppState,
    wanted: Option<&str>,
    remembered: Option<&str>,
    has_subtitles: bool,
    copy_mode: bool,
) -> Result<String, ShareError> {
    let mode = match wanted {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => remembered.unwrap_or(crate::db::SUBS_NONE).to_string(),
    };
    if mode == crate::db::SUBS_NONE {
        return Ok(mode);
    }
    if !crate::db::SUBS_MODES.contains(&mode.as_str()) {
        return Err(ShareError::Invalid(format!(
            "unknown subtitles: {mode} (none, burn or track)"
        )));
    }
    let settings = state.settings();
    if !settings.subtitles.enabled {
        return Err(ShareError::Unmet(
            "disabled",
            "subtitles are switched off - turn them on under Settings › Subtitles".into(),
        ));
    }
    if mode == crate::db::SUBS_BURN && copy_mode {
        return Err(ShareError::Invalid(
            "burned-in subtitles need the h264 mode - 'As recorded' copies the picture untouched, \
             so nothing can be drawn into it. Take the track instead."
                .into(),
        ));
    }
    if !has_subtitles {
        // the rendering would read the speech first; it may only promise
        // that when it can actually do it
        if !state.runtime().whisper {
            return Err(ShareError::Unmet(
                "filter",
                "subtitles: not in this ffmpeg build - it was built without the whisper filter"
                    .into(),
            ));
        }
        let model = crate::subtitles::model(&settings.subtitles.model);
        if model.is_none_or(|m| crate::subtitles::ready(&state.data_dir, m).is_none()) {
            return Err(ShareError::Unmet(
                "model",
                format!(
                    "this cut has no subtitles yet and the model {} is not on this PC",
                    settings.subtitles.model
                ),
            ));
        }
    }
    Ok(mode)
}

/// `POST /api/cuts/<id>/render` (since 3.0): encode a cut that exists and
/// send it on. Stages `queued -> encode -> upload -> notify -> done`.
pub fn start_render(
    state: &AppState,
    cut_id: &str,
    req: RenderRequest,
) -> Result<Started, ShareError> {
    let cut = state
        .db
        .cut(cut_id)
        .map_err(|e| ShareError::Invalid(format!("cannot read the cut: {e:#}")))?
        .ok_or_else(|| ShareError::UnknownCut(cut_id.to_string()))?;
    if !state.paths_of_cut(&cut).cut_of(&cut.id).is_file() {
        return Err(ShareError::Invalid(format!(
            "the file of cut {} is gone - cut the range again",
            cut.id
        )));
    }
    let mut inner = state.inner.lock();
    let audio = req.audio.unwrap_or_else(|| cut.audio.clone());
    let mode = AUDIO_MODES
        .iter()
        .find(|m| m.id == audio)
        .ok_or_else(|| ShareError::Invalid(format!("unknown audio mode: {audio}")))?;
    // the cut carries every track of the recording, so the clip decides what
    // the audio modes can do - and it may be gone by now
    if let Some(clip) = inner.clips.get(&cut.base) {
        if clip.tracks < mode.need {
            return Err(ShareError::Invalid(format!(
                "clip has only {} audio track(s) - '{}' needs {}",
                clip.tracks, mode.label, mode.need
            )));
        }
    }
    let share_mode = if req.mode.is_empty() {
        "h264".to_string()
    } else if SHARE_MODES.contains(&req.mode.as_str()) {
        req.mode
    } else {
        return Err(ShareError::Invalid(format!(
            "unknown mode: {} (h264 or copy)",
            req.mode
        )));
    };
    let vertical = req.vertical.unwrap_or(cut.vertical);
    if vertical && share_mode == "copy" {
        return Err(ShareError::Invalid(
            "a vertical cut needs the h264 mode (copy keeps the frame as recorded)".to_string(),
        ));
    }
    let vertical_pos = vertical.then(|| {
        let pos = req
            .vertical_pos
            .or(cut.vertical_pos)
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);
        (pos * 1000.0).round() / 1000.0
    });
    let target = state
        .runtime()
        .integrations
        .resolve_target(&req.target)
        .ok_or_else(|| {
            ShareError::Invalid(format!(
                "unknown or unconfigured target: {} (a storage id or 'file')",
                req.target
            ))
        })?;
    let duplicate = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| {
            j.cut.as_deref() == Some(&cut.id)
                && j.kind == KIND_RENDER
                && j.target == target
                && j.audio == audio
                && j.mode == share_mode
                && j.vertical == vertical
        })
        .map(|j| j.id.clone());
    if let Some(id) = duplicate {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    let limits = state.settings().limits(&target);
    let after = after_of(state, &req.after)?;
    // since 3.11: what this rendering does about subtitles, and whether it
    // can do it at all
    let remembered = state.db.subtitles(&cut.id).ok().flatten();
    let subtitles = subtitles_for(
        state,
        req.subtitles.as_deref(),
        remembered.as_ref().map(|s| s.mode.as_str()),
        remembered.as_ref().is_some_and(|s| !s.segments.is_empty()),
        share_mode == "copy",
    )?;
    let clip = inner.clips.get(&cut.base);
    let job = Job {
        id: id.clone(),
        kind: KIND_RENDER.to_string(),
        subtitles,
        base: cut.base.clone(),
        cut: Some(cut.id.clone()),
        target,
        start: cut.start,
        end: cut.end,
        seconds: ((cut.end - cut.start) * 100.0).round() / 100.0,
        audio,
        mode: share_mode,
        kbps: limits.max_kbps,
        max_height: limits.max_height,
        codec: clip.map(|c| c.codec.clone()).unwrap_or_default(),
        source_kbps: clip
            .filter(|c| c.duration > 0.0)
            .map(|c| (c.size as f64 * 8.0 / c.duration / 1000.0).round() as u32)
            .unwrap_or(0),
        vertical,
        vertical_pos,
        after,
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut: Some(cut.id),
    })
}

/// `POST /api/cuts/<id>/transcribe` (since 3.11): read the speech of a cut.
/// Stages `queued -> transcribe -> done`; it is no output and never appears
/// in the history.
pub fn start_transcribe(
    state: &AppState,
    cut_id: &str,
    model: &str,
    language: &str,
    source: &str,
) -> Result<Started, ShareError> {
    let cut = state
        .db
        .cut(cut_id)
        .map_err(|e| ShareError::Invalid(format!("cannot read the cut: {e:#}")))?
        .ok_or_else(|| ShareError::UnknownCut(cut_id.to_string()))?;
    if !state.paths_of_cut(&cut).cut_of(&cut.id).is_file() {
        return Err(ShareError::Invalid(format!(
            "the file of cut {} is gone - cut the range again",
            cut.id
        )));
    }
    let mut inner = state.inner.lock();
    let running = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| j.cut.as_deref() == Some(&cut.id) && j.kind == KIND_TRANSCRIBE)
        .map(|j| j.id.clone());
    if let Some(id) = running {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    let job = Job {
        id: id.clone(),
        kind: KIND_TRANSCRIBE.to_string(),
        base: cut.base.clone(),
        cut: Some(cut.id.clone()),
        start: cut.start,
        end: cut.end,
        seconds: ((cut.end - cut.start) * 100.0).round() / 100.0,
        audio: cut.audio.clone(),
        // the transcription belongs to the game, not the other way round
        idle: true,
        model: model.to_string(),
        language: language.to_string(),
        track: source.to_string(),
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut: Some(cut.id),
    })
}

/// `POST /api/jobs/<id>/publish` (since 2.5): send the file of a finished
/// job to another target without cutting again. Returns id and position.
pub fn publish(state: &AppState, source: &str, target: &str) -> Result<Started, ShareError> {
    let mut inner = state.inner.lock();
    // the source may be a job of this run or, after a restart, a history entry
    let src = inner
        .jobs
        .get(source)
        .cloned()
        .or_else(|| {
            inner
                .history
                .iter()
                .find(|e| e["id"] == source)
                .and_then(|e| serde_json::from_value::<Job>(e.clone()).ok())
                .map(|mut j| {
                    j.ok = Some(true);
                    j
                })
        })
        .ok_or_else(|| ShareError::UnknownJob(source.to_string()))?;
    // a share whose upload failed has its file too (since 3.10)
    let finished = src.ok == Some(true) || src.upload_error.is_some();
    let Some(file) = src.file.clone().filter(|_| finished) else {
        return Err(ShareError::Invalid(
            "the source job has no finished file".to_string(),
        ));
    };
    if !state.paths_for(&src.base).shared_dir.join(&file).is_file() {
        return Err(ShareError::Invalid("the shared file is gone".to_string()));
    }
    let runtime = state.runtime();
    let target = runtime
        .integrations
        .resolve_target(target)
        .filter(|t| t != crate::integrations::TARGET_FILE)
        .ok_or_else(|| {
            ShareError::Invalid(format!(
                "publish needs a configured storage target, not '{target}'"
            ))
        })?;
    // since 2.7: a file above the new target's limits is cut again within them
    let limits = state.settings().limits(&target);
    if needs_reencode(&src, limits) {
        drop(inner);
        return start(
            state,
            ShareRequest {
                base: src.base,
                start: src.start,
                end: src.end,
                audio: src.audio,
                mode: "h264".into(),
                target,
                vertical: src.vertical,
                vertical_pos: src.vertical_pos.unwrap_or(0.5),
                // a publish never changes the state of the clip itself
                after: crate::settings::AFTER_KEEP.to_string(),
                // and it carries over what the file it re-cuts already had
                subtitles: src.subtitles.clone(),
            },
        );
    }
    let duplicate = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| j.source.as_deref() == Some(source) && j.target == target)
        .map(|j| j.id.clone());
    if let Some(id) = duplicate {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    let job = Job {
        id: id.clone(),
        kind: KIND_PUBLISH.to_string(),
        base: src.base.clone(),
        // the same file, so the same cut it once came from (since 3.0)
        cut: src.cut.clone(),
        target,
        source: Some(source.to_string()),
        start: src.start,
        end: src.end,
        seconds: src.seconds,
        audio: src.audio.clone(),
        mode: src.mode.clone(),
        kbps: src.kbps,
        // the file was made with these, and the contract says a job carries
        // `kbps` and `maxHeight` as used - without it a capped rendering read
        // as a full-resolution one in the history
        max_height: src.max_height,
        codec: src.codec.clone(),
        source_kbps: src.source_kbps,
        vertical: src.vertical,
        vertical_pos: src.vertical_pos,
        title: src.title.clone(),
        file: Some(file),
        size_mb: src.size_mb,
        actual_start: src.actual_start,
        stage: "queued".into(),
        percent: 100,
        at: util::now_local(),
        ..Job::default()
    };
    let cut = job.cut.clone();
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut,
    })
}

/// Does the finished file of `src` exceed `limits`? A copy-mode file and
/// a quality-driven encode (`kbps` 0) exceed any bitrate cap; a file made
/// without a height cap exceeds any height cap.
pub fn needs_reencode(src: &Job, limits: crate::settings::Limits) -> bool {
    if limits.max_kbps > 0 && (src.mode == "copy" || src.kbps == 0 || src.kbps > limits.max_kbps) {
        return true;
    }
    limits.max_height > 0
        && (src.mode == "copy" || src.max_height == 0 || src.max_height > limits.max_height)
}

/// Run the running job to completion, then the next one from the queue.
/// Spawned by the HTTP handler for a job that got position 0.
pub fn run(
    state: Arc<AppState>,
    id: String,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(run_inner(state, id))
}

async fn run_inner(state: Arc<AppState>, id: String) {
    let token = state.cancel_token(&id);
    let kind = state.job(&id).map(|j| j.kind).unwrap_or_default();
    let (preview, cut_only) = (kind == crate::state::KIND_PREVIEW, kind == KIND_CUT);
    let transcribe = kind == KIND_TRANSCRIBE;
    let result = if preview {
        preview_pipeline(&state, &id, &token).await
    } else if cut_only {
        cut_pipeline(&state, &id, &token).await
    } else if transcribe {
        transcribe_pipeline(&state, &id, &token).await
    } else {
        pipeline(&state, &id, &token).await
    };
    let what = match (preview, cut_only, transcribe) {
        (true, _, _) => "preview",
        (_, true, _) => "cut",
        (_, _, true) => "transcribe",
        _ => "share",
    };
    if let Err(e) = &result {
        if token.is_cancelled() {
            tracing::info!("{what} [{id}] cancelled");
        } else {
            tracing::error!("{what} [{id}] failed: {e:#}");
        }
    }
    let failed = result.is_err();
    let next = state.complete_job(&id, result.map_err(|e| format!("{e:#}")));
    if let Some(job) = state.job(&id) {
        // a cut that never got its file leaves no half-cut behind; a
        // transcription never made one, so it must not drop one either
        if failed && !transcribe {
            if let Some(cut) = job.cut.as_deref() {
                state.drop_pending_cut(cut);
            }
        }
        if !failed && !transcribe {
            apply_after(&state, &job).await;
        }
        if !job.cancelled && !preview && !cut_only && !transcribe {
            let uploaded = job.direct.is_some();
            toast::show(&state, Toast::share_result(&job, uploaded, &state.ui_url()));
        }
    }
    state.cancels.lock().remove(&id);
    if let Some(next) = next {
        tokio::spawn(run(state, next));
    }
}

/// "Afterwards" of a finished job (since 3.0): the clip leaves the list, and
/// with `recycle` its recording goes to the recycle bin as well. The cut and
/// everything that came out of it stay either way.
async fn apply_after(state: &Arc<AppState>, job: &Job) {
    use crate::settings::{AFTER_DONE, AFTER_RECYCLE};
    match job.after.as_str() {
        AFTER_DONE => {
            if let Err(e) = state.set_clip_state(&job.base, crate::db::CLIP_DONE) {
                tracing::warn!("share [{}]: cannot mark the clip done: {e}", job.id);
            }
        }
        AFTER_RECYCLE => {
            // A later job of this clip may still have to cut from the
            // recording. Until 3.9 it lost it and failed with "unknown clip";
            // now the clip is only put away here, and the last such job
            // recycles the recording when it is done.
            if let Some(heir) = hand_on_recycling(state, job) {
                tracing::info!(
                    "share [{}]: the recording of {} stays until job {heir} has cut from it",
                    job.id,
                    job.base
                );
                if let Err(e) = state.set_clip_state(&job.base, crate::db::CLIP_DONE) {
                    tracing::warn!("share [{}]: cannot mark the clip done: {e}", job.id);
                }
            } else {
                crate::state::recycle_recording(state, &job.base).await;
            }
        }
        _ => {}
    }
}

/// The last job after `job` - running now or waiting - that needs the
/// recording of the same clip, with "Afterwards: recycle" handed to it.
fn hand_on_recycling(state: &AppState, job: &Job) -> Option<String> {
    let mut inner = state.inner.lock();
    let later: Vec<String> = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter(|id| **id != job.id)
        .cloned()
        .collect();
    let heir = later.into_iter().rev().find(|id| {
        inner
            .jobs
            .get(id)
            .is_some_and(|j| j.base == job.base && needs_recording(j))
    })?;
    if let Some(j) = inner.jobs.get_mut(&heir) {
        j.after = crate::settings::AFTER_RECYCLE.to_string();
    }
    Some(heir)
}

/// Whether a job reads the recording itself: a share cuts its range from it
/// and the playable preview encodes it; a render has its cut, a publish its
/// file.
fn needs_recording(job: &Job) -> bool {
    let share = job.kind.is_empty() || job.kind == crate::state::KIND_SHARE;
    job.source.is_none() && (share || job.kind == KIND_CUT || job.is_preview())
}

/// The file an encode writes until it is finished: `x.mp4` → `x.part.mp4`.
pub fn part_of(out: &Path) -> PathBuf {
    out.with_extension("part.mp4")
}

/// Whether a file in `shared\` is an unfinished encode (see [`part_of`]).
pub fn is_part(name: &str) -> bool {
    name.to_ascii_lowercase().ends_with(".part.mp4")
}

/// A stop or a crash during an encode leaves its `.part.mp4` in `shared\`.
/// Called on start, before any job can run, so each one is a leftover.
pub fn remove_unfinished_encodes(shared: &Path) {
    let Ok(entries) = std::fs::read_dir(shared) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_part(&name) || !entry.path().is_file() {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!("removed the unfinished encode {name}"),
            Err(e) => tracing::warn!("cannot remove the unfinished encode {name}: {e}"),
        }
    }
}

/// Where a rendering starts inside the cut file: the cut begins at its
/// keyframe, the range a moment later.
fn seek_in_cut(cut: &Cut, start: f64) -> f64 {
    (start - cut.actual_start.unwrap_or(cut.start)).max(0.0)
}

/// A cut made by 3.0 to 3.9 whose range starts right on a keyframe says it
/// begins one keyframe earlier than it does (the lookup never read the
/// keyframe on the start), and a rendering from it loses that much at the
/// front. While the recording is there, ask it again and put the answer in
/// the store; without the recording the stored value is all there is.
async fn recheck_cut_start(state: &AppState, cut: &mut Cut) {
    let Some(clip) = clip_path_of(state, &cut.base).ok().filter(|p| p.is_file()) else {
        return;
    };
    let Some(keyframe) = state
        .runtime()
        .media
        .keyframe_at_or_before(&clip, cut.start)
        .await
    else {
        return;
    };
    if cut
        .actual_start
        .is_some_and(|s| (s - keyframe).abs() < 0.001)
    {
        return;
    }
    let stored = cut
        .actual_start
        .map_or_else(|| "an unknown point".to_string(), |s| format!("{s:.3} s"));
    tracing::info!(
        "cut [{}]: begins at {keyframe:.3} s, not at {stored} - corrected",
        cut.id
    );
    match state
        .db
        .set_cut_file(&cut.id, cut.file.as_deref(), Some(keyframe), &cut.state)
    {
        Ok(()) => cut.actual_start = Some(keyframe),
        Err(e) => tracing::warn!("cut [{}]: cannot store where it begins: {e:#}", cut.id),
    }
}

/// The cut file of this job's range: ready already, or made now. Stream copy
/// with every audio track, so nothing is lost and no GPU is needed.
async fn make_cut(
    state: &AppState,
    job: &Job,
    clip: &Path,
    token: &CancellationToken,
) -> Result<Cut> {
    let id = job
        .cut
        .as_deref()
        .ok_or_else(|| anyhow!("job has no cut to make"))?;
    let mut cut = state
        .db
        .cut(id)?
        .ok_or_else(|| anyhow!("cut {id} is no longer known"))?;
    // since 3.10.1: beside the recording, wherever that is
    let out = state.paths_of_cut(&cut).cut_of(&cut.id);
    if cut.state == CUT_READY && out.is_file() {
        tracing::info!("share [{}]: cut {} is there already", job.id, cut.id);
        recheck_cut_start(state, &mut cut).await;
        return Ok(cut);
    }
    let runtime = state.runtime();
    let started = Instant::now();
    // `.cuts\` is made with the first cut, not at start (since 3.10)
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    tokio::select! {
        r = runtime.media.cut(clip, &out, job.start, job.seconds) => r.context("cut")?,
        _ = token.cancelled() => {
            let _ = std::fs::remove_file(&out);
            bail!("cancelled during cut");
        }
    }
    // The cut begins at the keyframe at or before `start`, and every
    // rendering seeks by the difference. Asking the recording where that
    // keyframe is beats deriving it from the length of the cut: a container
    // duration counts the last frame's own time as well, which is a frame
    // too much and costs the rendering its last one.
    let actual_start = match runtime.media.keyframe_at_or_before(clip, job.start).await {
        Some(keyframe) => keyframe,
        None => {
            let len = runtime
                .media
                .duration_exact(&out)
                .await
                .unwrap_or(job.seconds);
            (job.start - (len - job.seconds)).max(0.0)
        }
    };
    let size_mb = std::fs::metadata(&out)
        .map(|m| (m.len() as f64 / 1_048_576.0 * 10.0).round() / 10.0)
        .unwrap_or(0.0);
    let name = cut_file_name(&cut.id);
    state
        .db
        .set_cut_file(&cut.id, Some(&name), Some(actual_start), CUT_READY)?;
    cut.file = Some(name);
    cut.actual_start = Some(actual_start);
    cut.state = CUT_READY.to_string();
    tracing::info!(
        "cut [{}]: {} {}-{} s -> {} ({size_mb} MB, starts at {actual_start:.3} s) in {:.1} s",
        cut.id,
        job.base,
        job.start,
        job.end,
        out.display(),
        started.elapsed().as_secs_f64()
    );
    state.tray_changed();
    Ok(cut)
}

/// `kind: cut`: make the cut file and stop. Nothing is encoded or sent.
async fn cut_pipeline(state: &AppState, id: &str, token: &CancellationToken) -> Result<()> {
    let job = state.job(id).ok_or_else(|| anyhow!("job vanished"))?;
    let clip_path = clip_path_of(state, &job.base)?;
    state.with_job(id, |j| j.stage = "cut".into());
    let cut = make_cut(state, &job, &clip_path, token).await?;
    state.with_job(id, |j| {
        j.cut = Some(cut.id.clone());
        j.percent = 100;
    });
    Ok(())
}

fn clip_path_of(state: &AppState, base: &str) -> Result<PathBuf> {
    let inner = state.inner.lock();
    let clip = inner
        .clips
        .get(base)
        .ok_or_else(|| anyhow!("unknown clip: {base}"))?;
    Ok(PathBuf::from(&clip.path))
}

/// `POST /api/clips/<base>/preview` (since 2.6): queue the playable H.264
/// copy of a clip. `idle` marks the scan-time variant (idle priority).
/// Returns the job id and its place in the queue.
pub fn start_preview(state: &AppState, base: &str, idle: bool) -> Result<Started, ShareError> {
    let mut inner = state.inner.lock();
    let clip = inner
        .clips
        .get(base)
        .ok_or_else(|| ShareError::UnknownClip(base.to_string()))?;
    if clip.preview_h264.is_some() || state.paths_for(base).preview_h264_of(base).is_file() {
        return Err(ShareError::Invalid(
            "the playable preview exists already".to_string(),
        ));
    }
    let duplicate = inner
        .current_job
        .iter()
        .chain(inner.queue.iter())
        .filter_map(|id| inner.jobs.get(id))
        .find(|j| j.is_preview() && j.base == base)
        .map(|j| j.id.clone());
    if let Some(id) = duplicate {
        return Err(ShareError::Busy(id));
    }
    if inner.queue.len() >= MAX_QUEUE {
        return Err(ShareError::QueueFull);
    }
    let mut id = random_token(8);
    while inner.jobs.contains_key(&id) {
        id = random_token(8);
    }
    let job = Job {
        id: id.clone(),
        kind: crate::state::KIND_PREVIEW.to_string(),
        idle,
        base: clip.base.clone(),
        target: crate::integrations::TARGET_FILE.to_string(),
        start: 0.0,
        end: clip.duration,
        seconds: clip.duration,
        audio: "mix".into(),
        mode: "h264".into(),
        kbps: PREVIEW_KBPS,
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    let position = state.register_job(&mut inner, job);
    Ok(Started {
        job: id,
        position,
        cut: None,
    })
}

/// The playable copy: 720p H.264 at `PREVIEW_KBPS` with audio track 1,
/// into `.preview/<base>.h264.mp4`; the clip learns the URL when it is done.
async fn preview_pipeline(state: &AppState, id: &str, token: &CancellationToken) -> Result<()> {
    let job = state.job(id).ok_or_else(|| anyhow!("job vanished"))?;
    let runtime = state.runtime();
    let clip_path = {
        let inner = state.inner.lock();
        let clip = inner
            .clips
            .get(&job.base)
            .ok_or_else(|| anyhow!("unknown clip: {}", job.base))?;
        PathBuf::from(&clip.path)
    };
    let out = state.paths_for(&job.base).preview_h264_of(&job.base);
    let tmp = out.with_extension("part.mp4");
    tracing::info!(
        "preview [{id}]: {} ({} s) -> {}{}",
        job.base,
        job.seconds,
        out.display(),
        if job.idle { " (idle priority)" } else { "" }
    );
    state.with_job(id, |j| j.stage = "encode".into());
    let started = Instant::now();
    let profile = runtime.encoder.clone();
    if let Err(e) = encode(
        state, id, &job, &clip_path, 0.0, &tmp, token, &profile, None,
    )
    .await
    {
        if token.is_cancelled() || !profile.is_gpu_path() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        tracing::warn!(
            "preview [{id}]: {} failed ({e:#}) - retrying with software decoding",
            profile.label
        );
        let _ = std::fs::remove_file(&tmp);
        encode(
            state,
            id,
            &job,
            &clip_path,
            0.0,
            &tmp,
            token,
            &profile.software_fallback(),
            None,
        )
        .await?;
    }
    std::fs::rename(&tmp, &out).context("move the preview into place")?;
    let size_mb = (std::fs::metadata(&out)?.len() as f64 / 1_048_576.0 * 100.0).round() / 100.0;
    state.with_job(id, |j| {
        j.percent = 100;
        j.size_mb = Some(size_mb);
    });
    state.set_preview_h264(&job.base, Some(crate::state::preview_h264_url(&job.base)));
    tracing::info!(
        "preview [{id}]: ready in {} s, {size_mb} MB",
        started.elapsed().as_secs()
    );
    Ok(())
}

/// Bitrate of the playable preview in kbit/s.
pub const PREVIEW_KBPS: u32 = 2000;

/// `POST /api/jobs/<id>/post { target }` (since 2.7): post the link of a
/// finished job to one notify integration now. Returns the status text.
/// Which audio stream of a cut file carries the speech (since 3.11).
///
/// Not guessed from the number of tracks: replaycut already knows how they
/// are laid out. OBS is asked first - the check behind the diagnostics line
/// "Audio tracks" knows which OBS track is fed by a microphone and nothing
/// else. If OBS is not connected, or its configuration has moved on since
/// the recording, the layout that `audio_args` has asserted since 1.4
/// applies: 0 the mix, 1 the microphone, 2 the game, 3 the voice chat.
/// Anything less than four tracks is a simple recording, and that is the
/// mix.
async fn speech_stream(
    state: &AppState,
    file: &Path,
    wanted: &str,
) -> (u32, &'static str, &'static str) {
    let tracks = state.runtime().media.audio_tracks(file).await;
    if wanted == crate::db::SOURCE_MIX {
        return (0, crate::db::SOURCE_MIX, "asked for");
    }
    if tracks < 2 {
        return (0, crate::db::SOURCE_MIX, "one track only");
    }
    if let Some(facts) = state.obs.status().facts.as_ref() {
        // OBS counts tracks from 1, ffmpeg counts streams from 0
        if let Some(stream) = crate::obs_status::microphone_track(facts)
            .map(|t| t.saturating_sub(1))
            .filter(|s| *s < tracks)
        {
            return (stream, crate::db::SOURCE_MIC, "OBS says so");
        }
    }
    if tracks >= 4 {
        return (1, crate::db::SOURCE_MIC, "the usual layout");
    }
    // asked for the microphone, but this recording has no separate one
    (0, crate::db::SOURCE_MIX, "no separate microphone")
}

/// A folder that goes when the rendering is over, whichever way it ends.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Lay out what a rendering needs to show subtitles (since 3.11): the ASS
/// file for burning in, the SRT for the track, and the bundled font beside
/// them. `None` when this rendering was not asked for any.
///
/// A cut that has no transcript yet gets one here, as a stage before the
/// encode: asking for subtitles on a cut that has none is a request to read
/// them, not a mistake.
async fn prepare_subtitles(
    state: &AppState,
    id: &str,
    job: &Job,
    token: &CancellationToken,
) -> Result<Option<RenderSubs>> {
    let mode = job.subtitles.as_str();
    if mode.is_empty() || mode == crate::db::SUBS_NONE {
        return Ok(None);
    }
    let cut_id = job
        .cut
        .clone()
        .ok_or_else(|| anyhow!("subtitles need a cut"))?;
    let mut cut = state
        .db
        .cut(&cut_id)?
        .ok_or_else(|| anyhow!("the cut of this rendering is no longer known"))?;
    let subs = match state.db.subtitles(&cut_id)? {
        Some(s) => s,
        None => {
            // read them now, in front of the encode
            recheck_cut_start(state, &mut cut).await;
            let input = state.paths_of_cut(&cut).cut_of(&cut.id);
            let settings = state.settings();
            transcribe_into(
                state,
                id,
                &cut,
                &input,
                &settings.subtitles.model,
                &settings.subtitles.language,
                &settings.subtitles.source,
                token,
            )
            .await?
        }
    };
    if subs.segments.is_empty() {
        bail!("there is no speech in this cut to put on the picture");
    }

    let dir = state.data_dir.join("tmp").join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // the rendering starts where the job starts, so that is time zero
    let offset = job.start;
    if mode == crate::db::SUBS_BURN {
        let settings = state.settings();
        let style = &settings.subtitles.style;
        // the picture the subtitles are drawn into: a vertical rendering is
        // always 1080x1920, everything else keeps the recording's frame
        // unless the target caps its height
        let (w, h) = if job.vertical {
            (1080, 1920)
        } else {
            let clip = state.inner.lock().clips.get(&job.base).cloned();
            let (cw, ch) = clip.map(|c| (c.width, c.height)).unwrap_or((1920, 1080));
            let (cw, ch) = (cw.max(16), ch.max(16));
            if job.max_height > 0 && ch > job.max_height {
                (cw * job.max_height / ch, job.max_height)
            } else {
                (cw, ch)
            }
        };
        std::fs::write(
            dir.join(crate::subtitles::ASS_FILE),
            crate::subtitles::to_ass(
                &subs.segments,
                offset,
                style.placement(job.vertical),
                style,
                w,
                h,
            ),
        )?;
        std::fs::write(
            dir.join(crate::subtitles::FONT_FILE),
            crate::subtitles::FONT,
        )?;
    }
    if mode == crate::db::SUBS_TRACK {
        std::fs::write(
            dir.join(crate::subtitles::TRACK_FILE),
            crate::subtitles::to_srt(&subs.segments, offset),
        )?;
    }
    tracing::info!(
        "{} [{id}]: {} subtitle(s) {}",
        job.kind,
        subs.segments.len(),
        if mode == crate::db::SUBS_BURN {
            "burned into the picture"
        } else {
            "as a track"
        }
    );
    // remember what this cut was rendered with, for the next time
    let mut remember = subs.clone();
    remember.mode = mode.to_string();
    let _ = state.db.put_subtitles(&cut_id, Some(&remember));
    Ok(Some(RenderSubs {
        dir,
        burn: mode == crate::db::SUBS_BURN,
        track: mode == crate::db::SUBS_TRACK,
        language: subs.language.clone(),
    }))
}

/// The `transcribe` pipeline (since 3.11): run the speech of the cut
/// through whisper and put the segments on the cut. Writes no file, sends
/// nothing anywhere, and leaves no entry in the history.
async fn transcribe_pipeline(state: &AppState, id: &str, token: &CancellationToken) -> Result<()> {
    let job = state.job(id).ok_or_else(|| anyhow!("job vanished"))?;
    let mut cut = state
        .db
        .cut(job.cut.as_deref().unwrap_or_default())?
        .ok_or_else(|| anyhow!("the cut of this transcription is no longer known"))?;
    let input = state.paths_of_cut(&cut).cut_of(&cut.id);
    recheck_cut_start(state, &mut cut).await;
    transcribe_into(
        state,
        id,
        &cut,
        &input,
        &job.model,
        &job.language,
        &job.track,
        token,
    )
    .await?;
    Ok(())
}

/// Read the speech of a cut and put the segments on it. The `transcribe`
/// job is one caller; a rendering that was asked for subtitles on a cut
/// that has none is the other (since 3.11).
#[allow(clippy::too_many_arguments)]
async fn transcribe_into(
    state: &AppState,
    id: &str,
    cut: &Cut,
    input: &Path,
    model: &str,
    language: &str,
    want_track: &str,
    token: &CancellationToken,
) -> Result<crate::db::Subtitles> {
    let settings = state.settings();
    if !settings.subtitles.enabled {
        bail!("subtitles are switched off");
    }
    let runtime = state.runtime();
    if !runtime.whisper {
        bail!("this ffmpeg was built without the whisper filter");
    }
    if !input.is_file() {
        bail!("the file of cut {} is gone - cut the range again", cut.id);
    }
    let model = crate::subtitles::model(model).ok_or_else(|| anyhow!("unknown model: {model}"))?;
    // the filter is given the bare file name and ffmpeg runs in that folder,
    // so all this needs is that the file is there and is the right one
    if crate::subtitles::ready(&state.data_dir, model).is_none() {
        bail!("the model {} is not in the models folder", model.name);
    }
    let (stream, source, why) = speech_stream(state, input, want_track).await;
    state.with_job(id, |j| {
        j.stage = "transcribe".into();
        j.percent = 0;
        j.track = source.to_string();
    });

    // libavfilter parses `:` and `\` inside a filter argument, so a Windows
    // path in there is a fight nobody wins. Everything the filter names is
    // a bare file name and ffmpeg runs in the folder that holds them.
    let dir = crate::subtitles::models_dir(&state.data_dir);
    let out_name = format!("{id}.srt");
    let out = dir.join(&out_name);
    let _ = std::fs::remove_file(&out);
    let vad = crate::subtitles::ready(&state.data_dir, &crate::subtitles::VAD)
        .map(|_| format!(":vad_model={}", crate::subtitles::VAD.file))
        .unwrap_or_default();
    // `queue` is the window whisper gets to look at, and it decides how the
    // text reads. Measured on a real recording (2026-09-20): at 10 s a
    // sentence is cut wherever the window ends, and `max_len` cut it again
    // by character count, so half the lines broke mid-phrase. At 30 s -
    // whisper's own window - the sentences come out whole and the words are
    // recognised better. It costs time: 0.9x real time instead of 1.5x.
    // Line length is the editor's and the renderer's business, not the
    // transcription's, so `max_len` is off.
    let filter = format!(
        "aresample=16000,aformat=sample_fmts=s16:channel_layouts=mono,\
         whisper=model={}:language={}{vad}:queue=30:max_len=0:use_gpu={}:\
         format=srt:destination={out_name}",
        model.file,
        language,
        u8::from(settings.subtitles.gpu),
    );
    let input_s = input.to_string_lossy().into_owned();
    let map = format!("0:a:{stream}");
    let args = [
        "-nostats",
        "-progress",
        "pipe:1",
        "-y",
        "-v",
        "error",
        "-i",
        &input_s,
        "-vn",
        "-map",
        &map,
        "-af",
        &filter,
        "-f",
        "null",
        "-",
    ];
    tracing::info!(
        "transcribe [{id}]: cut {} from track {stream} ({source}, {why}) with {} in '{}'",
        cut.id,
        model.name,
        language
    );
    let media = runtime
        .media
        .clone()
        .with_resource_limits(crate::settings::FfmpegPriority::Idle, runtime.media.threads);
    let mut cmd = media.ffmpeg_command();
    cmd.current_dir(&dir);
    cmd.args(args);
    run_with_progress(state, id, cmd, cut.end - cut.start, token).await?;

    let text = std::fs::read_to_string(&out).unwrap_or_default();
    let _ = std::fs::remove_file(&out);
    let offset = cut.actual_start.unwrap_or(cut.start);
    let segments = crate::subtitles::tidy(crate::subtitles::parse_srt(&text, offset));
    let language = if language == "auto" {
        // whisper detected one; the SRT does not say which, so the honest
        // answer until the editor is told otherwise is that we do not know
        String::new()
    } else {
        language.to_string()
    };
    tracing::info!("transcribe [{id}]: {} segment(s)", segments.len());
    let subs = crate::db::Subtitles {
        language,
        model: model.name.to_string(),
        source: source.to_string(),
        at: util::now_local(),
        mode: state
            .db
            .subtitles(&cut.id)
            .ok()
            .flatten()
            .map(|s| s.mode)
            .unwrap_or_else(|| crate::db::SUBS_NONE.to_string()),
        edited: false,
        segments,
    };
    state.db.put_subtitles(&cut.id, Some(&subs))?;
    state.tray_changed();
    Ok(subs)
}

pub async fn post_now(state: &AppState, id: &str, target: &str) -> Result<String, ShareError> {
    let job = state
        .job(id)
        .or_else(|| state.history_job(id))
        .ok_or_else(|| ShareError::UnknownJob(id.to_string()))?;
    let Some(direct) = job.direct.clone() else {
        return Err(ShareError::Invalid(
            "this job produced no link to post".to_string(),
        ));
    };
    let runtime = state.runtime();
    let entry = runtime
        .integrations
        .notifies
        .iter()
        .find(|n| n.id == target)
        .ok_or_else(|| {
            ShareError::Invalid(format!("unknown or unconfigured notify target: {target}"))
        })?;
    let settings = state.settings();
    let prefix = settings.display_name.clone();
    let title = job.title.clone().unwrap_or_default();
    let label = post_label(&prefix, &job.base, &title);
    let text = format!(
        "**{prefix}** {label} ({} s) - {direct}",
        job.seconds.round() as i64
    );
    let n = crate::notify::Notification {
        text,
        prefix,
        label,
        title,
        base: job.base.clone(),
        seconds: job.seconds,
        target: job.target.clone(),
        link: job.link.clone().unwrap_or_else(|| direct.clone()),
        direct,
        at: job.at.clone(),
        job: id.to_string(),
    };
    let status = entry
        .notify
        .post(&n)
        .await
        .map_err(|e| ShareError::Invalid(format!("{}: {e:#}", entry.label)))?;
    tracing::info!("post [{id}]: {}: {status}", entry.label);
    let note = format!("{}: {status}", entry.label);
    state.note_post(id, &note);
    Ok(status)
}

async fn pipeline(state: &AppState, id: &str, token: &CancellationToken) -> Result<()> {
    let job = state.job(id).ok_or_else(|| anyhow!("job vanished"))?;
    // The runtime of the moment the job started; a settings change while
    // it runs does not swap integrations or encoder under its feet.
    let runtime = state.runtime();
    let settings = state.settings();
    // A publish job (since 2.5) re-uses the file of its source; a share cuts
    // one and renders it, a render (since 3.0) finds its cut ready.
    let republish = job.source.is_some();
    let title = if republish {
        job.title.clone().unwrap_or_default()
    } else {
        let inner = state.inner.lock();
        inner.names.get(&job.base).cloned().unwrap_or_default()
    };
    let file_name = match &job.file {
        Some(f) if republish => f.clone(),
        _ => {
            let name = share_file_name(&job.base, job.start, job.end, &slug(&title));
            let name = if job.vertical {
                vertical_file_name(&name)
            } else {
                name
            };
            free_file_name(state, id, &name)
        }
    };
    let out = state.paths_for(&job.base).shared_dir.join(&file_name);
    // the storage this job goes to, or none for `file`
    let storage = if job.target == crate::integrations::TARGET_FILE {
        None
    } else {
        Some(
            runtime
                .integrations
                .storage(&job.target)
                .ok_or_else(|| anyhow!("target '{}' is no longer configured", job.target))?,
        )
    };
    if republish {
        tracing::info!(
            "publish [{id}]: {file_name} from job {} -> {}",
            job.source.as_deref().unwrap_or("?"),
            job.target
        );
    }
    let mode_label = AUDIO_MODES
        .iter()
        .find(|m| m.id == job.audio)
        .map(|m| m.label)
        .unwrap_or("?");
    // A publish encodes nothing and said so above; a render is named a render.
    // Until 3.9 both read as a share "@ N kbps" in the log.
    if !republish {
        tracing::info!(
            "{} [{id}]: {} {}-{} s ({} s) {}{}, audio '{mode_label}' -> {file_name}",
            if job.kind == crate::state::KIND_RENDER {
                "render"
            } else {
                "share"
            },
            job.base,
            job.start,
            job.end,
            job.seconds,
            if job.mode == "copy" {
                "copy (no re-encode)".to_string()
            } else if job.kbps == 0 {
                "at best quality".to_string()
            } else {
                format!("@ {} kbps", job.kbps)
            },
            if job.vertical {
                format!(" vertical 9:16 at {:.2}", job.vertical_pos.unwrap_or(0.5))
            } else {
                String::new()
            }
        );
    }

    // cut (since 3.0): the rendering is made from the cut file, not from the
    // recording. A share cuts its range first (stage `cut`, stream copy, a
    // second of IO); a render finds the cut it was asked for.
    let source = if republish {
        None
    } else if job.kind == KIND_RENDER {
        let mut cut = state
            .db
            .cut(job.cut.as_deref().unwrap_or_default())?
            .ok_or_else(|| anyhow!("the cut of this render is no longer known"))?;
        let file = state.paths_of_cut(&cut).cut_of(&cut.id);
        if !file.is_file() {
            bail!("the file of cut {} is gone - cut the range again", cut.id);
        }
        recheck_cut_start(state, &mut cut).await;
        Some((file, seek_in_cut(&cut, job.start)))
    } else {
        state.with_job(id, |j| j.stage = "cut".into());
        let clip_path = clip_path_of(state, &job.base)?;
        let cut = make_cut(state, &job, &clip_path, token).await?;
        state.with_job(id, |j| j.cut = Some(cut.id.clone()));
        Some((
            state.paths_of_cut(&cut).cut_of(&cut.id),
            seek_in_cut(&cut, job.start),
        ))
    };

    // encode (skipped when the file already exists from the source job)
    if let Some((input, seek)) = source {
        state.with_job(id, |j| {
            j.stage = "encode".into();
            j.title = Some(title.clone());
        });
        let started = Instant::now();
        // The GPU path may fail on a driver quirk: try once more with software
        // decoding and CPU scaling before giving up (since 2.4).
        // A vertical cut crops on the CPU, and since 3.11 burned-in subtitles
        // are drawn on the CPU too, so frames that a GPU filter would keep on
        // the card (cuda, qsv) are decoded in software instead.
        let burning = job.subtitles == crate::db::SUBS_BURN;
        let profile = if (job.vertical || burning) && runtime.encoder.gpu_frames() {
            runtime.encoder.software_fallback()
        } else {
            runtime.encoder.clone()
        };
        // ffmpeg writes `<name>.part.mp4`, and only a finished encode gets
        // the output's name. Until 3.9 it wrote the name itself, so a stop
        // or a failed encode left a broken file that looked like an output;
        // leftovers of a stop or a crash go when the service starts.
        let part = part_of(&out);
        // `shared\` is made with the first share, not at start (since 3.10)
        if let Some(dir) = out.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        // since 3.11: the subtitle files of this rendering, written into a
        // folder of their own and removed with it
        let subs = prepare_subtitles(state, id, &job, token).await?;
        let _sweep = subs.as_ref().map(|s| TempDir(s.dir.clone()));
        if let Err(e) = encode(
            state,
            id,
            &job,
            &input,
            seek,
            &part,
            token,
            &profile,
            subs.as_ref(),
        )
        .await
        {
            let _ = std::fs::remove_file(&part);
            if token.is_cancelled() || job.mode == "copy" || !profile.is_gpu_path() {
                return Err(e);
            }
            tracing::warn!(
                "share [{id}]: {} failed ({e:#}) - retrying with software decoding",
                profile.label
            );
            state
                .encoder_fallbacks
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let retry = encode(
                state,
                id,
                &job,
                &input,
                seek,
                &part,
                token,
                &profile.software_fallback(),
                subs.as_ref(),
            )
            .await;
            if let Err(e) = retry {
                let _ = std::fs::remove_file(&part);
                return Err(e);
            }
        }
        if let Err(e) = std::fs::rename(&part, &out) {
            let _ = std::fs::remove_file(&part);
            bail!("cannot move the finished file into place: {e}");
        }
        let size_mb = (std::fs::metadata(&out)?.len() as f64 / 1_048_576.0 * 100.0).round() / 100.0;
        // copy mode cuts at the keyframe before `start`: say where the file really begins
        let actual_start = if job.mode == "copy" {
            let len = runtime.media.duration(&out).await.unwrap_or(job.seconds);
            let s = ((job.start - (len - job.seconds)).max(0.0) * 100.0).round() / 100.0;
            if s < job.start {
                tracing::info!(
                    "share [{id}]: copy starts {:.1} s earlier (keyframe)",
                    job.start - s
                );
            }
            Some(s)
        } else {
            None
        };
        state.with_job(id, |j| {
            j.percent = 100;
            j.size_mb = Some(size_mb);
            j.file = Some(file_name.clone());
            j.actual_start = actual_start;
        });
        tracing::info!(
            "share [{id}]: encoded in {} s, {size_mb} MB",
            started.elapsed().as_secs()
        );
    }

    // upload
    let mut direct: Option<String> = None;
    if let Some(entry) = storage {
        let storage = &entry.storage;
        state.with_job(id, |j| j.stage = "upload".into());
        let month = month_of(&job.base);
        let meta = crate::integrations::PublishMeta {
            month: month.clone(),
            title: title.clone(),
            base: job.base.clone(),
            display_name: settings.display_name.clone(),
            vertical: job.vertical,
            at: job.at.clone(),
        };
        let published = tokio::select! {
            r = storage.publish(&out, &meta) => r.context("upload")?,
            _ = token.cancelled() => {
                // the upload may have finished on the server before the request was dropped
                let path = storage.remote_path(&month, &file_name);
                if path.is_empty() {
                    tracing::debug!("share [{id}]: no remote cleanup possible for {}", entry.label);
                } else if let Err(e) = storage.delete(std::slice::from_ref(&path)).await {
                    tracing::debug!("share [{id}]: remote cleanup of {path}: {e:#}");
                }
                bail!("cancelled during upload");
            }
        };
        tracing::info!("share [{id}]: link {}", published.page);
        state.with_job(id, |j| {
            j.link = Some(published.page.clone());
            j.direct = Some(published.direct.clone());
            j.nc_path = Some(published.path.clone());
        });
        if !state.dry_run {
            let text = published.direct.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || platform::copy_text(&text)).await? {
                tracing::warn!("clipboard: {e:#}");
            }
        }
        direct = Some(published.direct);
        state.quota_wake.notify_one();
    }

    // notify (stage `notify` since 2.5): every integration that posts
    // automatically - since 2.7 only for the quick share, that is a share
    // to the default storage; menu shares and publishes stay quiet and the
    // page offers "Post to ..." instead
    let quick = job.source.is_none()
        && runtime
            .integrations
            .default_storage()
            .is_some_and(|s| s.id == job.target);
    if let (Some(direct), true) = (direct, quick) {
        let notifies: Vec<_> = runtime.integrations.auto_notifies().collect();
        if !notifies.is_empty() {
            state.with_job(id, |j| j.stage = "notify".into());
            let prefix = &settings.display_name;
            let label = post_label(prefix, &job.base, &title);
            let text = format!(
                "**{prefix}** {label} ({} s) - {direct}",
                job.seconds.round() as i64
            );
            let notification = crate::notify::Notification {
                text,
                prefix: prefix.clone(),
                label,
                title: title.clone(),
                base: job.base.clone(),
                seconds: job.seconds,
                target: job.target.clone(),
                link: state
                    .job(id)
                    .and_then(|j| j.link)
                    .unwrap_or_else(|| direct.clone()),
                direct: direct.clone(),
                at: job.at.clone(),
                job: id.to_string(),
            };
            let mut statuses = Vec::new();
            for entry in notifies {
                let status = match entry.notify.post(&notification).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!("share [{id}]: {} post failed: {e:#}", entry.label);
                        // with its cause: "error sending request" alone says nothing
                        format!("post failed: {e:#}")
                    }
                };
                tracing::info!("share [{id}]: {}: {status}", entry.label);
                // Always with the target's name, as "Post the link to …" has
                // done since 2.7; with one target the automatic post left it
                // out, so the same post read differently by path (#46).
                statuses.push(format!("{}: {status}", entry.label));
            }
            state.with_job(id, |j| j.discord = Some(statuses.join(" · ")));
        }
    }
    Ok(())
}

/// `seek` is where in `input` the range begins: zero for the preview copy of
/// a whole clip, the offset into the cut file for everything else (since 3.0).
#[allow(clippy::too_many_arguments)]
/// Everything an encode needs to know, so that [`encode_args`] can be a
/// plain function a test can pin down.
pub struct Encode<'a> {
    pub job: &'a Job,
    pub enc: &'a crate::media::Encoder,
    /// `-threads` for ffmpeg; 0 leaves it to ffmpeg.
    pub threads: u32,
    pub input: &'a Path,
    /// How far into `input` the rendering starts.
    pub seek: f64,
    pub out: &'a Path,
    /// Since 3.11: the ASS file to render into the picture, as a bare name
    /// in the folder ffmpeg is started in. A path would have to be escaped
    /// twice over for the filter graph, and on Windows that is a fight
    /// nobody wins - so the working directory does the pointing instead.
    pub burn: Option<&'a str>,
    /// Since 3.11: a subtitle file to carry along as a track, with the
    /// language it is in.
    pub track: Option<(&'a str, &'a str)>,
}

/// The ffmpeg command line of an encode, in order and complete.
///
/// This is where the picture is decided, so it is a function of its own:
/// the test `the_arguments_of_a_plain_share_are_what_they_have_always_been`
/// holds it to what 3.10 produced. A feature that changes the chain has to
/// change that test with it, in the open, rather than by accident.
pub fn encode_args(e: &Encode<'_>) -> Result<Vec<String>> {
    let (job, enc) = (e.job, e.enc);
    let kbps = job.kbps;
    let mut args: Vec<String> = ["-nostats", "-progress", "pipe:1", "-y", "-v", "error"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut push = |v: &str| args.push(v.to_string());
    let threads = e.threads.to_string();
    let copy = job.mode == "copy";
    if !copy && enc.decode.is_empty() && e.threads > 0 {
        // decoder (dav1d takes every core otherwise)
        push("-threads");
        push(&threads);
    }
    if !copy {
        enc.global.iter().for_each(|v| push(v));
        enc.decode.iter().for_each(|v| push(v));
    }
    for v in [
        "-ss",
        &e.seek.to_string(),
        "-t",
        &job.seconds.to_string(),
        "-i",
        &e.input.to_string_lossy(),
    ] {
        push(v);
    }
    // a subtitle track is a second input; `-ss` and `-t` above belong to the
    // first one, so it comes in whole and is cut by the output's length
    if let Some((file, _)) = e.track {
        push("-i");
        push(file);
    }
    for v in ["-map", "0:v:0"] {
        push(v);
    }
    for v in audio_args(&job.audio).ok_or_else(|| anyhow!("unknown audio mode {}", job.audio))? {
        push(v);
    }
    if let Some((_, language)) = e.track {
        // MP4 carries timed text; marked as the default so a player that
        // honours it shows the subtitles without being asked
        for v in ["-map", "1:0", "-c:s", "mov_text"] {
            push(v);
        }
        if let Some(iso) = crate::subtitles::iso639_2(language) {
            push("-metadata:s:s:0");
            push(&format!("language={iso}"));
        }
        for v in ["-disposition:s:0", "default"] {
            push(v);
        }
    }
    if copy {
        // The OBS video stream as it is; audio only re-encoded when tracks are
        // mixed. The keyframe before `start` comes along, and its frames are
        // shifted to time zero instead of hidden behind an edit list, so every
        // player shows the same thing (the job says where the file really begins).
        for v in ["-c:v", "copy", "-avoid_negative_ts", "make_zero"] {
            push(v);
        }
        if job.audio == "mix" {
            for v in ["-c:a", "copy"] {
                push(v);
            }
        } else {
            for v in ["-c:a", "aac", "-b:a", "128k"] {
                push(v);
            }
        }
    } else {
        // since 2.7 the recording's resolution stays unless the target caps it;
        // the preview copy is 720p, a vertical cut its own crop
        let base_filter: Option<String> = if job.vertical {
            Some(vertical_filter(job.vertical_pos.unwrap_or(0.5)))
        } else if job.is_preview() {
            Some(enc.scale.replace("{h}", "720"))
        } else if job.max_height > 0 {
            Some(enc.scale.replace("{h}", &job.max_height.to_string()))
        } else {
            None
        };
        // Since 3.11: the subtitles go into the finished picture - after the
        // crop and the scale, so a 9:16 rendering carries them inside its own
        // frame instead of losing them with the sides, and before the upload,
        // because libass draws on frames in main memory.
        let base_filter = match e.burn {
            Some(file) => {
                let ass = format!("ass={file}:fontsdir=.");
                Some(match base_filter {
                    Some(b) => format!("{b},{ass}"),
                    None => ass,
                })
            }
            None => base_filter,
        };
        // plus the upload an encoder needs when the frames reach it in software
        if let Some(vf) = enc.filters(base_filter) {
            push("-vf");
            push(&vf);
        }
        push("-c:v");
        push(&enc.name);
        if e.threads > 0 {
            // encoder and filters
            push("-threads");
            push(&threads);
        }
        if kbps > 0 {
            // a bitrate cap: constant bitrate as before 2.7
            enc.opts.iter().for_each(|v| push(v));
            for v in [
                "-b:v",
                &format!("{kbps}k"),
                "-maxrate",
                &format!("{kbps}k"),
                "-bufsize",
                &format!("{}k", kbps * 2),
            ] {
                push(v);
            }
        } else {
            // best quality: the encoder's quality mode, no bitrate
            enc.quality.iter().for_each(|v| push(v));
        }
        if enc.pix_fmt {
            for v in ["-pix_fmt", "yuv420p"] {
                push(v);
            }
        }
        for v in ["-c:a", "aac", "-b:a", "128k"] {
            push(v);
        }
    }
    for v in ["-movflags", "+faststart", &e.out.to_string_lossy()] {
        push(v);
    }
    Ok(args)
}

/// The subtitle files of a rendering (since 3.11) and the folder ffmpeg is
/// started in so that it can name them without a path.
pub struct RenderSubs {
    pub dir: PathBuf,
    pub burn: bool,
    pub track: bool,
    pub language: String,
}

#[allow(clippy::too_many_arguments)]
async fn encode(
    state: &AppState,
    id: &str,
    job: &Job,
    input: &Path,
    seek: f64,
    out: &Path,
    token: &CancellationToken,
    enc: &crate::media::Encoder,
    subs: Option<&RenderSubs>,
) -> Result<()> {
    let runtime = state.runtime();
    let args = encode_args(&Encode {
        job,
        enc,
        threads: runtime.media.threads,
        input,
        seek,
        out,
        burn: subs.filter(|s| s.burn).map(|_| crate::subtitles::ASS_FILE),
        track: subs
            .filter(|s| s.track)
            .map(|s| (crate::subtitles::TRACK_FILE, s.language.as_str())),
    })?;

    // a scan-time preview must not compete with the game: idle priority
    let idle_media = job.idle.then(|| {
        runtime
            .media
            .clone()
            .with_resource_limits(crate::settings::FfmpegPriority::Idle, runtime.media.threads)
    });
    let mut cmd = idle_media
        .as_ref()
        .unwrap_or(&runtime.media)
        .ffmpeg_command();
    // the subtitle files are named without a path, so ffmpeg is started
    // where they are; everything else on the line is absolute
    if let Some(s) = subs {
        cmd.current_dir(&s.dir);
    }
    cmd.args(&args);
    run_with_progress(state, id, cmd, job.seconds, token)
        .await
        .inspect_err(|_| {
            let _ = std::fs::remove_file(out);
        })
}

/// Run an ffmpeg that reports `-progress` on stdout, turning `out_time_us`
/// into the job's percent, and give up on a timeout or a cancel. Shared by
/// the encode and, since 3.11, the transcription: both are one long ffmpeg
/// whose progress is the only thing the page has to look at.
///
/// The caller owns whatever ffmpeg was writing - this does not remove it.
async fn run_with_progress(
    state: &AppState,
    id: &str,
    mut cmd: tokio::process::Command,
    seconds: f64,
    token: &CancellationToken,
) -> Result<()> {
    let mut child = cmd.spawn().context("cannot start ffmpeg")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stdout missing"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("ffmpeg stderr missing"))?;
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf).await;
        String::from_utf8_lossy(&buf).trim().to_string()
    });

    let total_us = seconds * 1_000_000.0;
    let progress = async {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(us) = line
                .strip_prefix("out_time_us=")
                .and_then(|v| v.trim().parse::<f64>().ok())
            {
                if total_us > 0.0 {
                    let pct = ((us / total_us) * 100.0).floor().clamp(0.0, 99.0) as u8;
                    state.with_job(id, |j| j.percent = pct);
                }
            }
        }
    };
    tokio::select! {
        r = tokio::time::timeout(ENCODE_TIMEOUT, progress) => {
            if r.is_err() {
                let _ = child.kill().await;
                bail!("ffmpeg timed out after {} s", ENCODE_TIMEOUT.as_secs());
            }
        }
        _ = token.cancelled() => {
            let _ = child.kill().await;
            let _ = stderr_task.await;
            bail!("cancelled during encode");
        }
    }
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .context("ffmpeg did not exit")??;
    let err = stderr_task.await.unwrap_or_default();
    if !status.success() {
        if err.is_empty() {
            // ended from outside or crashed: "ffmpeg: exit code: 0xffffffff"
            // was all the card said (#46)
            bail!("ffmpeg stopped without saying why ({status})");
        }
        bail!("ffmpeg: {err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libx264 at best quality, the profile every machine falls back to.
    /// Written out rather than taken from the table so that this test says
    /// what it expects instead of asking the code.
    fn x264() -> crate::media::Encoder {
        crate::media::Encoder {
            label: "libx264",
            name: "libx264".into(),
            global: Vec::new(),
            decode: Vec::new(),
            scale: crate::media::SW_SCALE,
            upload: "",
            opts: vec!["-preset", "veryfast"],
            quality: vec!["-preset", "veryfast", "-crf", "18"],
            pix_fmt: true,
        }
    }

    fn a_share_job() -> Job {
        Job {
            id: "abcd1234".into(),
            base: "Replay A".into(),
            start: 6.0,
            end: 12.0,
            seconds: 6.0,
            audio: "mix".into(),
            mode: "h264".into(),
            stage: "encode".into(),
            ..Job::default()
        }
    }

    fn args_of(job: &Job, enc: &crate::media::Encoder) -> Vec<String> {
        encode_args(&Encode {
            job,
            enc,
            threads: 4,
            input: Path::new(r"C:\clips\.cuts\24af8830.mkv"),
            seek: 1.0,
            out: Path::new(r"C:\clips\shared\out.mp4"),
            burn: None,
            track: None,
        })
        .expect("the audio mode is known")
    }

    /// The invariant of R15: subtitles change nothing for a share that does
    /// not ask for them. This is the command line of 3.10, written out. If
    /// a change makes this test fail, the picture changed - say so in the
    /// changelog rather than adjusting the numbers.
    #[test]
    fn the_arguments_of_a_plain_share_are_what_they_have_always_been() {
        let expected: Vec<&str> = vec![
            "-nostats",
            "-progress",
            "pipe:1",
            "-y",
            "-v",
            "error",
            "-threads",
            "4",
            "-ss",
            "1",
            "-t",
            "6",
            "-i",
            "C:\\clips\\.cuts\\24af8830.mkv",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c:v",
            "libx264",
            "-threads",
            "4",
            "-preset",
            "veryfast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-movflags",
            "+faststart",
            "C:\\clips\\shared\\out.mp4",
        ];
        assert_eq!(args_of(&a_share_job(), &x264()), expected);
    }

    /// The same for the two shapes that do touch the chain today, so that a
    /// filter added later cannot slip in front of the crop unnoticed.
    #[test]
    fn a_vertical_share_crops_before_anything_else_and_a_copy_has_no_filter() {
        let mut vertical = a_share_job();
        vertical.vertical = true;
        vertical.vertical_pos = Some(0.5);
        vertical.max_height = 1080;
        let args = args_of(&vertical, &x264());
        let vf = args.iter().position(|a| a == "-vf").expect("a filter");
        assert_eq!(
            args[vf + 1],
            "crop=ih*9/16:ih:(iw-ih*9/16)*0.500:0,scale=1080:1920"
        );
        // the filter comes before the encoder, and the height cap loses to
        // the crop - a vertical share is 1080x1920 whatever the target caps
        assert!(args[vf + 2] == "-c:v");

        let mut copy = a_share_job();
        copy.mode = "copy".into();
        let args = args_of(&copy, &x264());
        assert!(!args.iter().any(|a| a == "-vf"), "copy re-encodes nothing");
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "copy"]));
    }

    /// Where the subtitles sit in the chain decides whether they survive a
    /// 9:16 rendering: after the crop and the scale they are inside the
    /// frame, before them they would be cut off with the sides.
    #[test]
    fn burned_in_subtitles_are_drawn_into_the_finished_picture() {
        let mut job = a_share_job();
        job.vertical = true;
        job.vertical_pos = Some(0.5);
        let args = encode_args(&Encode {
            job: &job,
            enc: &x264(),
            threads: 4,
            input: Path::new("in.mkv"),
            seek: 1.0,
            out: Path::new("out.mp4"),
            burn: Some("subs.ass"),
            track: None,
        })
        .unwrap();
        let vf = args.iter().position(|a| a == "-vf").expect("a filter");
        assert_eq!(
            args[vf + 1],
            "crop=ih*9/16:ih:(iw-ih*9/16)*0.500:0,scale=1080:1920,ass=subs.ass:fontsdir=.",
            "the crop comes first, then the subtitles"
        );
        // a plain share with no subtitles asked for has no filter at all
        let plain = args_of(&a_share_job(), &x264());
        assert!(!plain.iter().any(|a| a.contains("ass=")));
    }

    /// A track is a second input and a stream of its own; the picture is
    /// not touched, which is why it works in copy mode too.
    #[test]
    fn a_subtitle_track_rides_along_without_touching_the_picture() {
        let mut job = a_share_job();
        job.mode = "copy".into();
        let args = encode_args(&Encode {
            job: &job,
            enc: &x264(),
            threads: 4,
            input: Path::new("in.mkv"),
            seek: 1.0,
            out: Path::new("out.mp4"),
            burn: None,
            track: Some(("subs.srt", "de")),
        })
        .unwrap();
        assert!(args.windows(2).any(|w| w == ["-i", "subs.srt"]));
        assert!(args.windows(2).any(|w| w == ["-map", "1:0"]));
        assert!(args.windows(2).any(|w| w == ["-c:s", "mov_text"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-metadata:s:s:0", "language=deu"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-disposition:s:0", "default"]));
        assert!(!args.iter().any(|a| a == "-vf"), "copy touches no frame");
        // the second input comes after the first, so -ss and -t stay with it
        let first = args.iter().position(|a| a == "in.mkv").unwrap();
        let second = args.iter().position(|a| a == "subs.srt").unwrap();
        assert!(first < second);

        // a language nobody has a three-letter code for goes untagged
        let mut job = a_share_job();
        job.mode = "copy".into();
        let args = encode_args(&Encode {
            job: &job,
            enc: &x264(),
            threads: 4,
            input: Path::new("in.mkv"),
            seek: 1.0,
            out: Path::new("out.mp4"),
            burn: None,
            track: Some(("subs.srt", "")),
        })
        .unwrap();
        assert!(!args.iter().any(|a| a.starts_with("language=")));
    }

    #[test]
    fn an_unfinished_encode_has_a_name_of_its_own() {
        let out = Path::new("shared").join("WARDOGS_2026-09-11_23-10-05_11-71_9x16.mp4");
        let part = part_of(&out);
        assert_eq!(
            part.file_name().unwrap().to_string_lossy(),
            "WARDOGS_2026-09-11_23-10-05_11-71_9x16.part.mp4"
        );
        assert!(is_part(&part.file_name().unwrap().to_string_lossy()));
        assert!(is_part("x.PART.MP4"));
        assert!(!is_part(&out.file_name().unwrap().to_string_lossy()));
        // a title may end in "part" - that is still a finished output
        assert!(!is_part("Clip_12-30_Best-part.mp4"));
    }

    #[test]
    fn leftover_encodes_are_removed_and_outputs_stay() {
        let dir = std::env::temp_dir().join(format!("rc-part-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a.part.mp4", "a.mp4", "b_9x16.part.mp4", "notes.txt"] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        remove_unfinished_encodes(&dir);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(left, ["a.mp4", "notes.txt"]);
    }

    #[test]
    fn slug_matches_contract() {
        assert_eq!(slug("Dry run test"), "Dry-run-test");
        assert_eq!(slug("  Hallo, Welt!  "), "Hallo-Welt");
        assert_eq!(slug("Ärger über F9"), "Ärger-über-F9");
        assert_eq!(slug(""), "");
        assert_eq!(slug("---"), "");
        assert_eq!(slug(&"a".repeat(50)).len(), 40);
    }

    #[test]
    fn month_from_base_name() {
        assert_eq!(month_of("Replay 2026-09-04 11-40-00"), "2026-09");
        assert_eq!(month_of("clip"), "unsorted");
        assert_eq!(month_of("2026-09-04"), "2026-09");
    }

    #[test]
    fn share_file_names() {
        assert_eq!(
            share_file_name("Replay 2026-09-04 11-40-00", 2.0, 8.0, "Dry-run-test"),
            "Replay_2026-09-04_11-40-00_2-8_Dry-run-test.mp4"
        );
        assert_eq!(
            share_file_name("a  b", 219.0, 232.92, ""),
            "a_b_219-233.mp4"
        );
        assert_eq!(
            vertical_file_name("a_b_219-233.mp4"),
            "a_b_219-233_9x16.mp4"
        );
        // the counter of an output whose name is taken goes last, so the
        // 9:16 marker stays where it is
        assert_eq!(
            numbered_file_name("a_b_219-233_9x16.mp4", 2),
            "a_b_219-233_9x16_2.mp4"
        );
        assert_eq!(numbered_file_name("a_b_219-233", 3), "a_b_219-233_3");
    }

    #[test]
    fn vertical_filter_crops_a_9_16_window() {
        assert_eq!(
            vertical_filter(0.5),
            "crop=ih*9/16:ih:(iw-ih*9/16)*0.500:0,scale=1080:1920"
        );
        assert!(vertical_filter(7.0).contains("*1.000:0"));
        assert!(vertical_filter(-1.0).contains("*0.000:0"));
    }

    #[test]
    fn post_label_strips_prefix_only_as_a_word() {
        assert_eq!(
            post_label("WARDOGS", "WARDOGS 2026-09-04 23-26-58", ""),
            "2026-09-04 23-26-58"
        );
        assert_eq!(
            post_label("WARDOGS", "WARDOGS 2026-09-04", "Das gibt nen F9"),
            "Das gibt nen F9 - 2026-09-04"
        );
        assert_eq!(
            post_label("replaycut", "replaycut-test 2026-09-04", ""),
            "replaycut-test 2026-09-04"
        );
        assert_eq!(post_label("", "Replay 1", ""), "Replay 1");
    }

    #[test]
    fn audio_modes_known() {
        assert!(audio_args("mix").is_some());
        assert!(audio_args("gamediscord").is_some());
        assert!(audio_args("nope").is_none());
    }
}
