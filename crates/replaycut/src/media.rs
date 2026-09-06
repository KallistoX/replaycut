//! ffmpeg and ffprobe: locating the binaries, encoder detection, preview
//! remux, probing. Every process runs without a console window.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

use crate::settings::FfmpegPriority;

#[derive(Debug, Clone)]
pub struct Media {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Windows priority class passed as a process creation flag (0 = inherit).
    #[cfg_attr(not(windows), allow(dead_code))]
    priority_flag: u32,
    /// `-threads` for encodes; 0 = leave it to ffmpeg.
    pub threads: u32,
}

/// Process creation flag for a priority class.
pub fn priority_flag(priority: FfmpegPriority) -> u32 {
    match priority {
        FfmpegPriority::Normal => 0x0000_0020,
        FfmpegPriority::BelowNormal => 0x0000_4000,
        FfmpegPriority::Idle => 0x0000_0040,
    }
}

/// How a share is encoded: the H.264 encoder plus, since 2.4, the decode
/// options and the scale filter that go with it (the "full GPU path" keeps
/// decode, scaling and encode on the card where ffmpeg can).
#[derive(Debug, Clone)]
pub struct Encoder {
    /// Short name of the profile, for the log and the diagnostics.
    pub label: &'static str,
    pub name: String,
    /// Input options before `-i` (`-hwaccel ...`), empty for software decoding.
    pub decode: Vec<&'static str>,
    /// The scale filter of the profile with `{h}` for the height; only used
    /// when a target limits the height (since 2.7 shares keep the
    /// recording's resolution).
    pub scale: &'static str,
    /// Preset plus rate control for a bitrate cap (`-b:v` follows).
    pub opts: Vec<&'static str>,
    /// Preset plus quality-driven rate control (since 2.7, the default: no
    /// bitrate, the encoder spends what the picture needs).
    pub quality: Vec<&'static str>,
    /// `-pix_fmt yuv420p` on the encoder input (software frames only).
    pub pix_fmt: bool,
}

impl Encoder {
    /// A hardware decode or GPU filter is in play, so a share may fall back.
    pub fn is_gpu_path(&self) -> bool {
        !self.decode.is_empty() || self.gpu_frames()
    }

    /// The frames stay on the card (cuda, qsv): CPU filters cannot touch them.
    pub fn gpu_frames(&self) -> bool {
        self.scale != SW_SCALE
    }

    /// The `-vf` value that scales to `height`.
    pub fn filter_for(&self, height: u32) -> String {
        self.scale.replace("{h}", &height.to_string())
    }

    /// The same encoder with software decoding and CPU scaling.
    pub fn software_fallback(&self) -> Self {
        Self {
            label: "fallback",
            name: self.name.clone(),
            decode: Vec::new(),
            scale: SW_SCALE,
            opts: self.opts.clone(),
            quality: self.quality.clone(),
            pix_fmt: true,
        }
    }

    /// What the diagnostics show: `h264_amf (amf-d3d11: d3d11va decode)`.
    pub fn describe(&self) -> String {
        let decode = self
            .decode
            .iter()
            .skip(1)
            .step_by(2)
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        if self.decode.is_empty() {
            format!("{} ({}: software decode)", self.name, self.label)
        } else {
            format!("{} ({}: {decode} decode)", self.name, self.label)
        }
    }
}

pub const SW_SCALE: &str = "scale=-2:{h}";

/// One candidate of the detection, in preference order per vendor: the
/// full GPU path first, then the same encoder with software decoding.
struct Profile {
    label: &'static str,
    encoder: &'static str,
    decode: &'static [&'static str],
    scale: &'static str,
    opts: &'static [&'static str],
    quality: &'static [&'static str],
    pix_fmt: bool,
}

// Bitrate-capped rate control, for targets with a limit (and the test encodes).
const AMF_OPTS: &[&str] = &["-quality", "quality", "-rc", "cbr"];
const NVENC_OPTS: &[&str] = &["-preset", "p5", "-rc", "cbr"];
const QSV_OPTS: &[&str] = &["-preset", "medium"];
const X264_OPTS: &[&str] = &["-preset", "veryfast"];
// Quality-driven rate control (since 2.7): one fixed step per encoder that
// looks like the recording; the size follows the picture.
const AMF_QUALITY: &[&str] = &[
    "-quality", "quality", "-rc", "cqp", "-qp_i", "18", "-qp_p", "20", "-qp_b", "22",
];
const NVENC_QUALITY: &[&str] = &["-preset", "p5", "-rc", "vbr", "-cq", "19", "-b:v", "0"];
const QSV_QUALITY: &[&str] = &["-preset", "medium", "-global_quality", "20"];
const X264_QUALITY: &[&str] = &["-preset", "veryfast", "-crf", "18"];

