//! `replaycut install`, `replaycut uninstall`, `replaycut autostart`: the
//! per-user installation under `%LOCALAPPDATA%\replaycut\app`, the start
//! menu and desktop shortcuts, the toast registration, the optional Run
//! entry and the optional firewall rule. No admin except for the one
//! firewall step, which asks first. Windows only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use crate::migrate;
use crate::platform;
use crate::settings::Settings;
use crate::setup::ask_yes_no;
use crate::state::VERSION;
use crate::winshell::{self, Elevated};

const EXE_NAME: &str = "replaycut.exe";
const OLD_EXE_NAME: &str = "replaycut.old.exe";
const UI_FILE: &str = "ui\\index.html";
const OPTIONAL_FILES: [&str; 5] = [
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "install.cmd",
    "uninstall.cmd",
];
const ICON_NAME: &str = "replaycut.ico";
const ICON: &[u8] = include_bytes!("../assets/replaycut.ico");
const FIREWALL_RULE: &str = "replaycut";
const SHORTCUT_NAME: &str = "replaycut.lnk";

/// Where the installed executable lives.
pub fn installed_exe() -> PathBuf {
    winshell::app_dir().join(EXE_NAME)
}

fn shortcut_paths() -> Result<Vec<PathBuf>> {
    Ok(vec![
        winshell::programs_dir()?.join(SHORTCUT_NAME),
        winshell::desktop_dir()?.join(SHORTCUT_NAME),
    ])
}

fn step(text: &str) {
    println!("\n== {text}");
}

/// Copy the package into the app folder. The running executable may be the
/// one to replace: Windows allows renaming it, not overwriting it.
fn copy_package(source_dir: &Path, app: &Path) -> Result<()> {
    std::fs::create_dir_all(app.join("ui"))?;
    let _ = std::fs::remove_file(app.join(OLD_EXE_NAME));

    let source_exe = source_dir.join(EXE_NAME);
    let app_exe = app.join(EXE_NAME);
    let same = std::fs::canonicalize(&source_exe).ok() == std::fs::canonicalize(&app_exe).ok();
    if same {
        println!("  {EXE_NAME}: already in place");
    } else {
        if std::fs::copy(&source_exe, &app_exe).is_err() && app_exe.is_file() {
            std::fs::rename(&app_exe, app.join(OLD_EXE_NAME))
                .with_context(|| format!("cannot replace {}", app_exe.display()))?;
            std::fs::copy(&source_exe, &app_exe)?;
        } else if !app_exe.is_file() {
            std::fs::copy(&source_exe, &app_exe)
                .with_context(|| format!("cannot copy {}", source_exe.display()))?;
        }
        println!("  {EXE_NAME}: copied");
    }

    let ui = [source_dir.join(UI_FILE), PathBuf::from("ui/index.html")]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("{UI_FILE} not found next to {EXE_NAME}"))?;
    let ui_target = app.join(UI_FILE);
    if std::fs::canonicalize(&ui).ok() != std::fs::canonicalize(&ui_target).ok() {
        std::fs::copy(&ui, &ui_target).context("cannot copy the UI file")?;
    }
    println!("  {UI_FILE}: copied");

    for name in OPTIONAL_FILES {
        let src = source_dir.join(name);
        let dst = app.join(name);
        if src.is_file() && std::fs::canonicalize(&src).ok() != std::fs::canonicalize(&dst).ok() {
            let _ = std::fs::copy(&src, &dst);
        }
    }
    std::fs::write(app.join(ICON_NAME), ICON).context("cannot write the icon")?;
    Ok(())
}

/// The elevated PowerShell script: our rule, plus the 1.x leftovers (rule and
/// URL reservation), which are harmless no-ops when they do not exist.
pub fn firewall_script(port: u16, exe: &Path, old: (u16, &str)) -> String {
    let (old_port, old_rule) = old;
    let mut s = format!(
        "$ErrorActionPreference = 'Continue'\n\
         Remove-NetFirewallRule -DisplayName '{old_rule}' -ErrorAction SilentlyContinue\n\
         netsh http delete urlacl url=http://+:{old_port}/ | Out-Null\n"
    );
    s.push_str(&format!(
        "Remove-NetFirewallRule -DisplayName '{FIREWALL_RULE}' -ErrorAction SilentlyContinue\n\
         New-NetFirewallRule -DisplayName '{FIREWALL_RULE}' -Direction Inbound -Protocol TCP -LocalPort {port} -Profile Private -Action Allow -Program '{}' | Out-Null\n\
         exit 0\n",
        exe.display()
    ));
    s
}

