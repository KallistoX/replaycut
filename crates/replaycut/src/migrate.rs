//! Migration from the 1.x PowerShell service ("WARDOGS Clip-Service"): its
//! settings lived in the arguments of a scheduled task, its state files
//! have the same formats as ours, its credentials sit under `wardogs/*` in
//! the Credential Manager. Settings, state and credentials are only taken
//! over where nothing of ours exists yet; the old task is always stopped and
//! removed so that the port is free.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::credentials;
use crate::settings::Settings;
use crate::winshell;

pub const OLD_TASK: &str = "WARDOGS Clip-Service";
pub const OLD_DISPLAY_NAME: &str = "WARDOGS";
pub const OLD_FIREWALL_RULE: &str = "WARDOGS Clip-Service";
const OLD_CREDENTIALS: [(&str, &str); 2] = [
    ("wardogs/nextcloud", credentials::NEXTCLOUD),
    ("wardogs/discord-webhook", credentials::DISCORD_WEBHOOK),
];
const STATE_FILES: [&str; 3] = ["clip-names.json", "clip-seen.json", "clip-history.json"];

/// Traces of the old service found on this machine.
pub struct OldService {
    pub task_exists: bool,
    /// Lower-cased parameter names -> values from the task's arguments.
    pub args: HashMap<String, String>,
    pub data_dir: PathBuf,
    pub port: u16,
}

pub fn old_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("WARDOGS")
}

pub fn detect() -> Option<OldService> {
    let data_dir = old_data_dir();
    let xml = task_xml();
    let has_files = STATE_FILES.iter().any(|f| data_dir.join(f).is_file());
    if xml.is_none() && !has_files {
        return None;
    }
    let args = xml
        .as_deref()
        .and_then(arguments_from_xml)
        .map(|a| parse_arguments(&a))
        .unwrap_or_default();
    let port = args
        .get("port")
        .and_then(|p| p.parse().ok())
        .unwrap_or(8420);
    Some(OldService {
        task_exists: xml.is_some(),
        args,
        data_dir,
        port,
    })
}

fn task_xml() -> Option<String> {
    let (ok, out) = winshell::run_hidden("schtasks", &["/Query", "/TN", OLD_TASK, "/XML"]).ok()?;
    if ok && out.contains("<Task") {
        Some(out)
    } else {
        None
    }
}

/// The text of `<Arguments>` with XML entities resolved.
pub fn arguments_from_xml(xml: &str) -> Option<String> {
    let start = xml.find("<Arguments>")? + "<Arguments>".len();
    let end = xml[start..].find("</Arguments>")? + start;
    Some(
        xml[start..end]
            .replace("&quot;", "\"")
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// `-Name value`, `-Name "value with spaces"`, bare `-Switch` -> `true`.
pub fn parse_arguments(s: &str) -> HashMap<String, String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut pending = false;
    for c in s.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                pending = true;
            }
            c if c.is_whitespace() && !quoted => {
                if pending {
                    tokens.push(std::mem::take(&mut current));
                    pending = false;
                }
            }
            c => {
                current.push(c);
                pending = true;
            }
        }
    }
    if pending {
        tokens.push(current);
    }
    let mut map = HashMap::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if let Some(name) = t
            .strip_prefix('-')
            .filter(|n| !n.is_empty() && !n.starts_with('-'))
        {
            let name = name.to_ascii_lowercase();
            let value = tokens
                .get(i + 1)
                .filter(|v| !v.starts_with('-') || v.parse::<f64>().is_ok());
            match value {
                Some(v) => {
                    map.insert(name, v.clone());
                    i += 2;
                }
                None => {
                    map.insert(name, "true".into());
                    i += 1;
                }
            }
        } else {
            i += 1;
        }
    }
    map
}

/// Old task arguments -> our settings (only the fields the old service had).
pub fn apply_arguments(args: &HashMap<String, String>, settings: &mut Settings) -> Vec<String> {
    let mut taken = Vec::new();
    if let Some(v) = args.get("clipdir") {
        settings.clip_dir = PathBuf::from(v);
        taken.push(format!("clip folder {v}"));
    }
    if let Some(p) = args.get("port").and_then(|v| v.parse().ok()) {
        settings.port = p;
        taken.push(format!("port {p}"));
    }
    if let Some(v) = args.get("nextcloudurl") {
        settings.integrations.nextcloud.url = v.trim_end_matches('/').to_string();
        taken.push("Nextcloud URL".into());
    }
    if let Some(v) = args.get("nextcloudfolder") {
        settings.integrations.nextcloud.folder = v.trim_matches('/').to_string();
        taken.push(format!("Nextcloud folder {v}"));
    }
    if let Some(d) = args.get("expiredays").and_then(|v| v.parse().ok()) {
        settings.integrations.nextcloud.expire_days = d;
        taken.push(format!("link expiry {d} days"));
    }
    if let Some(k) = args.get("sharekbps").and_then(|v| v.parse().ok()) {
        settings.share_kbps = k;
        taken.push(format!("share bitrate {k} kbps"));
    }
    settings.display_name = OLD_DISPLAY_NAME.to_string();
    taken.push(format!("display name {OLD_DISPLAY_NAME}"));
    taken
}