const PROFILES: [Profile; 7] = [
    // The winget ffmpeg has no scale_amf: decode on the GPU, ffmpeg brings the
    // frames back on its own (no hwaccel_output_format), scale on the CPU.
    Profile {
        label: "amf-d3d11",
        encoder: "h264_amf",
        decode: &["-hwaccel", "d3d11va"],
        scale: SW_SCALE,
        opts: AMF_OPTS,
        quality: AMF_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "amf",
        encoder: "h264_amf",
        decode: &[],
        scale: SW_SCALE,
        opts: AMF_OPTS,
        quality: AMF_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "nvenc-cuda",
        encoder: "h264_nvenc",
        decode: &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
        scale: "scale_cuda=-2:{h}",
        opts: NVENC_OPTS,
        quality: NVENC_QUALITY,
        pix_fmt: false,
    },
    Profile {
        label: "nvenc",
        encoder: "h264_nvenc",
        decode: &[],
        scale: SW_SCALE,
        opts: NVENC_OPTS,
        quality: NVENC_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "qsv-full",
        encoder: "h264_qsv",
        decode: &["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"],
        scale: "scale_qsv=-1:{h}",
        opts: QSV_OPTS,
        quality: QSV_QUALITY,
        pix_fmt: false,
    },
    Profile {
        label: "qsv",
        encoder: "h264_qsv",
        decode: &[],
        scale: SW_SCALE,
        opts: QSV_OPTS,
        quality: QSV_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "libx264",
        encoder: "libx264",
        decode: &[],
        scale: SW_SCALE,
        opts: X264_OPTS,
        quality: X264_QUALITY,
        pix_fmt: true,
    },
];

