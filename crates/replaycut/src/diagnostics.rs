//! `GET /api/diagnostics` and `replaycut test`: everything that can go
//! wrong, as one list with a status per row, plus a plain-text copy for a
//! support message. Checks run in parallel with a short timeout each and
//! never change anything.

use std::time::Duration;

use serde::Serialize;
use serde_json::json;

use crate::credentials;
use crate::integrations::{self, Nextcloud};
use crate::platform;
use crate::state::{AppState, VERSION};

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: &'static str,
    pub label: &'static str,
    /// `ok`, `warn`, `fail` or `skip`.
    pub status: &'static str,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    fn new(
        id: &'static str,
        label: &'static str,
        status: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            id,
            label,
            status,
            detail: detail.into(),
            fix: None,
        }
    }
    fn with_fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

pub struct Report {
    pub checks: Vec<Check>,
    pub text: String,
}

impl Report {
    pub fn json(&self) -> serde_json::Value {
        json!({ "checks": self.checks, "text": self.text })
    }
}

fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
}

fn count_files(dir: &std::path::Path, ext: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.path().is_file()
                        && e.path()
                            .extension()
                            .and_then(|x| x.to_str())
                            .is_some_and(|x| x.eq_ignore_ascii_case(ext))
                })
                .count()
        })
        .unwrap_or(0)
}

