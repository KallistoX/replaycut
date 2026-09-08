//! The Linux counterpart of `winshell`: where a per-user installation puts
//! its files (the XDG base directories and `~/.local/bin`), the desktop
//! entry and icon that stand in for the shortcuts, and the systemd user
//! unit that stands in for the Run entry. Linux only.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

pub const SERVICE_UNIT: &str = "replaycut.service";
pub const DESKTOP_ENTRY: &str = "replaycut.desktop";
/// The icon name the desktop entry and the notifications refer to.
pub const ICON_NAME: &str = "replaycut";
const ICON: &[u8] = include_bytes!("../assets/replaycut.svg");
/// The target a service of the graphical session hangs on to; the same
/// pattern the bars and applets of wlroots desktops use.
const SESSION_TARGET: &str = "graphical-session.target";

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg(var: &str, fallback: &[&str]) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => fallback.iter().fold(home(), |p, part| p.join(part)),
    }
}

/// Where the program files live: `<data-dir>/app`, the same layout as the
/// Windows installation, so the updater treats both alike.
pub fn app_dir() -> PathBuf {
    crate::settings::default_data_dir().join("app")
}

fn data_home() -> PathBuf {
    xdg("XDG_DATA_HOME", &[".local", "share"])
}

fn config_home() -> PathBuf {
    xdg("XDG_CONFIG_HOME", &[".config"])
}

/// `~/.local/bin/replaycut`, a link to the installed executable for the
/// command line (`replaycut stop`, `replaycut autostart`).
pub fn bin_link() -> PathBuf {
    home().join(".local").join("bin").join("replaycut")
}

pub fn desktop_entry_path() -> PathBuf {
    data_home().join("applications").join(DESKTOP_ENTRY)
}

pub fn icon_path() -> PathBuf {
    data_home()
        .join("icons")
        .join("hicolor")
        .join("scalable")
        .join("apps")
        .join(format!("{ICON_NAME}.svg"))
}

fn systemd_user_dir() -> PathBuf {
    config_home().join("systemd").join("user")
}

pub fn unit_path() -> PathBuf {
    systemd_user_dir().join(SERVICE_UNIT)
}

/// The symlink `systemctl --user enable` creates.
fn wants_link() -> PathBuf {
    systemd_user_dir()
        .join(format!("{SESSION_TARGET}.wants"))
        .join(SERVICE_UNIT)
}

/// A path as one argument in `Exec=` and `ExecStart=`: double quotes, with
/// the characters both formats treat specially escaped.
fn quoted(path: &Path) -> String {
    let mut out = String::from("\"");
    for c in path.to_string_lossy().chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The desktop entry: the app menu starts the service, which opens the page
/// (or, when it already runs, only opens the page).
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=replaycut\n\
         Comment=Clip manager for the OBS replay buffer\n\
         Exec={}\n\
         Icon={ICON_NAME}\n\
         Terminal=false\n\
         Categories=AudioVideo;Video;\n\
         Keywords=OBS;replay;clip;\n\
         StartupNotify=false\n",
        quoted(exe)
    )
}

/// The user unit: part of the graphical session, so it starts when the
/// desktop is up (the clipboard and the notifications need its environment)
/// and stops with it.
pub fn service_unit(exe: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=replaycut - clip manager for the OBS replay buffer\n\
         Documentation=https://github.com/KallistoX/replaycut\n\
         PartOf={SESSION_TARGET}\n\
         After={SESSION_TARGET}\n\
         \n\
         [Service]\n\
         Type=exec\n\
         ExecStart={} --no-browser\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy={SESSION_TARGET}\n",
        quoted(exe)
    )
}

