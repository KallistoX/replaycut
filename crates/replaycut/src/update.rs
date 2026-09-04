//! Updates: the daily check against the GitHub releases (since 2.0, shown
//! as a banner) and, since 2.3, the one-click update - download the
//! release ZIP and `SHA256SUMS`, verify the minisign signature and the
//! hash, unpack, put the files into the app folder and restart.
//!
//! Trust model: GitHub plus the maintainer's minisign key. A release
//! without a valid `SHA256SUMS.minisig` is never installed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::state::{AppState, VERSION};

const RELEASES_URL: &str = "https://api.github.com/repos/KallistoX/replaycut/releases/latest";
const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(10);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
const NOTES_LIMIT: usize = 16 * 1024;
pub const EXE_NAME: &str = "replaycut.exe";
pub const OLD_EXE_NAME: &str = "replaycut.old.exe";
const MARKER: &str = "installed.json";
/// Everything the release ZIP carries; the first two are required.
const PACKAGE_FILES: [&str; 7] = [
    "replaycut.exe",
    "ui/index.html",
    "install.cmd",
    "uninstall.cmd",
    "README.md",
    "CHANGELOG.md",
    "LICENSE",
];

/// The maintainer's minisign public keys (base64 as `minisign -G` prints
/// them). More than one allows a rotation: a release is accepted when any
/// key verifies it. Empty means no update can be verified.
pub const PUBLIC_KEYS: &[&str] = &[
    // minisign key 48259F89A10BFB0C, see dist/minisign.pub
    "RWQM+wuhiZ8lSAv6rQmSUeFAuYBrG8OYfzcVrw0xcuUlYxCN8XNQO/18",
];

fn releases_url() -> String {
    std::env::var("REPLAYCUT_RELEASES_URL").unwrap_or_else(|_| RELEASES_URL.to_string())
}

/// Public keys in use: the built-in ones, or the test override.
fn public_keys() -> Vec<String> {
    if let Ok(k) = std::env::var("REPLAYCUT_UPDATE_PUBKEY") {
        if !k.trim().is_empty() {
            return vec![k.trim().to_string()];
        }
    }
    PUBLIC_KEYS.iter().map(|k| k.to_string()).collect()
}

/// A release that is newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub notes: String,
    pub published_at: String,
    pub asset_name: String,
    pub asset_size: u64,
    #[serde(skip)]
    pub asset_url: String,
    #[serde(skip)]
    pub sums_url: String,
    #[serde(skip)]
    pub minisig_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Checking,
    Available,
    Downloading,
    Ready,
    Installing,
    Error,
}

/// What `GET /api/update` shows.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub phase: Phase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<UpdateInfo>,
    pub percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// The unpacked package, once verified.
    #[serde(skip)]
    pub ready_dir: Option<PathBuf>,
    /// True on the first start after a one-click update, until the UI saw it.
    pub just_updated: bool,
    /// The release notes and page of the version just installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_url: Option<String>,
}

/// `major.minor.patch` plus whether a pre-release suffix is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub parts: (u64, u64, u64),
    pub pre_release: bool,
}

/// Parse `2.1.0`, `v2.1.0` or `2.0.0-dev`; anything else is `None`.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, !p.is_empty()),
        None => (s, false),
    };
    let core = core.split_once('+').map_or(core, |(c, _)| c);
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next().unwrap_or(Some(0))?;
    let patch = it.next().unwrap_or(Some(0))?;
    if it.next().is_some() {
        return None;
    }
    Some(Version {
        parts: (major, minor, patch),
        pre_release: pre,
    })
}

/// True when `tag` is a release newer than `current`. A pre-release build
/// counts as older than the release with the same number.
pub fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(t), Some(c)) => {
            t.parts > c.parts || (t.parts == c.parts && c.pre_release && !t.pre_release)
        }
        _ => false,
    }
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("replaycut/{VERSION}"))
        .timeout(timeout)
        .build()?)
}

