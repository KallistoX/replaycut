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

use crate::integrations::random_token;
use crate::platform;
use crate::state::{AppState, Job, AUDIO_MODES, MAX_QUEUE};
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
    /// The same share is already running or waiting; carries its id.
    Busy(String),
    Invalid(String),
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

/// Validate and register a job. Holds the state lock for the whole check so
/// two concurrent requests cannot both pass the busy check.
/// Validate and register a share. Returns the job id and its place in the
/// queue (0 = runs at once; the caller spawns `run` for it).
pub fn start(state: &AppState, req: ShareRequest) -> Result<(String, usize), ShareError> {
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
    let job = Job {
        id: id.clone(),
        base: clip.base.clone(),
        target,
        start,
        end,
        seconds,
        audio,
        mode: share_mode,
        kbps: state.settings().share_kbps,
        vertical: req.vertical,
        vertical_pos,
        stage: "queued".into(),
        percent: 0,
        at: util::now_local(),
        ..Job::default()
    };
    let position = state.register_job(&mut inner, job);
    Ok((id, position))
}

/// `POST /api/jobs/<id>/publish` (since 2.5): send the file of a finished
/// job to another target without cutting again. Returns id and position.
pub fn publish(
    state: &AppState,
    source: &str,
    target: &str,
) -> Result<(String, usize), ShareError> {
    let mut inner = state.inner.lock();
    let src = inner
        .jobs
        .get(source)
        .cloned()
        .ok_or_else(|| ShareError::UnknownJob(source.to_string()))?;
    let Some(file) = src.file.clone().filter(|_| src.ok == Some(true)) else {
        return Err(ShareError::Invalid(
            "the source job has no finished file".to_string(),
        ));
    };
    if !state.paths().shared_dir.join(&file).is_file() {
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
        base: src.base.clone(),
        target,
        source: Some(source.to_string()),
        start: src.start,
        end: src.end,
        seconds: src.seconds,
        audio: src.audio.clone(),
        mode: src.mode.clone(),
        kbps: src.kbps,
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
    let position = state.register_job(&mut inner, job);
    Ok((id, position))
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
    let result = pipeline(&state, &id, &token).await;
    if let Err(e) = &result {
        if token.is_cancelled() {
            tracing::info!("share [{id}] cancelled");
        } else {
            tracing::error!("share [{id}] failed: {e:#}");
        }
    }
    let next = state.complete_job(&id, result.map_err(|e| format!("{e:#}")));
    if let Some(job) = state.job(&id) {
        if !job.cancelled {
            let uploaded = job.direct.is_some();
            toast::show(&state, Toast::share_result(&job, uploaded, &state.ui_url()));
        }
    }
    state.cancels.lock().remove(&id);
    if let Some(next) = next {
        tokio::spawn(run(state, next));
    }
}

async fn pipeline(state: &AppState, id: &str, token: &CancellationToken) -> Result<()> {
    let job = state.job(id).ok_or_else(|| anyhow!("job vanished"))?;
    // The runtime of the moment the job started; a settings change while
    // it runs does not swap integrations or encoder under its feet.
    let runtime = state.runtime();
    let settings = state.settings();
    // A publish job (since 2.5) re-uses the file of its source; a share cuts one.
    let republish = job.source.is_some();
    let (clip_path, title) = if republish {
        (PathBuf::new(), job.title.clone().unwrap_or_default())
    } else {
        let inner = state.inner.lock();
        let clip = inner
            .clips
            .get(&job.base)
            .ok_or_else(|| anyhow!("unknown clip: {}", job.base))?;
        (
            PathBuf::from(&clip.path),
            inner.names.get(&job.base).cloned().unwrap_or_default(),
        )
    };
    let file_name = match &job.file {
        Some(f) if republish => f.clone(),
        _ => {
            let name = share_file_name(&job.base, job.start, job.end, &slug(&title));
            if job.vertical {
                vertical_file_name(&name)
            } else {
                name
            }
        }
    };
    let out = state.paths().shared_dir.join(&file_name);
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
    tracing::info!(
        "share [{id}]: {} {}-{} s ({} s) {}{}, audio '{mode_label}' -> {file_name}",
        job.base,
        job.start,
        job.end,
        job.seconds,
        if job.mode == "copy" {
            "copy (no re-encode)".to_string()
        } else {
            format!("@ {} kbps", job.kbps)
        },
        if job.vertical {
            format!(" vertical 9:16 at {:.2}", job.vertical_pos.unwrap_or(0.5))
        } else {
            String::new()
        }
    );

    // encode (skipped when the file already exists from the source job)
    if !republish {
        state.with_job(id, |j| {
            j.stage = "encode".into();
            j.title = Some(title.clone());
        });
        let started = Instant::now();
        // The GPU path may fail on a driver quirk: try once more with software
        // decoding and CPU scaling before giving up (since 2.4).
        // A vertical cut crops on the CPU, so frames that a GPU filter would
        // keep on the card (cuda, qsv) are decoded in software instead.
        let profile = if job.vertical && runtime.encoder.filter != crate::media::SW_SCALE {
            runtime.encoder.software_fallback()
        } else {
            runtime.encoder.clone()
        };
        if let Err(e) = encode(state, id, &job, &clip_path, &out, token, &profile).await {
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
            let _ = std::fs::remove_file(&out);
            encode(
                state,
                id,
                &job,
                &clip_path,
                &out,
                token,
                &profile.software_fallback(),
            )
            .await?;
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

    // notify: every integration that posts automatically (stage `notify` since 2.5)
    if let Some(direct) = direct {
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
                        format!("post failed: {e}")
                    }
                };
                tracing::info!("share [{id}]: {}: {status}", entry.label);
                statuses.push(if notifies_len_is_one(&runtime) {
                    status
                } else {
                    format!("{}: {status}", entry.label)
                });
            }
            state.with_job(id, |j| j.discord = Some(statuses.join(" · ")));
        }
    }
    Ok(())
}

