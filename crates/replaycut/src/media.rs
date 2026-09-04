//! ffmpeg and ffprobe: locating the binaries, encoder detection, preview
//! remux, probing. Every process runs without a console window.

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Media {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Encoder {
    pub name: String,
    #[allow(dead_code)] // used by the share pipeline (R3b)
    pub opts: Vec<&'static str>,
}

/// Candidates in preference order with their rate-control options.
const ENCODER_PROFILES: [(&str, &[&str]); 4] = [
    ("h264_amf", &["-quality", "quality", "-rc", "cbr"]),
    ("h264_nvenc", &["-preset", "p5", "-rc", "cbr"]),
    ("h264_qsv", &["-preset", "medium"]),
    ("libx264", &["-preset", "veryfast"]),
];

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
        Ok(Self { ffmpeg, ffprobe })
    }

    fn command(&self, exe: &Path) -> Command {
        let mut cmd = Command::new(exe);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);
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

    /// Probe each candidate with a real two-frame encode: the ffmpeg build
    /// knows all hardware encoders even without the matching GPU.
    pub async fn detect_encoder(&self, preferred: &str) -> Result<Encoder> {
        let have = self
            .ffmpeg(&["-hide_banner", "-encoders"], Duration::from_secs(60))
            .await?;
        let have = String::from_utf8_lossy(&have.stdout).into_owned();
        let candidates: Vec<(&str, &[&str])> = if preferred.is_empty() || preferred == "auto" {
            ENCODER_PROFILES.to_vec()
        } else {
            ENCODER_PROFILES
                .iter()
                .copied()
                .filter(|(n, _)| *n == preferred)
                .collect::<Vec<_>>()
                .into_iter()
                .chain(std::iter::once((preferred, &[][..])))
                .take(1)
                .collect()
        };
        let mut tried = Vec::new();
        for (name, opts) in candidates {
            tried.push(name.to_string());
            if !have
                .lines()
                .any(|l| l.split_whitespace().nth(1) == Some(name))
            {
                continue;
            }
            let mut args = vec![
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=320x240:rate=30:duration=0.2",
                "-c:v",
                name,
            ];
            args.extend_from_slice(opts);
            args.extend_from_slice(&["-b:v", "1000k", "-pix_fmt", "yuv420p", "-f", "null", "-"]);
            let out = self.ffmpeg(&args, Duration::from_secs(60)).await?;
            if out.status.success() {
                return Ok(Encoder {
                    name: name.to_string(),
                    opts: opts.to_vec(),
                });
            }
            let first = String::from_utf8_lossy(&out.stderr);
            let first = first
                .lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or("")
                .trim()
                .to_string();
            tracing::warn!("encoder {name} not usable: {first}");
        }
        bail!(
            "no usable H.264 encoder found (tried: {})",
            tried.join(", ")
        )
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