/// The latest release as GitHub describes it, whether newer or not.
async fn fetch_latest_from(url: &str) -> Result<UpdateInfo> {
    let v: serde_json::Value = client(TIMEOUT)?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("no tag_name in the release document"))?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let find = |name: &str| {
        assets
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .map(|a| {
                (
                    a["browser_download_url"].as_str().unwrap_or("").to_string(),
                    a["size"].as_u64().unwrap_or(0),
                )
            })
    };
    let asset_name = format!("replaycut-{version}-windows-x64.zip");
    let (asset_url, asset_size) = find(&asset_name).unwrap_or_default();
    let (sums_url, _) = find("SHA256SUMS").unwrap_or_default();
    let (minisig_url, _) = find("SHA256SUMS.minisig").unwrap_or_default();
    let mut notes = v["body"].as_str().unwrap_or("").to_string();
    if notes.len() > NOTES_LIMIT {
        let cut = notes
            .char_indices()
            .map(|(i, _)| i)
            .take_while(|&i| i < NOTES_LIMIT)
            .last()
            .unwrap_or(0);
        notes.truncate(cut);
        notes.push_str("\n\n…");
    }
    Ok(UpdateInfo {
        version,
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        notes,
        published_at: v["published_at"].as_str().unwrap_or_default().to_string(),
        asset_name,
        asset_size,
        asset_url,
        sums_url,
        minisig_url,
    })
}

/// One check: remember a newer release (or forget an outdated one).
/// Returns the newer release, if any.
pub async fn check(state: &AppState) -> Result<Option<UpdateInfo>> {
    {
        let mut u = state.update.lock();
        if matches!(u.phase, Phase::Downloading | Phase::Installing) {
            return Ok(u.latest.clone());
        }
        u.phase = Phase::Checking;
    }
    let result = fetch_latest_from(&releases_url()).await;
    let outcome = {
        let mut u = state.update.lock();
        u.checked_at = Some(crate::util::now_local());
        match result {
            Ok(info) if is_newer(&info.version, VERSION) => {
                let same_ready = u.phase == Phase::Ready
                    && u.latest.as_ref().map(|l| &l.version) == Some(&info.version);
                if !same_ready {
                    if u.latest.as_ref() != Some(&info) {
                        tracing::info!(
                            "update available: replaycut {} ({})",
                            info.version,
                            info.url
                        );
                    }
                    u.phase = Phase::Available;
                    u.ready_dir = None;
                    u.error = None;
                }
                u.latest = Some(info.clone());
                Ok(Some(info))
            }
            Ok(info) => {
                tracing::debug!("update check: {} is not newer than {VERSION}", info.version);
                u.phase = Phase::Idle;
                u.latest = None;
                u.error = None;
                Ok(None)
            }
            Err(e) => {
                tracing::debug!("update check failed: {e:#}");
                if u.phase == Phase::Checking {
                    u.phase = if u.latest.is_some() {
                        Phase::Available
                    } else {
                        Phase::Idle
                    };
                }
                Err(e)
            }
        }
    };
    state.tray_changed();
    outcome
}

/// Background task: first check after a minute, then daily.
pub async fn run(state: Arc<AppState>) {
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        let _ = check(&state).await;
        tokio::time::sleep(INTERVAL).await;
    }
}

// ------------------------------------------------------------ download and verify