/// The `hwaccel` setting: `auto` (or empty) takes the profile's decode,
/// `none` forces software decoding, anything else is passed to ffmpeg as
/// `-hwaccel <value>` with CPU scaling.
pub fn hwaccel_mode(setting: &str) -> HwAccel {
    match setting.trim() {
        "" | "auto" => HwAccel::Auto,
        "none" => HwAccel::None,
        other => HwAccel::Manual(other.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HwAccel {
    Auto,
    None,
    Manual(String),
}

/// Values `hwaccel` accepts in settings.json.
pub const HWACCEL_VALUES: [&str; 6] = ["", "auto", "none", "cuda", "d3d11va", "qsv"];

/// The newest preview in the clip folder, the sample for testing a GPU path
/// (lavfi sources cannot exercise a hardware decoder).
pub fn newest_preview(clip_dir: &Path) -> Option<PathBuf> {
    let dir = clip_dir.join(".preview");
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("mp4"))
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

impl Media {
    /// `ffmpeg` on the PATH, else the winget install location on Windows.
    pub fn locate() -> Result<Self> {
        let exe = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let mut found = std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join(exe))
                    .find(|f| f.is_file())
            })
            .unwrap_or(None);
        if found.is_none() {
            if let Some(local) = std::env::var_os("LOCALAPPDATA") {
                let packages = PathBuf::from(local)
                    .join("Microsoft")
                    .join("WinGet")
                    .join("Packages");
                found = winget_ffmpeg(&packages);
            }
        }
        let ffmpeg =
            found.ok_or_else(|| anyhow!("ffmpeg not found - install it and put it on the PATH"))?;
        let ffprobe = ffmpeg.with_file_name(if cfg!(windows) {
            "ffprobe.exe"
        } else {
            "ffprobe"
        });
        anyhow::ensure!(
            ffprobe.is_file(),
            "ffprobe not found next to {}",
            ffmpeg.display()
        );
        Ok(Self {
            ffmpeg,
            ffprobe,
            priority_flag: 0,
            threads: 0,
        })
    }

    /// Run every ffmpeg/ffprobe process at this priority and encode with this many threads.
    pub fn with_resource_limits(mut self, priority: FfmpegPriority, threads: u32) -> Self {
        self.priority_flag = priority_flag(priority);
        self.threads = threads;
        self
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW | self.priority_flag);
        cmd
    }

    async fn run(&self, exe: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
        let mut cmd = self.command(exe);
        cmd.args(args);
        let out = tokio::time::timeout(timeout, cmd.output())
            .await
            .map_err(|_| anyhow!("{} timed out after {timeout:?}", exe.display()))?
            .with_context(|| format!("cannot start {}", exe.display()))?;
        Ok(out)
    }

    /// A configured ffmpeg command for callers that stream its output themselves.
    pub fn ffmpeg_command(&self) -> Command {
        self.command(&self.ffmpeg)
    }

    pub async fn ffmpeg(&self, args: &[&str], timeout: Duration) -> Result<Output> {
        self.run(&self.ffmpeg, args, timeout).await
    }

    async fn ffprobe(&self, args: &[&str]) -> Result<String> {
        let out = self
            .run(&self.ffprobe, args, Duration::from_secs(60))
            .await?;
        if !out.status.success() {
            bail!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Probe each candidate with a real short encode: the ffmpeg build knows
    /// all hardware encoders even without the matching GPU. `sample` (the
    /// newest preview) lets the GPU decode paths prove themselves; without
    /// one only the software-decode profiles are tried.
    pub async fn detect_encoder(
        &self,
        preferred: &str,
        hwaccel: &HwAccel,
        sample: Option<&Path>,
    ) -> Result<Encoder> {
        let have = self
            .ffmpeg(&["-hide_banner", "-encoders"], Duration::from_secs(60))
            .await?;
        let have = String::from_utf8_lossy(&have.stdout).into_owned();
        let known = |name: &str| {
            have.lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
        };
        let auto = preferred.is_empty() || preferred == "auto";
        let mut candidates: Vec<Encoder> = PROFILES
            .iter()
            .filter(|p| auto || p.encoder == preferred)
            .filter(|p| match hwaccel {
                HwAccel::Auto => true,
                _ => p.decode.is_empty() && p.scale == SW_SCALE,
            })
            .filter(|p| p.decode.is_empty() || sample.is_some())
            .map(|p| Encoder {
                label: p.label,
                name: p.encoder.to_string(),
                decode: p.decode.to_vec(),
                scale: p.scale,
                opts: p.opts.to_vec(),
                quality: p.quality.to_vec(),
                pix_fmt: p.pix_fmt,
            })
            .collect();
        if !auto && candidates.is_empty() {
            // an encoder we have no profile for: bare, as the user asked
            candidates.push(Encoder {
                label: "custom",
                name: preferred.to_string(),
                decode: Vec::new(),
                scale: SW_SCALE,
                opts: Vec::new(),
                quality: Vec::new(),
                pix_fmt: true,
            });
        }
        if let HwAccel::Manual(value) = hwaccel {
            let leaked: &'static str = Box::leak(value.clone().into_boxed_str());
            for c in &mut candidates {
                c.decode = vec!["-hwaccel", leaked];
            }
        }
        let mut tried = Vec::new();
        for enc in candidates {
            tried.push(enc.label.to_string());
            if !known(&enc.name) {
                continue;
            }
            let out = self.test_encode(&enc, sample).await?;
            if out.status.success() {
                tracing::info!("encoder: {}", enc.describe());
                return Ok(enc);
            }
            let first = String::from_utf8_lossy(&out.stderr);
            let first = first
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            tracing::warn!("encoder profile {} not usable: {first}", enc.label);
        }
        bail!(
            "no usable H.264 encoder found (tried: {})",
            tried.join(", ")
        )
    }

    /// Two seconds through the whole path (decode, scale, encode) into
    /// `-f null`, or a synthetic source when no sample exists.
    async fn test_encode(&self, enc: &Encoder, sample: Option<&Path>) -> Result<Output> {
        let mut args: Vec<&str> = vec!["-v", "error"];
        let sample_s = sample.map(|p| p.to_string_lossy().into_owned());
        match &sample_s {
            Some(s) => {
                args.extend(enc.decode.iter().copied());
                args.extend(["-t", "2", "-i", s, "-map", "0:v:0", "-an"]);
            }
            None => args.extend([
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=1280x720:rate=30:duration=0.5",
            ]),
        }
        let vf = enc.filter_for(1080);
        args.extend(["-vf", &vf, "-c:v", &enc.name]);
        args.extend(enc.opts.iter().copied());
        args.extend(["-b:v", "2000k"]);
        if enc.pix_fmt {
            args.extend(["-pix_fmt", "yuv420p"]);
        }
        args.extend(["-f", "null", "-"]);
        self.ffmpeg(&args, Duration::from_secs(90)).await
    }

    /// `replaycut bench`: every profile the build knows, on `seconds` of the
    /// newest clip, with wall time, CPU time and output size.
    pub async fn bench(&self, clip: &Path, seconds: u32) -> Result<Vec<BenchRow>> {
        let have = self
            .ffmpeg(&["-hide_banner", "-encoders"], Duration::from_secs(60))
            .await?;
        let have = String::from_utf8_lossy(&have.stdout).into_owned();
        let mut rows = Vec::new();
        let clip_s = clip.to_string_lossy().into_owned();
        let secs = seconds.to_string();
        for p in PROFILES.iter() {
            if !have
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(p.encoder))
            {
                continue;
            }
            let out = std::env::temp_dir().join(format!("replaycut-bench-{}.mp4", p.label));
            let out_s = out.to_string_lossy().into_owned();
            let mut args: Vec<&str> = vec!["-y", "-v", "error"];
            args.extend(p.decode.iter().copied());
            args.extend(["-t", &secs, "-i", &clip_s, "-map", "0:v:0", "-an"]);
            let vf = p.scale.replace("{h}", "1080");
            args.extend(["-vf", &vf, "-c:v", p.encoder]);
            args.extend(p.opts.iter().copied());
            args.extend(["-b:v", "6000k", "-maxrate", "6000k", "-bufsize", "12000k"]);
            if p.pix_fmt {
                args.extend(["-pix_fmt", "yuv420p"]);
            }
            args.push(&out_s);
            let started = std::time::Instant::now();
            let (ok, cpu, err) = run_timed(&self.ffmpeg, &args);
            let wall = started.elapsed().as_secs_f64();
            let size_mb = std::fs::metadata(&out)
                .map(|m| m.len() as f64 / 1_048_576.0)
                .unwrap_or(0.0);
            let _ = std::fs::remove_file(&out);
            rows.push(BenchRow {
                label: p.label,
                encoder: p.encoder,
                ok,
                wall,
                cpu,
                size_mb,
                error: err,
            });
        }
        Ok(rows)
    }

    /// Preview = original video plus audio track 1, remuxed with faststart.
    pub async fn remux_preview(&self, mkv: &Path, out: &Path) -> Result<()> {
        let mkv_s = mkv.to_string_lossy();
        let out_s = out.to_string_lossy();
        let args = [
            "-y",
            "-v",
            "error",
            "-i",
            &mkv_s,
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            &out_s,
        ];
        let res = self.ffmpeg(&args, Duration::from_secs(120)).await?;
        if !res.status.success() {
            let _ = std::fs::remove_file(out);
            bail!(
                "ffmpeg remux: {}",
                String::from_utf8_lossy(&res.stderr).trim()
            );
        }
        Ok(())
    }

    /// One JPEG frame of the preview at `at` seconds, 320 px wide (since 2.4).
    pub async fn thumbnail(&self, preview: &Path, out: &Path, at: f64) -> Result<()> {
        let at_s = format!("{at:.2}");
        let preview_s = preview.to_string_lossy();
        let out_s = out.to_string_lossy();
        let args = [
            "-y",
            "-v",
            "error",
            "-ss",
            &at_s,
            "-i",
            &preview_s,
            "-frames:v",
            "1",
            "-vf",
            "scale=320:-2",
            "-q:v",
            "4",
            &out_s,
        ];
        let res = self.ffmpeg(&args, Duration::from_secs(60)).await?;
        if !res.status.success() || !out.is_file() {
            let _ = std::fs::remove_file(out);
            bail!(
                "ffmpeg thumbnail: {}",
                String::from_utf8_lossy(&res.stderr).trim()
            );
        }
        Ok(())
    }

    /// Container duration in seconds, rounded to two decimals.
    pub async fn duration(&self, path: &Path) -> Result<f64> {
        let p = path.to_string_lossy();
        let out = self
            .ffprobe(&[
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
                &p,
            ])
            .await?;
        let d: f64 = out
            .trim()
            .parse()
            .with_context(|| format!("ffprobe duration {out:?}"))?;
        Ok((d * 100.0).round() / 100.0)
    }

    /// Codec, size and frame rate of the first video stream; empty values
    /// when probing fails (the clip still works, the hints are missing).
    pub async fn video_info(&self, path: &Path) -> VideoInfo {
        let p = path.to_string_lossy();
        match self
            .ffprobe(&[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=codec_name,width,height,r_frame_rate",
                "-of",
                "csv=p=0",
                &p,
            ])
            .await
        {
            Ok(out) => VideoInfo::parse(&out),
            Err(e) => {
                tracing::warn!("cannot probe the video of {}: {e}", path.display());
                VideoInfo::default()
            }
        }
    }

    /// Number of audio streams; 1 when probing fails (as in 1.4).
    pub async fn audio_tracks(&self, path: &Path) -> u32 {
        let p = path.to_string_lossy();
        match self
            .ffprobe(&[
                "-v",
                "error",
                "-select_streams",
                "a",
                "-show_entries",
                "stream=index",
                "-of",
                "csv=p=0",
                &p,
            ])
            .await
        {
            Ok(out) => out.lines().filter(|l| !l.trim().is_empty()).count().max(1) as u32,
            Err(e) => {
                tracing::warn!("cannot count audio tracks of {}: {e}", path.display());
                1
            }
        }
    }
}

