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
    /// Priority of every ffmpeg/ffprobe process; `None` inherits ours.
    priority: Option<FfmpegPriority>,
    /// `-threads` for encodes; 0 = leave it to ffmpeg.
    pub threads: u32,
}

/// Windows process creation flag for a priority class.
#[cfg(windows)]
pub fn priority_flag(priority: FfmpegPriority) -> u32 {
    match priority {
        FfmpegPriority::Normal => 0x0000_0020,
        FfmpegPriority::BelowNormal => 0x0000_4000,
        FfmpegPriority::Idle => 0x0000_0040,
    }
}

/// Unix nice level for a priority class: `normal` inherits, `belowNormal`
/// yields to anything at the default level (the game), `idle` takes only
/// what nobody else wants.
#[cfg(not(windows))]
pub fn nice_level(priority: FfmpegPriority) -> i32 {
    match priority {
        FfmpegPriority::Normal => 0,
        FfmpegPriority::BelowNormal => 10,
        FfmpegPriority::Idle => 19,
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
    /// Global options that name the device (`-vaapi_device ...`), empty for
    /// encoders that find theirs on their own.
    pub global: Vec<&'static str>,
    /// Input options before `-i` (`-hwaccel ...`), empty for software decoding.
    pub decode: Vec<&'static str>,
    /// The scale filter of the profile with `{h}` for the height; only used
    /// when a target limits the height (since 2.7 shares keep the
    /// recording's resolution).
    pub scale: &'static str,
    /// The filter that hands software frames to an encoder that only takes
    /// frames on the card (`format=nv12,hwupload` for VAAPI); empty when the
    /// encoder takes software frames itself.
    pub upload: &'static str,
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
        self.filters(Some(self.scale.replace("{h}", &height.to_string())))
            .unwrap_or_default()
    }

    /// The `-vf` value for a share: `base` (a scale or a crop, in software
    /// or on the card) plus the upload the encoder needs when the frames
    /// reach it in software; `None` when nothing has to happen.
    pub fn filters(&self, base: Option<String>) -> Option<String> {
        let upload = (!self.gpu_frames() && !self.upload.is_empty()).then_some(self.upload);
        match (base, upload) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(u)) => Some(u.to_string()),
            (Some(b), Some(u)) => Some(format!("{b},{u}")),
        }
    }

    /// The same encoder with software decoding and CPU scaling.
    pub fn software_fallback(&self) -> Self {
        Self {
            label: "fallback",
            name: self.name.clone(),
            global: self.global.clone(),
            decode: Vec::new(),
            scale: SW_SCALE,
            upload: self.upload,
            opts: self.opts.clone(),
            quality: self.quality.clone(),
            pix_fmt: self.upload.is_empty(),
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
#[derive(Clone, Copy)]
struct Profile {
    label: &'static str,
    encoder: &'static str,
    global: &'static [&'static str],
    decode: &'static [&'static str],
    scale: &'static str,
    upload: &'static str,
    opts: &'static [&'static str],
    quality: &'static [&'static str],
    pix_fmt: bool,
}