/// The line of `SHA256SUMS` for `name`: lower-case hex.
pub fn sum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?.trim_start_matches('*');
        (file.eq_ignore_ascii_case(name) && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

/// `SHA256SUMS` must carry a minisign signature by one of `keys`.
pub fn verify_signature(sums: &[u8], minisig: &str, keys: &[String]) -> Result<()> {
    if keys.is_empty() {
        bail!("no update signing key is built into this replaycut");
    }
    let signature = minisign_verify::Signature::decode(minisig)
        .map_err(|e| anyhow!("SHA256SUMS.minisig is not a minisign signature: {e}"))?;
    for key in keys {
        let pk = minisign_verify::PublicKey::from_base64(key)
            .map_err(|e| anyhow!("built-in public key is invalid: {e}"))?;
        if pk.verify(sums, &signature, true).is_ok() {
            return Ok(());
        }
    }
    bail!("SHA256SUMS.minisig does not match the release signing key")
}

pub fn sha256_hex(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Unpack the ZIP into `dir`, refusing paths that leave it.
pub fn unpack(zip_path: &Path, dir: &Path) -> Result<()> {
    let file = std::fs::File::open(zip_path)?;
    let mut archive = zip::ZipArchive::new(file).context("not a ZIP file")?;
    std::fs::create_dir_all(dir)?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(rel) = entry.enclosed_name() else {
            bail!("the ZIP contains an unsafe path: {}", entry.name());
        };
        let target = dir.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    for required in &PACKAGE_FILES[..2] {
        if !dir.join(required).is_file() {
            bail!("the ZIP does not contain {required}");
        }
    }
    Ok(())
}

/// `<exe> --version` must print `replaycut <version>`.
async fn check_exe_version(exe: &Path, version: &str) -> Result<()> {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--version");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| anyhow!("the new replaycut.exe did not answer --version"))?
        .context("cannot run the new replaycut.exe")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !text.contains(version) {
        bail!(
            "the new replaycut.exe reports '{}' instead of {version}",
            text.trim()
        );
    }
    Ok(())
}

async fn fetch_bytes(url: &str, limit: usize) -> Result<Vec<u8>> {
    let res = client(TIMEOUT)?.get(url).send().await?.error_for_status()?;
    let bytes = res.bytes().await?;
    if bytes.len() > limit {
        bail!("{url}: too large");
    }
    Ok(bytes.to_vec())
}

async fn download_file(
    url: &str,
    to: &Path,
    expected: u64,
    mut progress: impl FnMut(u8),
) -> Result<()> {
    let res = client(DOWNLOAD_TIMEOUT)?
        .get(url)
        .send()
        .await?
        .error_for_status()?;
    let total = res.content_length().unwrap_or(expected).max(1);
    let mut file = tokio::fs::File::create(to).await?;
    let mut stream = res.bytes_stream();
    let mut done: u64 = 0;
    let mut last = 0u8;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk).await?;
        done += chunk.len() as u64;
        let pct = ((done * 100) / total).min(99) as u8;
        if pct != last {
            last = pct;
            progress(pct);
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    Ok(())
}

/// Where a download lands: `<data-dir>/update/<version>`.
fn update_dir(state: &AppState, version: &str) -> PathBuf {
    state.data_dir.join("update").join(version)
}

/// Download, verify (signature, hash, contents, `--version`) and unpack the
/// latest release. Sets the phase as it goes; errors land in the status.
pub async fn download(state: Arc<AppState>, verify_exe: bool) -> Result<()> {
    let info = {
        let mut u = state.update.lock();
        match u.phase {
            Phase::Ready => return Ok(()),
            Phase::Downloading | Phase::Installing => bail!("an update is already in progress"),
            _ => {}
        }
        let Some(info) = u.latest.clone() else {
            bail!("no update is available");
        };
        if info.asset_url.is_empty() || info.sums_url.is_empty() || info.minisig_url.is_empty() {
            bail!("the release is missing an asset (ZIP, SHA256SUMS or SHA256SUMS.minisig)");
        }
        u.phase = Phase::Downloading;
        u.percent = 0;
        u.error = None;
        info
    };
    let dir = update_dir(&state, &info.version);
    let progress = {
        let state = state.clone();
        move |pct| state.update.lock().percent = pct
    };
    let result = download_package(&info, &dir, &public_keys(), verify_exe, progress)
        .await
        .map(|_| ());
    let mut u = state.update.lock();
    match &result {
        Ok(()) => {
            u.phase = Phase::Ready;
            u.percent = 100;
            u.ready_dir = Some(dir.join("unpacked"));
            tracing::info!("update {} downloaded and verified", info.version);
        }
        Err(e) => {
            u.phase = Phase::Error;
            u.error = Some(format!("{e:#}"));
            u.ready_dir = None;
            let _ = std::fs::remove_dir_all(&dir);
            tracing::warn!("update {} failed: {e:#}", info.version);
        }
    }
    result
}

/// The pipeline behind `download`, without the status: fetch the sums and
/// the signature, verify, download the ZIP, compare the hash, unpack and
/// optionally run the new EXE. Returns the unpacked folder.
pub async fn download_package(
    info: &UpdateInfo,
    dir: &Path,
    keys: &[String],
    verify_exe: bool,
    mut progress: impl FnMut(u8),
) -> Result<PathBuf> {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    if let Some(free) = crate::platform::free_space(dir) {
        if free < info.asset_size.saturating_mul(3) {
            bail!(
                "not enough free space for the update ({} MB needed)",
                info.asset_size * 3 / 1_048_576
            );
        }
    }
    let sums = fetch_bytes(&info.sums_url, 64 * 1024)
        .await
        .context("SHA256SUMS")?;
    let minisig = fetch_bytes(&info.minisig_url, 64 * 1024)
        .await
        .context("SHA256SUMS.minisig")?;
    let minisig = String::from_utf8_lossy(&minisig).into_owned();
    verify_signature(&sums, &minisig, keys)?;
    let sums_text = String::from_utf8_lossy(&sums).into_owned();
    let expected = sum_for(&sums_text, &info.asset_name)
        .ok_or_else(|| anyhow!("SHA256SUMS has no line for {}", info.asset_name))?;
    let zip_path = dir.join(&info.asset_name);
    download_file(&info.asset_url, &zip_path, info.asset_size, &mut progress)
        .await
        .context("download")?;
    let actual = sha256_hex(&zip_path)?;
    if actual != expected {
        bail!("the download does not match SHA256SUMS ({actual} vs {expected})");
    }
    let unpacked = dir.join("unpacked");
    unpack(&zip_path, &unpacked)?;
    if verify_exe {
        check_exe_version(&unpacked.join(EXE_NAME), &info.version).await?;
    }
    Ok(unpacked)
}

// ------------------------------------------------------------ install

/// Where this executable lives.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
}

/// True when this executable is the installed copy in the app folder.
#[cfg(windows)]
pub fn is_installed_copy() -> bool {
    let app = crate::winshell::app_dir();
    match (exe_dir(), std::fs::canonicalize(&app)) {
        (Some(dir), Ok(app)) => std::fs::canonicalize(&dir).ok() == Some(app),
        _ => false,
    }
}
#[cfg(not(windows))]
pub fn is_installed_copy() -> bool {
    false
}

/// Put the unpacked package into `app`: the running EXE is renamed aside
/// (Windows allows that), everything else copied over. No restart here.
pub fn install_files(unpacked: &Path, app: &Path) -> Result<()> {
    let exe = app.join(EXE_NAME);
    let old = app.join(OLD_EXE_NAME);
    let _ = std::fs::remove_file(&old);
    if exe.exists() {
        std::fs::rename(&exe, &old)
            .with_context(|| format!("cannot move {} aside", exe.display()))?;
    }
    for name in PACKAGE_FILES {
        let src = unpacked.join(name);
        if !src.is_file() {
            continue;
        }
        let dst = app.join(name);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&src, &dst).with_context(|| format!("cannot copy {name}"))?;
    }
    Ok(())
}