/// The last lines of the newest log file, secrets never appear in the log.
fn log_tail(state: &AppState, lines: usize) -> Vec<String> {
    let dir = state.data_dir.join("logs");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let newest = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let Some(file) = newest else {
        return Vec::new();
    };
    let text = std::fs::read_to_string(file.path()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all.iter()
        .rev()
        .take(lines)
        .rev()
        .map(|l| l.to_string())
        .collect()
}

pub async fn run(state: &AppState) -> Report {
    let settings = state.settings();
    let runtime = state.runtime();
    let paths = state.paths();
    let mut checks = Vec::new();

    // service
    let uptime = state.started.elapsed().as_secs();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let ram = platform::process_memory_mb()
        .map(|m| format!(" · {m} MB RAM"))
        .unwrap_or_default();
    checks.push(Check::new(
        "service",
        "replaycut",
        "ok",
        format!(
            "{VERSION} · running since {} ({} h {} min) · {exe}{ram} · autostart {}{}",
            state.started_at,
            uptime / 3600,
            (uptime % 3600) / 60,
            if platform::autostart_enabled() {
                "on"
            } else {
                "off"
            },
            if state.dry_run { " · DRY RUN" } else { "" }
        ),
    ));

    // update
    checks.push(if !settings.check_updates {
        Check::new(
            "update",
            "Update",
            "skip",
            "the daily check is off (checkUpdates: false)",
        )
    } else if let Some(u) = state.update.lock().latest.clone() {
        Check::new(
            "update",
            "Update",
            "warn",
            format!("{} is available - this is {VERSION}", u.version),
        )
        .with_fix(format!(
            "Update now on the clips page or in Settings; release notes: {}",
            u.url
        ))
    } else {
        Check::new(
            "update",
            "Update",
            "ok",
            format!("{VERSION} - no newer release known (GitHub is asked once a day)"),
        )
    });

    // ffmpeg (version line) and encoder
    let ffmpeg_check = async {
        match state.media_base.ffmpeg(&["-version"], TIMEOUT).await {
            Ok(out) if out.status.success() => {
                let first = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .replace("ffmpeg version ", "")
                    .split(' ')
                    .next()
                    .unwrap_or("")
                    .to_string();
                Check::new(
                    "ffmpeg",
                    "ffmpeg",
                    "ok",
                    format!(
                        "{first} at {} · ffprobe found",
                        state.media_base.ffmpeg.display()
                    ),
                )
            }
            Ok(out) => Check::new(
                "ffmpeg",
                "ffmpeg",
                "fail",
                format!(
                    "{} does not run: {}",
                    state.media_base.ffmpeg.display(),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            )
            .with_fix("Reinstall ffmpeg (winget install Gyan.FFmpeg) and restart replaycut."),
            Err(e) => Check::new("ffmpeg", "ffmpeg", "fail", format!("{e:#}"))
                .with_fix("Install ffmpeg (winget install Gyan.FFmpeg) and restart replaycut."),
        }
    };

    // folder
    let folder_check = {
        let dir = paths.clip_dir.clone();
        let preview_dir = paths.preview_dir.clone();
        let shared_dir = paths.shared_dir.clone();
        async move {
            if !dir.is_dir() {
                return Check::new(
                    "folder",
                    "Recording folder",
                    "fail",
                    format!("{} does not exist", dir.display()),
                )
                .with_fix("Set the folder OBS records to under Settings › General, or create it.");
            }
            let free = platform::free_space(&dir);
            let clips = count_files(&dir, "mkv");
            let previews = count_files(&preview_dir, "mp4");
            let shared = count_files(&shared_dir, "mp4");
            let free_text = free
                .map(|f| format!(" · {} free", gb(f)))
                .unwrap_or_default();
            let status = match free {
                Some(f) if f < 2 * 1_073_741_824 => "warn",
                _ => "ok",
            };
            let mut c = Check::new(
                "folder",
                "Recording folder",
                status,
                format!(
                    "{}{free_text} · {clips} clip{} · {previews} preview{} · {shared} shared file{}",
                    dir.display(),
                    if clips == 1 { "" } else { "s" },
                    if previews == 1 { "" } else { "s" },
                    if shared == 1 { "" } else { "s" }
                ),
            );
            if status == "warn" {
                c = c.with_fix("Less than 2 GB free: delete old clips (they go to the recycle bin) or move shared\\ elsewhere.");
            }
            c
        }
    };

    // scan
    let scan_check = {
        let scan_at = state.inner.lock().scan_at.clone();
        let just_started = uptime < 60;
        let paused = state.scanning_paused();
        async move {
            if paused {
                return Check::new(
                    "scan",
                    "Folder scan",
                    "warn",
                    "paused - new replays wait in the folder",
                )
                .with_fix("Resume it in the tray menu (Pause scanning) or on the clips page.");
            }
            match scan_at {
                None if just_started => Check::new(
                    "scan",
                    "Folder scan",
                    "ok",
                    "first scan running (the service just started)",
                ),
                None => Check::new("scan", "Folder scan", "warn", "no scan yet")
                    .with_fix("Restart replaycut (Settings › Restart now, or the tray)."),
                Some(at) => {
                    let age = chrono::NaiveDateTime::parse_from_str(&at, "%Y-%m-%dT%H:%M:%S")
                        .ok()
                        .map(|t| (chrono::Local::now().naive_local() - t).num_seconds())
                        .unwrap_or(0);
                    if age > 30 {
                        Check::new(
                            "scan",
                            "Folder scan",
                            "warn",
                            format!("last scan {age} s ago - the watcher stopped reporting"),
                        )
                        .with_fix("Restart replaycut (Settings › Restart now, or the tray).")
                    } else {
                        Check::new(
                            "scan",
                            "Folder scan",
                            "ok",
                            format!("last scan {age} s ago · watcher active"),
                        )
                    }
                }
            }
        }
    };

    // onedrive (since 2.5)
    let onedrive_check = {
        let enabled = settings.integrations.onedrive.enabled;
        let runtime = runtime.clone();
        async move {
            if !enabled {
                return Check::new("onedrive", "OneDrive", "skip", "integration is off");
            }
            let Some(entry) = runtime.integrations.storage("onedrive") else {
                return Check::new("onedrive", "OneDrive", "fail", "enabled, but not connected")
                    .with_fix("Settings › Integrations › OneDrive: Connect OneDrive.");
            };
            match &entry.storage {
                integrations::Storage::OneDrive(od) => {
                    match tokio::time::timeout(TIMEOUT, od.account()).await {
                        Ok(Ok(acc)) => {
                            let quota = match (acc.used, acc.total) {
                                (Some(u), Some(t)) if t > 0 => {
                                    format!(" · {} of {} used", gb(u), gb(t))
                                }
                                _ => String::new(),
                            };
                            Check::new(
                                "onedrive",
                                "OneDrive",
                                "ok",
                                format!("connected as {}{quota}", acc.name),
                            )
                        }
                        Ok(Err(e)) => Check::new("onedrive", "OneDrive", "fail", format!("{e:#}"))
                            .with_fix(
                            "Disconnect and connect OneDrive again under Settings › Integrations.",
                        ),
                        Err(_) => Check::new(
                            "onedrive",
                            "OneDrive",
                            "fail",
                            "no answer from Microsoft Graph within 5 s",
                        ),
                    }
                }
                _ => Check::new("onedrive", "OneDrive", "ok", "dry run"),
            }
        }
    };

    // s3 and webdav (since 2.5): configured and reachable, no probe writes here
    let s3_check = {
        let enabled = settings.integrations.s3.enabled;
        let runtime = runtime.clone();
        async move {
            if !enabled {
                return Check::new("s3", "S3", "skip", "integration is off");
            }
            let Some(entry) = runtime.integrations.storage("s3") else {
                return Check::new("s3", "S3", "fail", "enabled, but no access keys stored")
                    .with_fix("Settings › Integrations › S3: enter access key and secret key.");
            };
            match &entry.storage {
                integrations::Storage::S3(s3) => match tokio::time::timeout(TIMEOUT, s3.head_bucket()).await {
                    Ok(Ok(())) => Check::new("s3", "S3", "ok", format!("bucket reachable · {}", s3.describe_link_mode())),
                    Ok(Err(e)) => Check::new("s3", "S3", "fail", format!("{e:#}"))
                        .with_fix("Check endpoint, region, bucket and keys under Settings › Integrations › S3 (Test connection)."),
                    Err(_) => Check::new("s3", "S3", "fail", "no answer from the endpoint within 5 s"),
                },
                _ => Check::new("s3", "S3", "ok", "dry run"),
            }
        }
    };
    let webdav_check = {
        let enabled = settings.integrations.webdav.enabled;
        let runtime = runtime.clone();
        async move {
            if !enabled {
                return Check::new("webdav", "WebDAV", "skip", "integration is off");
            }
            let Some(entry) = runtime.integrations.storage("webdav") else {
                return Check::new("webdav", "WebDAV", "fail", "enabled, but no login stored")
                    .with_fix("Settings › Integrations › WebDAV: enter user and password.");
            };
            match &entry.storage {
                integrations::Storage::WebDav(d) => match tokio::time::timeout(TIMEOUT, d.check()).await {
                    Ok(Ok(())) => Check::new("webdav", "WebDAV", "ok", "server reachable, login accepted"),
                    Ok(Err(e)) => Check::new("webdav", "WebDAV", "fail", format!("{e:#}"))
                        .with_fix("Check URL, user and password under Settings › Integrations › WebDAV (Test connection)."),
                    Err(_) => Check::new("webdav", "WebDAV", "fail", "no answer from the server within 5 s"),
                },
                _ => Check::new("webdav", "WebDAV", "ok", "dry run"),
            }
        }
    };

    // nextcloud and quota
    let nc_check = {
        let settings = settings.clone();
        let dry_run = state.dry_run;
        async move {
            let nc = &settings.integrations.nextcloud;
            if !nc.enabled {
                return (
                    Check::new("nextcloud", "Nextcloud", "skip", "integration is off"),
                    Check::new("quota", "Nextcloud quota", "skip", "integration is off"),
                );
            }
            if dry_run {
                return (
                    Check::new(
                        "nextcloud",
                        "Nextcloud",
                        "skip",
                        "dry run - uploads are simulated",
                    ),
                    Check::new("quota", "Nextcloud quota", "skip", "dry run"),
                );
            }
            let Ok(Some(cred)) = credentials::read(credentials::NEXTCLOUD) else {
                return (
                    Check::new(
                        "nextcloud",
                        "Nextcloud",
                        "fail",
                        "enabled, but no credentials are stored",
                    )
                    .with_fix("Settings › Integrations › Nextcloud: enter user and app password."),
                    Check::new("quota", "Nextcloud quota", "skip", "no credentials"),
                );
            };
            let started = std::time::Instant::now();
            let result = async {
                let client = Nextcloud::new(&settings, cred.user.clone(), cred.secret)?;
                tokio::time::timeout(TIMEOUT, client.user_info())
                    .await
                    .map_err(|_| anyhow::anyhow!("no answer within 5 s"))?
            }
            .await;
            match result {
                Ok(info) => {
                    state.set_quota(Some(&info));
                    let ms = started.elapsed().as_millis();
                    let nextcloud = Check::new(
                        "nextcloud",
                        "Nextcloud",
                        "ok",
                        format!("logged in as {} at {} · {ms} ms", cred.user, nc.url),
                    );
                    let quota = match (info.free, info.total) {
                        (Some(free), Some(total)) if total > 0 => {
                            let used = 100.0 * (1.0 - free as f64 / total as f64);
                            let status = if used >= 95.0 {
                                "fail"
                            } else if used >= 80.0 {
                                "warn"
                            } else {
                                "ok"
                            };
                            let mut c = Check::new(
                                "quota",
                                "Nextcloud quota",
                                status,
                                format!("{used:.0} % used · {} free of {}", gb(free), gb(total)),
                            );
                            if status != "ok" {
                                c = c.with_fix("Delete old uploads (Delete on a clip with 'Also remove from Nextcloud') or raise the quota.");
                            }
                            c
                        }
                        _ => Check::new("quota", "Nextcloud quota", "ok", "unlimited"),
                    };
                    (nextcloud, quota)
                }
                Err(e) => (
                    Check::new("nextcloud", "Nextcloud", "fail", format!("{e:#}"))
                        .with_fix("Check server address, user and app password under Settings › Integrations."),
                    Check::new("quota", "Nextcloud quota", "skip", "login failed"),
                ),
            }
        }
    };

    // webhook
    let webhook_check = {
        let settings = settings.clone();
        let dry_run = state.dry_run;
        async move {
            if !settings.integrations.discord.enabled {
                return Check::new("webhook", "Discord webhook", "skip", "integration is off");
            }
            if dry_run {
                return Check::new(
                    "webhook",
                    "Discord webhook",
                    "skip",
                    "dry run - posts are simulated",
                );
            }
            let Ok(Some(cred)) = credentials::read(credentials::DISCORD_WEBHOOK) else {
                return Check::new(
                    "webhook",
                    "Discord webhook",
                    "fail",
                    "enabled, but no webhook is stored",
                )
                .with_fix("Settings › Integrations › Discord: paste the webhook URL.");
            };
            if !integrations::is_webhook_url(&cred.secret) {
                return Check::new("webhook", "Discord webhook", "fail", "the stored URL is not a Discord webhook")
                    .with_fix("Discord: Server settings › Integrations › Webhooks › Copy Webhook URL, then paste it under Settings › Integrations.");
            }
            let client = reqwest::Client::builder()
                .timeout(TIMEOUT)
                .user_agent(concat!("replaycut/", env!("CARGO_PKG_VERSION")))
                .build();
            let Ok(client) = client else {
                return Check::new(
                    "webhook",
                    "Discord webhook",
                    "fail",
                    "cannot build the HTTP client",
                );
            };
            match client.get(&cred.secret).send().await {
                Ok(res) if res.status().is_success() => {
                    let body: serde_json::Value = res.json().await.unwrap_or_default();
                    let name = body["name"].as_str().unwrap_or("webhook");
                    Check::new(
                        "webhook",
                        "Discord webhook",
                        "ok",
                        format!("valid · webhook \"{name}\" · posts as {}", settings.display_name),
                    )
                }
                Ok(res) => Check::new(
                    "webhook",
                    "Discord webhook",
                    "fail",
                    format!("HTTP {} - the webhook no longer exists or the URL is incomplete", res.status().as_u16()),
                )
                .with_fix("Discord: Server settings › Integrations › Webhooks › Copy Webhook URL, then paste it under Settings › Integrations and send a test message."),
                Err(e) => Check::new("webhook", "Discord webhook", "fail", format!("{e}"))
                    .with_fix("Is the PC online? Discord did not answer."),
            }
        }
    };

    // network
    let network_check = {
        let settings = settings.clone();
        async move {
            let loopback_only = settings.bind == "127.0.0.1" || settings.bind == "::1";
            let mut detail = format!("listening on {}:{}", settings.bind, settings.port);
            if loopback_only {
                detail.push_str(" · this PC only");
                return Check::new("network", "Network", "ok", detail);
            }
            detail.push_str(&format!(
                " · http://{}:{}/",
                platform::hostname(),
                settings.port
            ));
            if let Some(ip) = platform::primary_ipv4() {
                detail.push_str(&format!(" · http://{ip}:{}/", settings.port));
            }
            match platform::firewall_rule_present() {
                Some(true) => Check::new("network", "Network", "ok", detail + " · firewall rule \"replaycut\" present"),
                Some(false) => Check::new("network", "Network", "warn", detail + " · no firewall rule \"replaycut\"")
                    .with_fix("Other devices may not reach this PC. Run install.cmd again and accept the firewall step."),
                None => Check::new("network", "Network", "ok", detail),
            }
        }
    };

    let (ffmpeg, folder, scan, (nextcloud, quota), webhook, network) = tokio::join!(
        ffmpeg_check,
        folder_check,
        scan_check,
        nc_check,
        webhook_check,
        network_check
    );
    checks.push(ffmpeg);
    let fallbacks = state
        .encoder_fallbacks
        .load(std::sync::atomic::Ordering::Relaxed);
    let mut encoder = Check::new(
        "encoder",
        "Encoder",
        if fallbacks > 0 { "warn" } else { "ok" },
        format!(
            "{} · priority {:?} · {} threads{}",
            runtime.encoder.describe(),
            settings.ffmpeg_priority,
            settings.ffmpeg_threads(),
            if fallbacks > 0 {
                format!(" · fell back to software decoding {fallbacks} time(s) since start")
            } else {
                String::new()
            }
        ),
    );
    if fallbacks > 0 {
        encoder = encoder.with_fix(
            "The GPU decode path failed at least once (driver?). Set Hardware decoding to 'none' under Settings › Encoding if it keeps happening; run 'replaycut bench' to compare.",
        );
    }
    checks.push(encoder);
    checks.push(folder);
    checks.push(scan);
    checks.push(nextcloud);
    checks.push(quota);
    checks.push(onedrive_check.await);
    checks.push(s3_check.await);
    checks.push(webdav_check.await);
    checks.push(webhook);
    checks.push({
        let obs = state.obs.status();
        if !obs.enabled {
            Check::new("obs", "OBS", "skip", "integration is off (obs.enabled: false)")
        } else if !obs.connected {
            Check::new(
                "obs",
                "OBS",
                "fail",
                format!(
                    "not connected: {}",
                    obs.reason.as_deref().unwrap_or("no connection yet")
                ),
            )
            .with_fix("OBS: Tools › WebSocket Server Settings › Enable WebSocket server (port 4455); with a password, enter it on the OBS page. Without OBS running, F9 falls back to a key press.")
        } else if obs.replay_active {
            let length = obs
                .facts
                .as_ref()
                .and_then(|f| f.profile.replay_seconds)
                .map(|s| format!(" ({s} s)"))
                .unwrap_or_default();
            Check::new(
                "obs",
                "OBS",
                "ok",
                format!(
                    "connected · OBS {} · replay buffer running{length}",
                    obs.version.as_deref().unwrap_or("?")
                ),
            )
        } else {
            Check::new(
                "obs",
                "OBS",
                "warn",
                format!(
                    "connected · OBS {} · replay buffer stopped - F9 does nothing",
                    obs.version.as_deref().unwrap_or("?")
                ),
            )
            .with_fix("Start the replay buffer in OBS (Controls › Start Replay Buffer) or on the OBS page.")
        }
    });
    checks.push(network);

    // the copy for a support message: no secrets, settings line, log tail
    let mut text = format!("replaycut {VERSION} - {}\n", crate::util::now_local());
    for c in &checks {
        text.push_str(&format!(
            "{:<9} {:<5} {}\n",
            c.id,
            c.status.to_ascii_uppercase(),
            c.detail
        ));
        if let Some(fix) = &c.fix {
            text.push_str(&format!("{:<15} fix: {fix}\n", ""));
        }
    }
    text.push_str(&format!(
        "settings  clipDir={} port={} bind={} shareKbps={} encoder={} hwaccel={:?} ffmpegPriority={:?} ffmpegThreads={} theme={} nextcloud={} discord={} password={}\n",
        settings.clip_dir.display(),
        settings.port,
        settings.bind,
        settings.share_kbps,
        settings.encoder,
        settings.hwaccel,
        settings.ffmpeg_priority,
        settings.ffmpeg_threads,
        settings.theme,
        settings.integrations.nextcloud.enabled,
        settings.integrations.discord.enabled,
        settings.password_hash.is_some()
    ));
    let tail = log_tail(state, 20);
    if !tail.is_empty() {
        text.push_str("log tail  (last 20 lines)\n");
        for line in tail {
            text.push_str("  ");
            text.push_str(&line);
            text.push('\n');
        }
    }
    Report { checks, text }
}