/// What the migration did, for the installer's summary.
#[derive(Default)]
pub struct Report {
    pub lines: Vec<String>,
    pub settings_changed: bool,
    /// The old task existed: its firewall rule and URL reservation should go
    /// in the elevated step, and autostart should stay on.
    pub had_task: bool,
    pub old_port: u16,
}

/// Run the migration. `fresh` means no settings.json existed before this
/// install, so the old arguments become our settings.
pub fn run(
    old: &OldService,
    settings: &mut Settings,
    fresh: bool,
    data_dir: &Path,
) -> Result<Report> {
    let mut report = Report {
        had_task: old.task_exists,
        old_port: old.port,
        ..Report::default()
    };

    if fresh && !old.args.is_empty() {
        let taken = apply_arguments(&old.args, settings);
        report.settings_changed = true;
        report
            .lines
            .push(format!("settings from the old task: {}", taken.join(", ")));
    } else if !old.args.is_empty() {
        report
            .lines
            .push("settings.json exists - the old task's arguments were not applied".into());
    }

    for name in STATE_FILES {
        let (from, to) = (old.data_dir.join(name), data_dir.join(name));
        if from.is_file() && !to.is_file() {
            match std::fs::copy(&from, &to) {
                Ok(_) => report.lines.push(format!("{name} copied")),
                Err(e) => report.lines.push(format!("{name} not copied: {e}")),
            }
        }
    }

    for (from, to) in OLD_CREDENTIALS {
        match (credentials::read(from)?, credentials::read(to)?) {
            (Some(c), None) => {
                credentials::write(to, &c.user, &c.secret)?;
                report.lines.push(format!("credential {from} -> {to}"));
            }
            (Some(_), Some(_)) => report
                .lines
                .push(format!("credential {to} already exists - kept")),
            (None, _) => {}
        }
    }
    if fresh {
        let nc = credentials::read(credentials::NEXTCLOUD)?.is_some();
        let dc = credentials::read(credentials::DISCORD_WEBHOOK)?.is_some();
        if settings.integrations.nextcloud.enabled != nc
            || settings.integrations.discord.enabled != dc
        {
            settings.integrations.nextcloud.enabled = nc;
            settings.integrations.discord.enabled = dc;
            report.settings_changed = true;
        }
        report.lines.push(format!(
            "integrations: Nextcloud {}, Discord {}",
            if nc { "on" } else { "off" },
            if dc { "on" } else { "off" }
        ));
    }

    if old.task_exists {
        let _ = winshell::run_hidden("schtasks", &["/End", "/TN", OLD_TASK]);
        let (ok, out) = winshell::run_hidden("schtasks", &["/Delete", "/TN", OLD_TASK, "/F"])?;
        if ok {
            report
                .lines
                .push(format!("scheduled task '{OLD_TASK}' stopped and removed"));
        } else {
            report.lines.push(format!(
                "scheduled task '{OLD_TASK}' could not be removed ({}) - remove it in Task Scheduler",
                out.trim()
            ));
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<Task><Actions><Exec><Command>powershell.exe</Command><Arguments>-NoProfile -File "C:\Users\me\wardogs\scripts\clip-service.ps1" -ClipDir "C:\Users\me\Videos\WARDOGS" -Port 8420 -NextcloudUrl "https://cloud.example.com" -NextcloudFolder "WARDOGS-Clips" -ExpireDays 0 -ShareKbps 6000</Arguments></Exec></Actions></Task>"#;

    #[test]
    fn arguments_are_extracted_and_parsed() {
        let args = parse_arguments(&arguments_from_xml(XML).unwrap());
        assert_eq!(args["clipdir"], r"C:\Users\me\Videos\WARDOGS");
        assert_eq!(args["port"], "8420");
        assert_eq!(args["nextcloudurl"], "https://cloud.example.com");
        assert_eq!(args["nextcloudfolder"], "WARDOGS-Clips");
        assert_eq!(args["expiredays"], "0");
        assert_eq!(args["sharekbps"], "6000");
        assert_eq!(args["noprofile"], "true");
    }

    #[test]
    fn entities_and_switches() {
        let a = arguments_from_xml("<Arguments>-Dir &quot;a &amp; b&quot; -DryRun</Arguments>")
            .unwrap();
        let args = parse_arguments(&a);
        assert_eq!(args["dir"], "a & b");
        assert_eq!(args["dryrun"], "true");
        assert!(arguments_from_xml("<Task/>").is_none());
    }

    #[test]
    fn mapping_to_settings() {
        let args = parse_arguments(&arguments_from_xml(XML).unwrap());
        let mut s = Settings::default();
        apply_arguments(&args, &mut s);
        assert_eq!(s.clip_dir, PathBuf::from(r"C:\Users\me\Videos\WARDOGS"));
        assert_eq!(s.port, 8420);
        assert_eq!(s.integrations.nextcloud.url, "https://cloud.example.com");
        assert_eq!(s.integrations.nextcloud.folder, "WARDOGS-Clips");
        assert_eq!(s.integrations.nextcloud.expire_days, 0);
        assert_eq!(s.share_kbps, 6000);
        assert_eq!(s.display_name, "WARDOGS");
    }
}