/// Apply the verified package and restart into the new version.
pub fn install(state: &AppState) -> Result<()> {
    let (unpacked, version, marker) = {
        let u = state.update.lock();
        match (&u.phase, &u.ready_dir, &u.latest) {
            (Phase::Ready, Some(dir), Some(info)) => (
                dir.clone(),
                info.version.clone(),
                serde_json::json!({ "version": info.version, "notes": info.notes, "url": info.url }),
            ),
            _ => bail!("no verified update is ready - download it first"),
        }
    };
    if !is_installed_copy() {
        bail!("this copy was not installed with install.cmd - update it by hand");
    }
    if state.inner.lock().current_job.is_some() {
        bail!("a share is running - update afterwards");
    }
    let shutdown = state
        .shutdown
        .get()
        .ok_or_else(|| anyhow!("restart not available"))?;
    let app = exe_dir().ok_or_else(|| anyhow!("cannot locate the app folder"))?;
    state.update.lock().phase = Phase::Installing;
    if let Err(e) = install_files(&unpacked, &app) {
        let mut u = state.update.lock();
        u.phase = Phase::Error;
        u.error = Some(format!("{e:#}"));
        return Err(e);
    }
    // The marker tells the next start that it is the update's first start.
    let _ = std::fs::write(
        state.data_dir.join("update").join(MARKER),
        marker.to_string(),
    );
    tracing::info!("update {version} installed - restarting");
    spawn_new(&app.join(EXE_NAME))?;
    shutdown.request("update");
    Ok(())
}