/// One line of `replaycut bench`.
#[derive(Debug)]
pub struct BenchRow {
    pub label: &'static str,
    pub encoder: &'static str,
    pub ok: bool,
    pub wall: f64,
    /// Kernel plus user time of the ffmpeg process, seconds (Windows only).
    pub cpu: Option<f64>,
    pub size_mb: f64,
    pub error: String,
}

/// Run ffmpeg to completion and report success, its CPU time and the first
/// error line. Blocking on purpose: the bench is a console command.
fn run_timed(exe: &Path, args: &[&str]) -> (bool, Option<f64>, String) {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (false, None, format!("cannot start ffmpeg: {e}")),
    };
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut s) = stderr {
            use std::io::Read;
            let _ = s.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => return (false, None, format!("ffmpeg: {e}")),
    };
    let cpu = process_cpu_seconds(&child);
    let err = reader
        .join()
        .unwrap_or_default()
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    (status.success(), cpu, err)
}

/// Kernel plus user time of a finished child, while its handle is still open.
#[cfg(windows)]
fn process_cpu_seconds(child: &std::process::Child) -> Option<f64> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::System::Threading::GetProcessTimes;
    let handle = HANDLE(child.as_raw_handle());
    let (mut c, mut e, mut k, mut u) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: the handle belongs to `child`, which outlives this call.
    unsafe { GetProcessTimes(handle, &mut c, &mut e, &mut k, &mut u) }.ok()?;
    let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
    Some((ticks(k) + ticks(u)) as f64 / 10_000_000.0)
}

