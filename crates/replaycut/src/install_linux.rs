//! `replaycut install`, `replaycut uninstall`, `replaycut autostart` on
//! Linux: the per-user installation under `$XDG_DATA_HOME/replaycut/app`
//! (the layout of the Windows installation), a link in `~/.local/bin`, the
//! desktop entry and icon, and the optional systemd user unit. No root; a
//! firewall, if one runs, is the user's to open. Linux only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::linuxshell;
use crate::platform;
use crate::settings::Settings;
use crate::setup::ask_yes_no;
use crate::state::VERSION;

const EXE_NAME: &str = "replaycut";
const UI_FILE: &str = "ui/index.html";
const OPTIONAL_FILES: [&str; 5] = [
    "README.md",
    "LICENSE",
    "CHANGELOG.md",
    "install.sh",
    "uninstall.sh",
];

/// Where the installed executable lives.
pub fn installed_exe() -> PathBuf {
    linuxshell::app_dir().join(EXE_NAME)
}

fn step(text: &str) {
    println!("\n== {text}");
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Copy the package into the app folder. A running executable can be
/// replaced on Linux: the old inode lives on until the process exits.
fn copy_package(source_dir: &Path, app: &Path) -> Result<()> {
    std::fs::create_dir_all(app.join("ui"))
        .with_context(|| format!("cannot create {}", app.display()))?;

    let source_exe = source_dir.join(EXE_NAME);
    let app_exe = app.join(EXE_NAME);
    if same_file(&source_exe, &app_exe) {
        println!("  {EXE_NAME}: already in place");
    } else {
        // unlink first so a running copy keeps its file and the new one
        // starts fresh with the source's permissions
        let _ = std::fs::remove_file(&app_exe);
        std::fs::copy(&source_exe, &app_exe)
            .with_context(|| format!("cannot copy {}", source_exe.display()))?;
        println!("  {EXE_NAME}: copied");
    }

    let ui = [source_dir.join(UI_FILE), PathBuf::from(UI_FILE)]
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| anyhow!("{UI_FILE} not found next to {EXE_NAME}"))?;
    let ui_target = app.join(UI_FILE);
    if !same_file(&ui, &ui_target) {
        std::fs::copy(&ui, &ui_target).context("cannot copy the UI file")?;
    }
    println!("  {UI_FILE}: copied");

    for name in OPTIONAL_FILES {
        let src = source_dir.join(name);
        let dst = app.join(name);
        if src.is_file() && !same_file(&src, &dst) {
            let _ = std::fs::copy(&src, &dst);
        }
    }
    Ok(())
}

async fn wait_for_service(settings: &Settings, data_dir: &Path) -> bool {
    let url = format!("{}/api/clips", crate::tls::local_base(settings));
    let client = crate::tls::local_client(data_dir, settings, Duration::from_secs(2));
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
) -> Result<()> {
    println!("replaycut {VERSION} - install");
    if linuxshell::is_package_copy() {
        println!(
            "\nThis copy was installed by your package manager, together with its\n\
             desktop entry and user unit - nothing to do here. Updates come with\n\
             the package; `replaycut autostart on` (or the switch in Settings)\n\
             starts it with your desktop session."
        );
        return Ok(());
    }
    let source_exe = std::env::current_exe().context("current executable")?;
    let source_dir = source_exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no folder"))?
        .to_path_buf();
    let app = linuxshell::app_dir();
    let app_exe = app.join(EXE_NAME);

    step("Stopping a running instance");
    linuxshell::stop_unit();
    if platform::stop_instance(settings.port, Duration::from_secs(10))? {
        println!("  stopped");
    } else {
        println!("  none running");
    }

    step(&format!("Installing files to {}", app.display()));
    copy_package(&source_dir, &app)?;
    match linuxshell::link_binary(&app_exe)? {
        Some(link) => println!("  {} -> {}", link.display(), app_exe.display()),
        None => println!(
            "  {} exists and is not a link - left alone",
            linuxshell::bin_link().display()
        ),
    }
    println!("\n  settings: {}", settings_path.display());

    step("Desktop entry and icon");
    println!("  {}", linuxshell::install_icon()?.display());
    println!("  {}", linuxshell::write_desktop_entry(&app_exe)?.display());

    step("Autostart");
    let autostart = if linuxshell::autostart_enabled() {
        linuxshell::set_autostart(&app_exe)?;
        println!("  stays on");
        true
    } else if ask_yes_no(
        "  Start replaycut automatically with your desktop session?",
        false,
    )? {
        linuxshell::set_autostart(&app_exe)?;
        println!("  on (systemd user unit {})", linuxshell::SERVICE_UNIT);
        true
    } else {
        println!("  off - start replaycut from the app menu (`replaycut autostart on` to change)");
        false
    };

    // Since 2.8 an installation is reachable on this PC only; access from
    // other devices is turned on in the browser. Linux has no firewall
    // step: most desktops run none, and the ones that do differ.
    step("Firewall");
    println!("  not needed - replaycut listens on this PC only.");
    println!("  Open it to your phone in the browser: Settings > Access.");
    println!(
        "  If a firewall runs on this PC, allow TCP port {} there.",
        settings.port
    );

    step("Starting replaycut");
    if autostart && linuxshell::start_unit().is_ok() {
        println!("  started as {}", linuxshell::SERVICE_UNIT);
    } else {
        platform::spawn_detached(&app_exe, &["--no-browser"])?;
    }
    let up = runtime.block_on(wait_for_service(settings, data_dir));
    let ui_url = format!("{}/", crate::tls::local_base(settings));
    // A fresh installation lands in the browser setup; an updated one on the clips.
    let open_url = if settings.setup_done {
        ui_url.clone()
    } else {
        format!("{ui_url}setup")
    };
    if up {
        println!("  running");
        let _ = platform::open_url(&open_url);
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
        platform::lan_host(),
        settings.port
    );
    if !settings.setup_done {
        println!("  setup:         {ui_url}setup (opened in the browser)");
    } else if !settings.integrations.nextcloud.enabled && !settings.integrations.discord.enabled {
        println!("  integrations:  none - add Nextcloud or Discord under {ui_url}settings");
    }
    Ok(())
}