/// Start the new executable with this process's arguments (the command-line
/// overrides stay in force), plus `--no-browser --wait-for-exit`.
fn spawn_new(exe: &Path) -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    for flag in ["--no-browser", "--wait-for-exit"] {
        if !args.iter().any(|a| a == flag) {
            args.push(flag.to_string());
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    #[cfg(windows)]
    {
        crate::winshell::spawn_detached(exe, &refs)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(exe)
            .args(&refs)
            .spawn()
            .with_context(|| format!("cannot start {}", exe.display()))?;
        Ok(())
    }
}

/// What the marker of a just-installed update says.
pub struct JustUpdated {
    pub notes: String,
    pub url: String,
}

/// At start: remove the previous executable and the update folder. Returns
/// the marker when this is the first start after a one-click update.
pub fn cleanup_after_start(data_dir: &Path) -> Option<JustUpdated> {
    let update_dir = data_dir.join("update");
    let marker = update_dir.join(MARKER);
    let just_updated = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .filter(|v| v["version"].as_str() == Some(VERSION))
        .map(|v| JustUpdated {
            notes: v["notes"].as_str().unwrap_or("").to_string(),
            url: v["url"].as_str().unwrap_or("").to_string(),
        });
    if update_dir.is_dir() {
        let _ = std::fs::remove_dir_all(&update_dir);
    }
    if let Some(dir) = exe_dir() {
        let old = dir.join(OLD_EXE_NAME);
        if old.exists() {
            // The old process may hold its image for a moment.
            std::thread::spawn(move || {
                for _ in 0..5 {
                    if std::fs::remove_file(&old).is_ok() || !old.exists() {
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
                tracing::debug!("{} could not be removed yet", old.display());
            });
        }
    }
    if just_updated.is_some() {
        tracing::info!("first start after the update to {VERSION}");
    }
    just_updated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(
            parse_version("v2.1.0"),
            Some(Version {
                parts: (2, 1, 0),
                pre_release: false
            })
        );
        assert_eq!(
            parse_version("2.0.0-dev"),
            Some(Version {
                parts: (2, 0, 0),
                pre_release: true
            })
        );
        assert_eq!(parse_version("2.1"), parse_version("2.1.0"));
        assert!(parse_version("latest").is_none());
        assert!(parse_version("1.2.3.4").is_none());
    }

    #[test]
    fn newer_means_higher_or_the_release_of_a_prerelease() {
        assert!(is_newer("v2.1.0", "2.0.0"));
        assert!(is_newer("v2.0.1", "2.0.0"));
        assert!(is_newer("v2.0.0", "2.0.0-dev"));
        assert!(!is_newer("v2.0.0", "2.0.0"));
        assert!(!is_newer("v1.9.9", "2.0.0"));
        assert!(!is_newer("v2.0.0-rc1", "2.0.0"));
        assert!(!is_newer("nonsense", "2.0.0"));
    }

    #[test]
    fn built_in_keys_are_valid_minisign_keys() {
        assert!(!PUBLIC_KEYS.is_empty(), "no release signing key built in");
        for k in PUBLIC_KEYS {
            minisign_verify::PublicKey::from_base64(k)
                .unwrap_or_else(|e| panic!("public key {k}: {e}"));
        }
    }

    #[test]
    fn sums_lines_are_found_case_insensitively() {
        let sums = "AB".repeat(32)
            + "  replaycut-2.3.0-windows-x64.zip\n"
            + &"cd".repeat(32)
            + " *Other.zip\n";
        assert_eq!(
            sum_for(&sums, "REPLAYCUT-2.3.0-windows-x64.zip").as_deref(),
            Some("ab".repeat(32).as_str())
        );
        assert_eq!(
            sum_for(&sums, "other.zip").as_deref(),
            Some("cd".repeat(32).as_str())
        );
        assert!(sum_for(&sums, "nope.zip").is_none());
        assert!(sum_for("short  replaycut.zip", "replaycut.zip").is_none());
    }

    // --- a minisign key pair for the tests (legacy "Ed" signatures over the raw file)

    pub(crate) struct TestKey {
        signing: ed25519_dalek::SigningKey,
        key_id: [u8; 8],
    }

    impl TestKey {
        pub fn new() -> Self {
            use rand_core::RngCore;
            let mut rng = rand_core::OsRng;
            let mut secret = [0u8; 32];
            rng.fill_bytes(&mut secret);
            let mut key_id = [0u8; 8];
            rng.fill_bytes(&mut key_id);
            Self {
                signing: ed25519_dalek::SigningKey::from_bytes(&secret),
                key_id,
            }
        }

        pub fn public_base64(&self) -> String {
            use base64::Engine as _;
            let mut bytes = b"Ed".to_vec();
            bytes.extend_from_slice(&self.key_id);
            bytes.extend_from_slice(self.signing.verifying_key().as_bytes());
            base64::engine::general_purpose::STANDARD.encode(bytes)
        }

        /// The `.minisig` file for `data`.
        pub fn sign(&self, data: &[u8]) -> String {
            use base64::Engine as _;
            use ed25519_dalek::Signer;
            let b64 = base64::engine::general_purpose::STANDARD;
            let sig = self.signing.sign(data).to_bytes();
            let mut sig_bytes = b"Ed".to_vec();
            sig_bytes.extend_from_slice(&self.key_id);
            sig_bytes.extend_from_slice(&sig);
            let trusted = "timestamp:1 file:SHA256SUMS";
            let mut global_input = sig.to_vec();
            global_input.extend_from_slice(trusted.as_bytes());
            let global = self.signing.sign(&global_input).to_bytes();
            format!(
                "untrusted comment: test\n{}\ntrusted comment: {trusted}\n{}\n",
                b64.encode(sig_bytes),
                b64.encode(global)
            )
        }
    }

    #[test]
    fn signature_is_checked_against_the_keys() {
        let key = TestKey::new();
        let other = TestKey::new();
        let sums = b"0000  replaycut-9.9.0-windows-x64.zip\n";
        let sig = key.sign(sums);
        assert!(verify_signature(sums, &sig, &[key.public_base64()]).is_ok());
        assert!(
            verify_signature(sums, &sig, &[other.public_base64(), key.public_base64()]).is_ok()
        );
        let err = verify_signature(sums, &sig, &[other.public_base64()]).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        let err = verify_signature(b"tampered", &sig, &[key.public_base64()]).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
        assert!(verify_signature(sums, "garbage", &[key.public_base64()]).is_err());
        let err = verify_signature(sums, &sig, &[]).unwrap_err();
        assert!(err.to_string().contains("no update signing key"), "{err}");
    }

    /// A release ZIP with the required files (a fake EXE is fine when
    /// `verify_exe` is off).
    pub(crate) fn make_zip(path: &Path, version: &str) {
        use std::io::Write;
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("replaycut.exe", opts).unwrap();
        zip.write_all(format!("fake replaycut {version}").as_bytes())
            .unwrap();
        zip.add_directory("ui", opts).unwrap();
        zip.start_file("ui/index.html", opts).unwrap();
        zip.write_all(b"<!doctype html><title>replaycut</title>")
            .unwrap();
        zip.start_file("CHANGELOG.md", opts).unwrap();
        zip.write_all(b"# Changelog\n").unwrap();
        zip.finish().unwrap();
    }

    #[test]
    fn unpack_checks_paths_and_required_files() {
        let dir = std::env::temp_dir().join(format!("rc-unpack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let zip_path = dir.join("pkg.zip");
        make_zip(&zip_path, "9.9.0");
        let out = dir.join("unpacked");
        unpack(&zip_path, &out).unwrap();
        assert!(out.join("replaycut.exe").is_file());
        assert!(out.join("ui").join("index.html").is_file());
        assert_eq!(sha256_hex(&zip_path).unwrap().len(), 64);

        // a ZIP without the UI file is refused
        let bad = dir.join("bad.zip");
        {
            use std::io::Write;
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
            zip.start_file("replaycut.exe", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        let err = unpack(&bad, &dir.join("bad-out")).unwrap_err();
        assert!(err.to_string().contains("ui/index.html"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_files_moves_the_exe_aside_and_copies_the_package() {
        let dir = std::env::temp_dir().join(format!("rc-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let app = dir.join("app");
        let unpacked = dir.join("unpacked");
        std::fs::create_dir_all(app.join("ui")).unwrap();
        std::fs::create_dir_all(unpacked.join("ui")).unwrap();
        std::fs::write(app.join(EXE_NAME), b"old exe").unwrap();
        std::fs::write(app.join("ui/index.html"), b"old ui").unwrap();
        std::fs::write(unpacked.join(EXE_NAME), b"new exe").unwrap();
        std::fs::write(unpacked.join("ui/index.html"), b"new ui").unwrap();
        std::fs::write(unpacked.join("CHANGELOG.md"), b"notes").unwrap();
        install_files(&unpacked, &app).unwrap();
        assert_eq!(std::fs::read(app.join(EXE_NAME)).unwrap(), b"new exe");
        assert_eq!(std::fs::read(app.join(OLD_EXE_NAME)).unwrap(), b"old exe");
        assert_eq!(std::fs::read(app.join("ui/index.html")).unwrap(), b"new ui");
        assert_eq!(std::fs::read(app.join("CHANGELOG.md")).unwrap(), b"notes");
        assert!(
            !app.join("LICENSE").exists(),
            "missing optional files are skipped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- the whole pipeline against a fake GitHub release served locally

    struct FakeRelease {
        url: String,
        _task: tokio::task::JoinHandle<()>,
    }

    /// Serve a release document plus assets; `tamper` breaks one of them.
    async fn fake_release(version: &str, key: &TestKey, tamper: &str) -> FakeRelease {
        use axum::{extract::Path as AxPath, routing::get, Router};
        let dir = std::env::temp_dir().join(format!("rc-release-{}-{tamper}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let asset_name = format!("replaycut-{version}-windows-x64.zip");
        let zip_path = dir.join(&asset_name);
        make_zip(&zip_path, version);
        let zip_bytes = std::fs::read(&zip_path).unwrap();
        let mut hash = sha256_hex(&zip_path).unwrap();
        if tamper == "hash" {
            hash = "00".repeat(32);
        }
        let sums = format!(
            "{hash}  {asset_name}
"
        );
        let minisig = if tamper == "signature" {
            TestKey::new().sign(sums.as_bytes())
        } else {
            key.sign(sums.as_bytes())
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut assets = vec![
            serde_json::json!({ "name": asset_name, "browser_download_url": format!("{base}/dl/{asset_name}"), "size": zip_bytes.len() }),
            serde_json::json!({ "name": "SHA256SUMS", "browser_download_url": format!("{base}/dl/SHA256SUMS"), "size": sums.len() }),
            serde_json::json!({ "name": "SHA256SUMS.minisig", "browser_download_url": format!("{base}/dl/SHA256SUMS.minisig"), "size": minisig.len() }),
        ];
        if tamper == "unsigned" {
            assets.pop();
        }
        let doc = serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": format!("{base}/releases/v{version}"),
            "published_at": "2026-09-04T12:00:00Z",
            "body": "## Added
- one-click update
",
            "assets": assets,
        });
        let files: std::collections::HashMap<String, Vec<u8>> = [
            (asset_name.clone(), zip_bytes),
            ("SHA256SUMS".to_string(), sums.into_bytes()),
            ("SHA256SUMS.minisig".to_string(), minisig.into_bytes()),
        ]
        .into_iter()
        .collect();
        let files = Arc::new(files);
        let app = Router::new()
            .route(
                "/release",
                get(move || {
                    let doc = doc.clone();
                    async move { axum::Json(doc) }
                }),
            )
            .route(
                "/dl/{name}",
                get(move |AxPath(name): AxPath<String>| {
                    let files = files.clone();
                    async move {
                        match files.get(&name) {
                            Some(b) => (axum::http::StatusCode::OK, b.clone()),
                            None => (axum::http::StatusCode::NOT_FOUND, Vec::new()),
                        }
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeRelease {
            url: format!("{base}/release"),
            _task: task,
        }
    }

    /// Not a test: writes a fake release for trying the UI against a dev
    /// instance. `RC_FAKE_DIR` is the output folder, `RC_FAKE_EXE` the
    /// executable to pack (built with version `RC_FAKE_VERSION`),
    /// `RC_FAKE_BASE` the URL the folder is served at. Run with
    /// `cargo test -p replaycut write_fake_release -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn write_fake_release() {
        use std::io::Write;
        let dir = PathBuf::from(std::env::var("RC_FAKE_DIR").expect("RC_FAKE_DIR"));
        let exe = PathBuf::from(std::env::var("RC_FAKE_EXE").expect("RC_FAKE_EXE"));
        let version = std::env::var("RC_FAKE_VERSION").unwrap_or_else(|_| "9.9.0".into());
        let base = std::env::var("RC_FAKE_BASE").unwrap_or_else(|_| "http://127.0.0.1:8481".into());
        std::fs::create_dir_all(&dir).unwrap();
        let asset_name = format!("replaycut-{version}-windows-x64.zip");
        let zip_path = dir.join(&asset_name);
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("replaycut.exe", opts).unwrap();
            zip.write_all(&std::fs::read(&exe).unwrap()).unwrap();
            zip.add_directory("ui", opts).unwrap();
            zip.start_file("ui/index.html", opts).unwrap();
            zip.write_all(
                &std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/index.html"))
                    .unwrap(),
            )
            .unwrap();
            zip.start_file("CHANGELOG.md", opts).unwrap();
            zip.write_all(
                b"# Changelog
",
            )
            .unwrap();
            zip.finish().unwrap();
        }
        let sums = format!(
            "{}  {asset_name}
",
            sha256_hex(&zip_path).unwrap()
        );
        let key = TestKey::new();
        std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
        std::fs::write(dir.join("SHA256SUMS.minisig"), key.sign(sums.as_bytes())).unwrap();
        std::fs::write(dir.join("pubkey.txt"), key.public_base64()).unwrap();
        let size = std::fs::metadata(&zip_path).unwrap().len();
        let notes = "A fake release for trying the one-click update.

## Added

- One-click update: the service downloads the release ZIP, verifies the **minisign signature** of `SHA256SUMS` and the hash, then restarts into the new version.
- `GET /api/update` with the phases `idle`, `available`, `downloading`, `ready`.

## Fixed

- A stale toast after OBS restarted.

## Notes

See the [changelog](https://example.com/CHANGELOG.md) for everything.

```
replaycut --version
```
";
        let doc = serde_json::json!({
            "tag_name": format!("v{version}"),
            "html_url": format!("{base}/release.json"),
            "published_at": "2026-09-05T08:00:00Z",
            "body": notes,
            "assets": [
                { "name": asset_name, "browser_download_url": format!("{base}/{asset_name}"), "size": size },
                { "name": "SHA256SUMS", "browser_download_url": format!("{base}/SHA256SUMS"), "size": sums.len() },
                { "name": "SHA256SUMS.minisig", "browser_download_url": format!("{base}/SHA256SUMS.minisig"), "size": 0 }
            ]
        });
        std::fs::write(
            dir.join("release.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
        println!(
            "fake release {version} in {} - REPLAYCUT_UPDATE_PUBKEY={}",
            dir.display(),
            key.public_base64()
        );
    }

    #[tokio::test]
    async fn fake_release_is_checked_downloaded_and_verified() {
        let key = TestKey::new();
        let keys = vec![key.public_base64()];
        let work = std::env::temp_dir().join(format!("rc-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);

        // the happy path
        let rel = fake_release("9.9.0", &key, "none").await;
        let info = fetch_latest_from(&rel.url).await.unwrap();
        assert_eq!(info.version, "9.9.0");
        assert!(is_newer(&info.version, VERSION));
        assert_eq!(info.asset_name, "replaycut-9.9.0-windows-x64.zip");
        assert!(info.asset_size > 0);
        assert!(info.notes.contains("one-click update"));
        assert!(info.minisig_url.ends_with("SHA256SUMS.minisig"));
        let mut seen = Vec::new();
        let unpacked = download_package(&info, &work.join("good"), &keys, false, |p| seen.push(p))
            .await
            .unwrap();
        assert!(unpacked.join(EXE_NAME).is_file());
        assert!(unpacked.join("ui").join("index.html").is_file());
        assert!(seen.iter().all(|&p| p < 100), "{seen:?}");

        // a fake EXE fails the --version check
        let err = download_package(&info, &work.join("exe"), &keys, true, |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("replaycut.exe"), "{err:#}");

        // a wrong hash
        let rel = fake_release("9.9.0", &key, "hash").await;
        let info = fetch_latest_from(&rel.url).await.unwrap();
        let err = download_package(&info, &work.join("hash"), &keys, false, |_| {})
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match SHA256SUMS"),
            "{err:#}"
        );

        // signed by somebody else
        let rel = fake_release("9.9.0", &key, "signature").await;
        let info = fetch_latest_from(&rel.url).await.unwrap();
        let err = download_package(&info, &work.join("sig"), &keys, false, |_| {})
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("signing key"), "{err:#}");

        // no signature published at all
        let rel = fake_release("9.9.0", &key, "unsigned").await;
        let info = fetch_latest_from(&rel.url).await.unwrap();
        assert!(info.minisig_url.is_empty());

        let _ = std::fs::remove_dir_all(&work);
    }
}