#[cfg(not(windows))]
fn process_cpu_seconds(_child: &std::process::Child) -> Option<f64> {
    None
}

/// What the setup wizard and the OBS page say about a clip's video.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoInfo {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

impl VideoInfo {
    /// ffprobe csv line: `h264,1920,1080,60/1` (fields may be missing or `N/A`).
    pub fn parse(csv: &str) -> Self {
        let line = csv.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let f: Vec<&str> = line.trim().split(',').collect();
        let num = |i: usize| {
            f.get(i)
                .and_then(|s| s.trim().parse::<u32>().ok())
                .unwrap_or(0)
        };
        let fps = f
            .get(3)
            .and_then(|s| {
                let s = s.trim();
                match s.split_once('/') {
                    Some((a, b)) => {
                        let (a, b) = (a.parse::<f64>().ok()?, b.parse::<f64>().ok()?);
                        (b > 0.0).then(|| a / b)
                    }
                    None => s.parse::<f64>().ok(),
                }
            })
            .map(|v| (v * 100.0).round() / 100.0)
            .unwrap_or(0.0);
        Self {
            codec: f.first().map(|s| s.trim().to_string()).unwrap_or_default(),
            width: num(1),
            height: num(2),
            fps,
        }
    }
}

fn winget_ffmpeg(packages: &Path) -> Option<PathBuf> {
    let dirs = std::fs::read_dir(packages).ok()?;
    for pkg in dirs.flatten() {
        if !pkg.file_name().to_string_lossy().starts_with("Gyan.FFmpeg") {
            continue;
        }
        for sub in std::fs::read_dir(pkg.path()).ok()?.flatten() {
            let candidate = sub.path().join("bin").join("ffmpeg.exe");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::VideoInfo;

    #[test]
    fn video_info_parses_ffprobe_csv() {
        let v = VideoInfo::parse("h264,1920,1080,60/1\n");
        assert_eq!(v.codec, "h264");
        assert_eq!((v.width, v.height), (1920, 1080));
        assert_eq!(v.fps, 60.0);
        let v = VideoInfo::parse("av1,2560,1440,60000/1001");
        assert_eq!(v.fps, 59.94);
        let v = VideoInfo::parse("hevc,N/A,N/A,0/0");
        assert_eq!(v.codec, "hevc");
        assert_eq!((v.width, v.height, v.fps), (0, 0, 0.0));
        assert_eq!(VideoInfo::parse(""), VideoInfo::default());
    }
}