pub fn uninstall(purge: bool, port: u16, settings_path: &Path, data_dir: &Path) -> Result<()> {
    println!("replaycut {VERSION} - uninstall");
    let app = linuxshell::app_dir();
    let package = linuxshell::is_package_copy();
    if package && !purge {
        println!(
            "\nThis copy was installed by your package manager - remove the package\n\
             with it. `replaycut uninstall --purge` still deletes the settings, state,\n\
             logs and stored credentials of this user."
        );
        return Ok(());
    }

    step("Stopping a running instance");
    linuxshell::stop_unit();
    if platform::stop_instance(port, Duration::from_secs(10))? {
        println!("  stopped");
    } else {
        println!("  none running");
    }

    if package {
        // the files are the package's; only the autostart link is this user's
        step("Removing autostart");
        if linuxshell::clear_autostart()? {
            println!("  {} disabled", linuxshell::SERVICE_UNIT);
        }
        println!(
            "\nThe program files belong to the package - remove it with your package manager."
        );
    } else {
        step("Removing autostart, desktop entry and icon");
        if linuxshell::clear_autostart()? {
            println!("  {} removed", linuxshell::SERVICE_UNIT);
        }
        for path in [linuxshell::desktop_entry_path(), linuxshell::icon_path()] {
            if linuxshell::remove_file_if_present(&path) {
                println!("  {} removed", path.display());
            }
        }
        if linuxshell::unlink_binary() {
            println!("  {} removed", linuxshell::bin_link().display());
        }

        step(&format!("Removing {}", app.display()));
        if app.is_dir() {
            // the running executable may be in there; Linux lets it go
            std::fs::remove_dir_all(&app)
                .with_context(|| format!("cannot remove {}", app.display()))?;
            println!("  removed");
        } else {
            println!("  not present");
        }
    }

    if purge
        && ask_yes_no(
            "  Also delete settings, titles, history, logs and the stored credentials?",
            false,
        )?
    {
        step("Removing settings and state");
        for name in [
            crate::db::FILE,
            // SQLite's write-ahead log, when the store was not closed cleanly
            "replaycut.db-wal",
            "replaycut.db-shm",
        ] {
            linuxshell::remove_file_if_present(&data_dir.join(name));
        }
        // the state files of 2.x, still in the data directory or already in
        // the backup the first start of 3.0 made
        for name in crate::db::STATE_FILES {
            linuxshell::remove_file_if_present(&data_dir.join(name));
        }
        let _ = std::fs::remove_dir_all(data_dir.join(crate::db::BACKUP_DIR));
        let _ = std::fs::remove_dir_all(data_dir.join("logs"));
        linuxshell::remove_file_if_present(settings_path);
        for target in [
            crate::credentials::NEXTCLOUD,
            crate::credentials::DISCORD_WEBHOOK,
            crate::credentials::OBS_WEBSOCKET,
            crate::credentials::ONEDRIVE,
            crate::credentials::S3,
            crate::credentials::WEBDAV,
            crate::credentials::YOUTUBE,
            crate::credentials::YOUTUBE_CLIENT,
            crate::credentials::TELEGRAM,
            crate::credentials::WEBHOOK_SECRET,
        ] {
            match crate::credentials::delete(target) {
                Ok(true) => println!("  credential {target} removed"),
                Ok(false) => {}
                Err(e) => println!("  credential {target}: {e:#}"),
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
            // a package's copy brings its own unit; ours needs the installed one
            let exe = if linuxshell::is_package_copy() {
                std::env::current_exe().context("current executable")?
            } else {
                installed_exe()
            };
            if !exe.is_file() {
                anyhow::bail!(
                    "replaycut is not installed ({} missing) - run install.sh first",
                    exe.display()
                );
            }
            linuxshell::set_autostart(&exe)?;
            println!(
                "autostart on: replaycut starts with your desktop session ({})",
                linuxshell::autostart_entry().unwrap_or_else(|| linuxshell::SERVICE_UNIT.into())
            );
        }
        AutostartMode::Off => {
            if linuxshell::clear_autostart()? {
                println!("autostart off");
            } else {
                println!("autostart was already off");
            }
        }
        AutostartMode::Status => match linuxshell::autostart_entry() {
            Some(v) => println!("autostart on: {v}"),
            None => println!("autostart off"),
        },
    }
    Ok(())
}