impl Profile {
    fn encoder(&self) -> Encoder {
        Encoder {
            label: self.label,
            name: self.encoder.to_string(),
            global: self.global.to_vec(),
            decode: self.decode.to_vec(),
            scale: self.scale,
            upload: self.upload,
            opts: self.opts.to_vec(),
            quality: self.quality.to_vec(),
            pix_fmt: self.pix_fmt,
        }
    }
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
        global: &[],
        decode: &["-hwaccel", "d3d11va"],
        scale: SW_SCALE,
        upload: "",
        opts: AMF_OPTS,
        quality: AMF_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "amf",
        encoder: "h264_amf",
        global: &[],
        decode: &[],
        scale: SW_SCALE,
        upload: "",
        opts: AMF_OPTS,
        quality: AMF_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "nvenc-cuda",
        encoder: "h264_nvenc",
        global: &[],
        decode: &["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"],
        scale: "scale_cuda=-2:{h}",
        upload: "",
        opts: NVENC_OPTS,
        quality: NVENC_QUALITY,
        pix_fmt: false,
    },
    Profile {
        label: "nvenc",
        encoder: "h264_nvenc",
        global: &[],
        decode: &[],
        scale: SW_SCALE,
        upload: "",
        opts: NVENC_OPTS,
        quality: NVENC_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "qsv-full",
        encoder: "h264_qsv",
        global: &[],
        decode: &["-hwaccel", "qsv", "-hwaccel_output_format", "qsv"],
        scale: "scale_qsv=-1:{h}",
        upload: "",
        opts: QSV_OPTS,
        quality: QSV_QUALITY,
        pix_fmt: false,
    },
    Profile {
        label: "qsv",
        encoder: "h264_qsv",
        global: &[],
        decode: &[],
        scale: SW_SCALE,
        upload: "",
        opts: QSV_OPTS,
        quality: QSV_QUALITY,
        pix_fmt: true,
    },
    Profile {
        label: "libx264",
        encoder: "libx264",
        global: &[],
        decode: &[],
        scale: SW_SCALE,
        upload: "",
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
pub const HWACCEL_VALUES: [&str; 7] = ["", "auto", "none", "cuda", "d3d11va", "qsv", "vaapi"];

/// The candidates in preference order: the static table, and on Linux the
/// VAAPI profiles for every render node (`/dev/dri/renderD*`) between NVENC
/// and Quick Sync, since VAAPI is what AMD and Intel offer there. Test
/// encodes decide which of them actually works.
fn profiles() -> Vec<Profile> {
    #[cfg(target_os = "linux")]
    {
        let mut all: Vec<Profile> = Vec::new();
        for p in PROFILES {
            if p.encoder == "h264_qsv" && !all.iter().any(|q| q.encoder == "h264_qsv") {
                all.extend(vaapi_profiles());
            }
            all.push(p);
        }
        all
    }
    #[cfg(not(target_os = "linux"))]
    {
        PROFILES.to_vec()
    }
}

// VAAPI: `-rc_mode CBR` for a bitrate cap, constant QP for quality (20 is
// about where the other encoders sit).
#[cfg(target_os = "linux")]
const VAAPI_OPTS: &[&str] = &["-rc_mode", "CBR"];
#[cfg(target_os = "linux")]
const VAAPI_QUALITY: &[&str] = &["-rc_mode", "CQP", "-qp", "20"];
#[cfg(target_os = "linux")]
const VAAPI_UPLOAD: &str = "format=nv12,hwupload";

/// Two profiles per render node: the full path (decode, scale and encode on
/// the card) and software decoding with an upload before the encoder. The
/// strings are leaked once; the list is built at most a handful of times.
#[cfg(target_os = "linux")]
fn vaapi_profiles() -> Vec<Profile> {
    use std::sync::OnceLock;
    static NODES: OnceLock<Vec<&'static str>> = OnceLock::new();
    let nodes = NODES.get_or_init(|| {
        let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri")
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path().to_string_lossy().into_owned())
                    .filter(|p| {
                        p.rsplit('/')
                            .next()
                            .is_some_and(|n| n.starts_with("renderD"))
                    })
                    .collect()
            })
            .unwrap_or_default();
        nodes.sort();
        nodes
            .into_iter()
            .map(|n| &*Box::leak(n.into_boxed_str()))
            .collect()
    });
    let mut out = Vec::new();
    for node in nodes {
        let short = node.rsplit('/').next().unwrap_or(node);
        let global: &'static [&'static str] = Box::leak(Box::new(["-vaapi_device", node]));
        out.push(Profile {
            label: Box::leak(format!("vaapi-full/{short}").into_boxed_str()),
            encoder: "h264_vaapi",
            global,
            decode: &["-hwaccel", "vaapi", "-hwaccel_output_format", "vaapi"],
            scale: "scale_vaapi=-2:{h}",
            upload: VAAPI_UPLOAD,
            opts: VAAPI_OPTS,
            quality: VAAPI_QUALITY,
            pix_fmt: false,
        });
    }
    for node in nodes {
        let short = node.rsplit('/').next().unwrap_or(node);
        let global: &'static [&'static str] = Box::leak(Box::new(["-vaapi_device", node]));
        out.push(Profile {
            label: Box::leak(format!("vaapi/{short}").into_boxed_str()),
            encoder: "h264_vaapi",
            global,
            decode: &[],
            scale: SW_SCALE,
            upload: VAAPI_UPLOAD,
            opts: VAAPI_OPTS,
            quality: VAAPI_QUALITY,
            pix_fmt: false,
        });
    }
    out
}

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
        if found.is_none() && cfg!(windows) {
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
            priority: None,
            threads: 0,
        })
    }

    /// Run every ffmpeg/ffprobe process at this priority and encode with this many threads.
    pub fn with_resource_limits(mut self, priority: FfmpegPriority, threads: u32) -> Self {
        self.priority = Some(priority);
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
        cmd.creation_flags(CREATE_NO_WINDOW | self.priority.map(priority_flag).unwrap_or(0));
        #[cfg(target_os = "linux")]
        if let Some(nice) = self.priority.map(nice_level).filter(|n| *n > 0) {
            // SAFETY: setpriority is async-signal-safe and touches no memory
            // shared with the parent, which is all pre_exec asks for.
            unsafe {
                cmd.pre_exec(move || {
                    rustix::process::setpriority_process(None, nice).map_err(Into::into)
                });
            }
        }
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
        let mut candidates: Vec<Encoder> = profiles()
            .iter()
            .filter(|p| auto || p.encoder == preferred)
            .filter(|p| match hwaccel {
                HwAccel::Auto => true,
                _ => p.decode.is_empty() && p.scale == SW_SCALE,
            })
            .filter(|p| p.decode.is_empty() || sample.is_some())
            .map(Profile::encoder)
            .collect();
        if !auto && candidates.is_empty() {
            // an encoder we have no profile for: bare, as the user asked
            candidates.push(Encoder {
                label: "custom",
                name: preferred.to_string(),
                global: Vec::new(),
                decode: Vec::new(),
                scale: SW_SCALE,
                upload: "",
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
        args.extend(enc.global.iter().copied());
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
        for p in profiles().iter() {
            if !have
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(p.encoder))
            {
                continue;
            }
            let out = std::env::temp_dir()
                .join(format!("replaycut-bench-{}.mp4", p.label.replace('/', "-")));
            let out_s = out.to_string_lossy().into_owned();
            let mut args: Vec<&str> = vec!["-y", "-v", "error"];
            args.extend(p.global.iter().copied());
            args.extend(p.decode.iter().copied());
            args.extend(["-t", &secs, "-i", &clip_s, "-map", "0:v:0", "-an"]);
            let vf = p.encoder().filter_for(1080);
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

    /// The cut of `[start, end]` as its own file (since 3.0): the recording's
    /// streams as they are, from the keyframe at or before `start` to `end`,
    /// with every audio track. Nothing is re-encoded, so it takes about as
    /// long as copying the bytes; the file starts at zero and the caller
    /// learns from its length where in the recording that is.
    pub async fn cut(&self, mkv: &Path, out: &Path, start: f64, seconds: f64) -> Result<()> {
        let (mkv_s, out_s) = (mkv.to_string_lossy(), out.to_string_lossy());
        let (start_s, seconds_s) = (start.to_string(), seconds.to_string());
        let args = [
            "-y",
            "-v",
            "error",
            "-ss",
            &start_s,
            "-t",
            &seconds_s,
            "-i",
            &mkv_s,
            "-map",
            "0",
            "-c",
            "copy",
            "-avoid_negative_ts",
            "make_zero",
            &out_s,
        ];
        let res = self.ffmpeg(&args, Duration::from_secs(300)).await?;
        if !res.status.success() || !out.is_file() {
            let _ = std::fs::remove_file(out);
            bail!(
                "ffmpeg cut: {}",
                String::from_utf8_lossy(&res.stderr).trim()
            );
        }
        Ok(())
    }

    /// Where a stream copy of `[at, ...]` really begins: the timestamp of the
    /// last video keyframe at or before `at` (since 3.0). Only packet headers
    /// are read, and only from a window before `at`, so it costs no decoding.
    /// `None` when ffprobe finds no keyframe there - the caller then knows
    /// nothing better than the range itself.
    pub async fn keyframe_at_or_before(&self, path: &Path, at: f64) -> Option<f64> {
        // OBS writes a keyframe every one or two seconds; 30 s is room for
        // a badly configured encoder and still a short read.
        let from = (at - 30.0).max(0.0);
        let interval = format!("{from:.3}%{at:.3}");
        let p = path.to_string_lossy();
        let out = self
            .ffprobe(&[
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=pts_time,flags",
                "-of",
                "csv=p=0",
                "-read_intervals",
                &interval,
                &p,
            ])
            .await
            .ok()?;
        out.lines()
            .filter_map(|line| {
                let (pts, flags) = line.trim().split_once(',')?;
                let pts: f64 = pts.parse().ok()?;
                // a keyframe at most half a frame past `at` still starts it
                (flags.starts_with('K') && pts <= at + 0.001).then_some(pts)
            })
            .next_back()
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
        Ok((self.duration_exact(path).await? * 100.0).round() / 100.0)
    }

    /// Container duration as ffprobe reports it. Where the number is used to
    /// compute a seek (the cut file since 3.0), two decimals are a third of a
    /// frame off - enough to lose the last one.
    pub async fn duration_exact(&self, path: &Path) -> Result<f64> {
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
        out.trim()
            .parse()
            .with_context(|| format!("ffprobe duration {out:?}"))
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
    /// Kernel plus user time of the ffmpeg process, seconds (Windows and Linux).
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
    #[cfg(target_os = "linux")]
    let children_before = children_cpu_seconds();
    let status = match child.wait() {
        Ok(s) => s,
        Err(e) => return (false, None, format!("ffmpeg: {e}")),
    };
    #[cfg(windows)]
    let cpu = process_cpu_seconds(&child);
    // The child is reaped, so its time is in the children total now; the
    // bench runs one ffmpeg at a time, so the difference is this one.
    #[cfg(target_os = "linux")]
    let cpu = children_cpu_seconds()
        .zip(children_before)
        .map(|(after, before)| after - before);
    #[cfg(not(any(windows, target_os = "linux")))]
    let cpu: Option<f64> = None;
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

/// Kernel plus user time of every reaped child so far, from `/proc/self/stat`
/// (fields 16 and 17, in clock ticks).
#[cfg(target_os = "linux")]
fn children_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // the command name in parentheses may contain spaces: count from its end
    let rest = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // `rest` starts with field 3 (state), so fields 16 and 17 are at 13 and 14
    let cutime: u64 = fields.get(13)?.parse().ok()?;
    let cstime: u64 = fields.get(14)?.parse().ok()?;
    let ticks = rustix::param::clock_ticks_per_second().max(1) as f64;
    Some((cutime + cstime) as f64 / ticks)
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