fn notifies_len_is_one(runtime: &crate::state::Runtime) -> bool {
    runtime.integrations.auto_notifies().count() == 1
}

async fn encode(
    state: &AppState,
    id: &str,
    job: &Job,
    input: &Path,
    out: &Path,
    token: &CancellationToken,
    enc: &crate::media::Encoder,
) -> Result<()> {
    let kbps = job.kbps;
    let (b, maxrate, bufsize) = (
        format!("{kbps}k"),
        format!("{kbps}k"),
        format!("{}k", kbps * 2),
    );
    let (start, seconds) = (job.start.to_string(), job.seconds.to_string());
    let input_s = input.to_string_lossy().into_owned();
    let out_s = out.to_string_lossy().into_owned();
    let vf = if job.vertical {
        vertical_filter(job.vertical_pos.unwrap_or(0.5))
    } else {
        enc.filter.to_string()
    };
    let mut args: Vec<&str> = vec!["-nostats", "-progress", "pipe:1", "-y", "-v", "error"];
    let runtime = state.runtime();
    let threads = runtime.media.threads.to_string();
    let copy = job.mode == "copy";
    if !copy && enc.decode.is_empty() && runtime.media.threads > 0 {
        args.extend(["-threads", &threads]); // decoder (dav1d takes every core otherwise)
    }
    if !copy {
        args.extend(enc.decode.iter().copied());
    }
    args.extend([
        "-ss", &start, "-t", &seconds, "-i", &input_s, "-map", "0:v:0",
    ]);
    args.extend(audio_args(&job.audio).ok_or_else(|| anyhow!("unknown audio mode {}", job.audio))?);
    if copy {
        // The OBS video stream as it is; audio only re-encoded when tracks are
        // mixed. The keyframe before `start` comes along, and its frames are
        // shifted to time zero instead of hidden behind an edit list, so every
        // player shows the same thing (the job says where the file really begins).
        args.extend(["-c:v", "copy", "-avoid_negative_ts", "make_zero"]);
        if job.audio == "mix" {
            args.extend(["-c:a", "copy"]);
        } else {
            args.extend(["-c:a", "aac", "-b:a", "128k"]);
        }
    } else {
        args.extend(["-vf", &vf, "-c:v", &enc.name]);
        if runtime.media.threads > 0 {
            args.extend(["-threads", &threads]); // encoder and filters
        }
        args.extend(enc.opts.iter().copied());
        args.extend(["-b:v", &b, "-maxrate", &maxrate, "-bufsize", &bufsize]);
        if enc.pix_fmt {
            args.extend(["-pix_fmt", "yuv420p"]);
        }
        args.extend(["-c:a", "aac", "-b:a", "128k"]);
    }
    args.extend(["-movflags", "+faststart", &out_s]);

    let mut cmd = runtime.media.ffmpeg_command();
    cmd.args(&args);
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

    let total_us = job.seconds * 1_000_000.0;
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
                let _ = std::fs::remove_file(out);
                bail!("ffmpeg timed out after {} s", ENCODE_TIMEOUT.as_secs());
            }
        }
        _ = token.cancelled() => {
            let _ = child.kill().await;
            let _ = stderr_task.await;
            let _ = std::fs::remove_file(out);
            bail!("cancelled during encode");
        }
    }
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .context("ffmpeg did not exit")??;
    let err = stderr_task.await.unwrap_or_default();
    if !status.success() {
        bail!(
            "ffmpeg: {}",
            if err.is_empty() {
                status.to_string()
            } else {
                err
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