fn firewall_removal_script() -> String {
    format!(
        "Remove-NetFirewallRule -DisplayName '{FIREWALL_RULE}' -ErrorAction SilentlyContinue\nexit 0\n"
    )
}

fn run_elevated(script: &str, what: &str) {
    match winshell::run_elevated_powershell(script) {
        Ok(Elevated::Exit(0)) => println!("  {what}: done"),
        Ok(Elevated::Exit(code)) => println!("  {what}: the elevated step exited with code {code}"),
        Ok(Elevated::Cancelled) => {
            println!("  {what}: skipped (the administrator prompt was declined)")
        }
        Err(e) => println!("  {what}: failed - {e:#}"),
    }
}

async fn wait_for_service(port: u16) -> bool {
    let url = format!("http://localhost:{port}/api/clips");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build();
    let Ok(client) = client else { return false };
    for _ in 0..16 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if client
            .get(&url)
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return true;
        }
    }
    false
}

pub fn install(
    runtime: &tokio::runtime::Runtime,
    settings: &mut Settings,
    settings_path: &Path,
    data_dir: &Path,
    settings_existed: bool,
) -> Result<()> {
    println!("replaycut {VERSION} - install");
    let source_exe = std::env::current_exe().context("current executable")?;
    let source_dir = source_exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no folder"))?
        .to_path_buf();
    let app = winshell::app_dir();
    let app_exe = app.join(EXE_NAME);
    let icon = app.join(ICON_NAME);

    step("Stopping a running instance");
    if platform::stop_instance(settings.port, Duration::from_secs(10))? {
        println!("  stopped");
    } else {
        println!("  none running");
    }
    winshell::kill_other_instances();

    step(&format!("Installing files to {}", app.display()));
    copy_package(&source_dir, &app)?;

    let mut migration: Option<migrate::Report> = None;
    if let Some(old) = migrate::detect() {
        step("Taking over the 1.x service");
        let report = migrate::run(&old, settings, !settings_existed, data_dir)?;
        for line in &report.lines {
            println!("  - {line}");
        }
        if report.settings_changed {
            settings.save(settings_path)?;
        }
        migration = Some(report);
    }
    println!("\n  settings: {}", settings_path.display());

    step("Registering shortcuts and notifications");
    winshell::register_app_id("replaycut", &icon)?;
    for lnk in shortcut_paths()? {
        winshell::create_shortcut(
            &lnk,
            &app_exe,
            "",
            "replaycut - clip manager for the OBS replay buffer",
            &icon,
            Some(platform::APP_ID),
        )?;
        println!("  {}", lnk.display());
    }

    step("Autostart");
    let had_task = migration.as_ref().is_some_and(|r| r.had_task);
    if had_task {
        winshell::set_autostart(&app_exe)?;
        println!("  on (the 1.x service started at sign-in, so does replaycut now)");
    } else if winshell::autostart_entry().is_some() {
        winshell::set_autostart(&app_exe)?;
        println!("  stays on");
    } else if ask_yes_no("  Start replaycut automatically when you sign in?", false)? {
        winshell::set_autostart(&app_exe)?;
        println!("  on");
    } else {
        println!("  off - double-click replaycut to start it (`replaycut autostart on` to change)");
    }

    step("Firewall");
    if ask_yes_no(
        "  Allow access from other devices in your private network (phone, laptop)?",
        true,
    )? {
        let old_port = migration.as_ref().map_or(settings.port, |r| r.old_port);
        let old = (old_port, migrate::OLD_FIREWALL_RULE);
        println!("  Windows will ask for administrator permission once.");
        run_elevated(
            &firewall_script(settings.port, &app_exe, old),
            "firewall rule",
        );
    } else {
        println!("  skipped - the page is reachable on this PC only");
    }

    step("Starting replaycut");
    winshell::spawn_detached(&app_exe, &["--no-browser"])?;
    let up = runtime.block_on(wait_for_service(settings.port));
    let ui_url = format!("http://localhost:{}/", settings.port);
    if up {
        println!("  running");
        let _ = platform::open_url(&ui_url);
    } else {
        println!(
            "  not reachable after 8 s - see the log in {}",
            data_dir.join("logs").display()
        );
    }

    println!("\nDone.");
    println!("  clip folder:   {}", settings.clip_dir.display());
    println!("  this PC:       {ui_url}");
    println!(
        "  other devices: http://{}:{}/",
        platform::hostname(),
        settings.port
    );
    if !settings.integrations.nextcloud.enabled && !settings.integrations.discord.enabled {
        println!("  integrations:  none - run `replaycut setup` to add Nextcloud or Discord");
    }
    Ok(())
}