fn write_file(path: &Path, text: impl AsRef<[u8]>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

pub fn remove_file_if_present(path: &Path) -> bool {
    path.symlink_metadata().is_ok() && std::fs::remove_file(path).is_ok()
}

pub fn write_desktop_entry(exe: &Path) -> Result<PathBuf> {
    let path = desktop_entry_path();
    write_file(&path, desktop_entry(exe))?;
    // Menus pick the file up on their own; the database is for MIME lookups
    // and older menus, and its absence is no error.
    if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("update-desktop-database")
            .arg(dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(path)
}

pub fn install_icon() -> Result<PathBuf> {
    let path = icon_path();
    write_file(&path, ICON)?;
    Ok(path)
}

/// Link `~/.local/bin/replaycut` to `exe`. `Ok(None)` when something that
/// is not ours sits there; it is left alone.
pub fn link_binary(exe: &Path) -> Result<Option<PathBuf>> {
    use std::os::unix::fs::symlink;
    let link = bin_link();
    match link.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(&link)
                .with_context(|| format!("cannot replace {}", link.display()))?;
        }
        Ok(_) => return Ok(None),
        Err(_) => {}
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    symlink(exe, &link).with_context(|| format!("cannot link {}", link.display()))?;
    Ok(Some(link))
}

/// Remove the link, but only when it is a link (a program of the same name
/// that the user put there stays).
pub fn unlink_binary() -> bool {
    let link = bin_link();
    link.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && std::fs::remove_file(&link).is_ok()
}

// ------------------------------------------------------------------ systemd

fn systemctl(args: &[&str]) -> Result<()> {
    let output = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .context("cannot run systemctl - autostart needs systemd's user manager")?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Write the unit for `exe` and enable it for the graphical session.
pub fn set_autostart(exe: &Path) -> Result<()> {
    write_file(&unit_path(), service_unit(exe))?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SERVICE_UNIT])?;
    Ok(())
}

/// Disable and remove the unit. `Ok(false)` when there was none.
pub fn clear_autostart() -> Result<bool> {
    let unit = unit_path();
    if !unit.is_file() && !autostart_enabled() {
        return Ok(false);
    }
    // `disable` fails when the unit file is already gone; the link is what
    // matters, and it goes either way.
    let _ = systemctl(&["disable", SERVICE_UNIT]);
    remove_file_if_present(&wants_link());
    remove_file_if_present(&unit);
    let _ = systemctl(&["daemon-reload"]);
    Ok(true)
}

/// Whether the unit is enabled: the link `enable` creates exists.
pub fn autostart_enabled() -> bool {
    wants_link().symlink_metadata().is_ok()
}

/// What `autostart status` prints when it is on.
pub fn autostart_entry() -> Option<String> {
    autostart_enabled().then(|| unit_path().display().to_string())
}

/// Start the unit now (after `set_autostart`), so the service runs under
/// systemd from the start instead of as a detached child.
pub fn start_unit() -> Result<()> {
    systemctl(&["start", SERVICE_UNIT])
}

/// Stop the unit if it runs; nothing to report when it does not.
pub fn stop_unit() {
    let _ = systemctl(&["stop", SERVICE_UNIT]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_quotes_the_executable() {
        let text = desktop_entry(Path::new("/home/you/.local/share/replaycut/app/replaycut"));
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("Exec=\"/home/you/.local/share/replaycut/app/replaycut\"\n"));
        assert!(text.contains("Icon=replaycut\n"));
        let odd = desktop_entry(Path::new("/tmp/a \"b\" $c/replaycut"));
        assert!(odd.contains("Exec=\"/tmp/a \\\"b\\\" \\$c/replaycut\"\n"));
    }

    #[test]
    fn service_unit_belongs_to_the_graphical_session() {
        let text = service_unit(Path::new("/opt/replaycut/replaycut"));
        assert!(text.contains("ExecStart=\"/opt/replaycut/replaycut\" --no-browser\n"));
        assert!(text.contains("PartOf=graphical-session.target\n"));
        assert!(text.contains("WantedBy=graphical-session.target\n"));
        assert!(text.contains("Restart=on-failure\n"));
    }
}