pub fn uninstall(purge: bool, port: u16, settings_path: &Path, data_dir: &Path) -> Result<()> {
    println!("replaycut {VERSION} - uninstall");
    let app = winshell::app_dir();

    step("Stopping a running instance");
    if platform::stop_instance(port, Duration::from_secs(10))? {
        println!("  stopped");
    } else {
        println!("  none running");
    }
    winshell::kill_other_instances();

    step("Removing autostart, shortcuts and notifications");
    if winshell::clear_autostart()? {
        println!("  autostart entry removed");
    }
    for lnk in shortcut_paths()? {
        if winshell::remove_file_if_present(&lnk) {
            println!("  {} removed", lnk.display());
        }
    }
    if winshell::unregister_app_id() {
        println!("  notification registration removed");
    }

    step("Firewall");
    if ask_yes_no("  Remove the firewall rule (administrator prompt)?", true)? {
        run_elevated(&firewall_removal_script(), "firewall rule");
    } else {
        println!("  kept");
    }

    step(&format!("Removing {}", app.display()));
    if app.is_dir() {
        let me = std::env::current_exe()
            .ok()
            .and_then(|p| std::fs::canonicalize(p).ok());
        let inside = match (me, std::fs::canonicalize(&app)) {
            (Some(me), Ok(app)) => me.starts_with(&app),
            _ => false,
        };
        if inside {
            // Cannot delete the running executable: leave it to a detached cmd.
            let script = format!("ping -n 3 127.0.0.1 > nul & rd /s /q \"{}\"", app.display());
            winshell::spawn_detached(Path::new("cmd.exe"), &["/c", &script])?;
            println!("  will be removed once this program exits");
        } else {
            std::fs::remove_dir_all(&app)
                .with_context(|| format!("cannot remove {}", app.display()))?;
            println!("  removed");
        }
    } else {
        println!("  not present");
    }

    if purge
        && ask_yes_no(
            "  Also delete settings, titles, history, logs and the stored credentials?",
            false,
        )?
    {
        step("Removing settings and state");
        for name in ["clip-names.json", "clip-seen.json", "clip-history.json"] {
            winshell::remove_file_if_present(&data_dir.join(name));
        }
        let _ = std::fs::remove_dir_all(data_dir.join("logs"));
        winshell::remove_file_if_present(settings_path);
        for target in [
            crate::credentials::NEXTCLOUD,
            crate::credentials::DISCORD_WEBHOOK,
        ] {
            if crate::credentials::delete(target)? {
                println!("  credential {target} removed");
            }
        }
        let _ = std::fs::remove_dir(data_dir);
        println!("  removed");
    } else {
        println!(
            "\nSettings, titles and history stay in {}; `replaycut uninstall --purge` removes them.",
            data_dir.display()
        );
    }
    println!("Your clips were not touched.");
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AutostartMode {
    On,
    Off,
    Status,
}

pub fn autostart(mode: AutostartMode) -> Result<()> {
    match mode {
        AutostartMode::On => {
            let exe = installed_exe();
            if !exe.is_file() {
                bail!(
                    "replaycut is not installed ({} missing) - run install.cmd first",
                    exe.display()
                );
            }
            winshell::set_autostart(&exe)?;
            println!("autostart on: replaycut starts when you sign in");
        }
        AutostartMode::Off => {
            if winshell::clear_autostart()? {
                println!("autostart off");
            } else {
                println!("autostart was already off");
            }
        }
        AutostartMode::Status => match winshell::autostart_entry() {
            Some(v) => println!("autostart on: {v}"),
            None => println!("autostart off"),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firewall_script_covers_new_rule_and_old_leftovers() {
        let s = firewall_script(8420, Path::new(r"C:\app\replaycut.exe"), (8420, "Old Rule"));
        assert!(s.contains("Remove-NetFirewallRule -DisplayName 'Old Rule'"));
        assert!(s.contains("netsh http delete urlacl url=http://+:8420/"));
        assert!(s.contains(
            "-LocalPort 8420 -Profile Private -Action Allow -Program 'C:\\app\\replaycut.exe'"
        ));
        assert!(s.trim_end().ends_with("exit 0"));
        let plain = firewall_script(9000, Path::new(r"C:\x.exe"), (9000, "Old Rule"));
        assert!(plain.contains("urlacl url=http://+:9000/"));
        assert!(plain.contains("-LocalPort 9000"));
    }
}
