//! Windows shell plumbing the installer needs: known folders, `.lnk`
//! shortcuts with an AppUserModelID, the HKCU registry (autostart entry,
//! toast registration), one elevated PowerShell step, detached process
//! start. Windows only; the installer is not offered elsewhere.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use windows::core::{Interface, GUID, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, PROPERTYKEY};
use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
use windows::Win32::System::Com::StructuredStorage::{
    PropVariantClear, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, IPersistFile, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Programs, IShellLinkW, SHGetKnownFolderPath, ShellExecuteExW,
    ShellLink, KNOWN_FOLDER_FLAG, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_WRITE};
use winreg::RegKey;

use crate::platform::APP_ID;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const AUMID_KEY: &str = r"Software\Classes\AppUserModelId\replaycut";
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

fn wide(s: &str) -> HSTRING {
    HSTRING::from(s)
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// `%LOCALAPPDATA%\replaycut\app`, where the installed files live.
pub fn app_dir() -> PathBuf {
    crate::settings::default_data_dir().join("app")
}

fn known_folder(id: &GUID) -> Result<PathBuf> {
    // SAFETY: SHGetKnownFolderPath returns a CoTaskMem string that is freed here.
    unsafe {
        let p =
            SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).context("SHGetKnownFolderPath")?;
        let s = p.to_string().context("known folder path")?;
        CoTaskMemFree(Some(p.0 as *const _));
        Ok(PathBuf::from(s))
    }
}

/// The user's Start Menu programs folder.
pub fn programs_dir() -> Result<PathBuf> {
    known_folder(&FOLDERID_Programs)
}

/// The user's desktop (respects redirection, e.g. to OneDrive).
pub fn desktop_dir() -> Result<PathBuf> {
    known_folder(&FOLDERID_Desktop)
}

fn init_com() {
    // SAFETY: CoInitializeEx on this thread; a changed-mode result only
    // means COM was already initialised differently, which is fine here.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
}

/// Write a shortcut. With `app_id` the link carries System.AppUserModel.ID,
/// which is what lets Windows show toasts for that id.
pub fn create_shortcut(
    lnk: &Path,
    target: &Path,
    arguments: &str,
    description: &str,
    icon: &Path,
    app_id: Option<&str>,
) -> Result<()> {
    init_com();
    // SAFETY: standard IShellLink/IPropertyStore/IPersistFile sequence; every
    // string outlives the call that uses it and the PROPVARIANT is cleared.
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("CoCreateInstance(ShellLink)")?;
        link.SetPath(&wide(&path_str(target)))?;
        link.SetArguments(&wide(arguments))?;
        link.SetDescription(&wide(description))?;
        if let Some(dir) = target.parent() {
            link.SetWorkingDirectory(&wide(&path_str(dir)))?;
        }
        link.SetIconLocation(&wide(&path_str(icon)), 0)?;
        if let Some(id) = app_id {
            let store: IPropertyStore = link.cast().context("IPropertyStore")?;
            let mut value: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            let mut var = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_LPWSTR,
                        wReserved1: 0,
                        wReserved2: 0,
                        wReserved3: 0,
                        Anonymous: PROPVARIANT_0_0_0 {
                            pwszVal: PWSTR(value.as_mut_ptr()),
                        },
                    }),
                },
            };
            let key: PROPERTYKEY = PKEY_AppUserModel_ID;
            let result = store.SetValue(&key, &var).and_then(|()| store.Commit());
            // The string is ours, not CoTaskMem: detach before clearing.
            (*var.Anonymous.Anonymous).Anonymous.pwszVal = PWSTR::null();
            let _ = PropVariantClear(&mut var);
            result.context("AppUserModel.ID")?;
        }
        let file: IPersistFile = link.cast().context("IPersistFile")?;
        file.Save(&wide(&path_str(lnk)), true)
            .with_context(|| format!("cannot write {}", lnk.display()))?;
    }
    Ok(())
}

pub fn remove_file_if_present(path: &Path) -> bool {
    path.is_file() && std::fs::remove_file(path).is_ok()
}

// ------------------------------------------------------------------ registry

/// The Run entry: quoted path plus `--no-browser`, so sign-in does not open a browser tab.
pub fn autostart_value(exe: &Path) -> String {
    format!("\"{}\" --no-browser", exe.display())
}

pub fn set_autostart(exe: &Path) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(RUN_KEY)
        .context("open the Run key")?;
    key.set_value(APP_ID, &autostart_value(exe))
        .context("write the Run entry")?;
    Ok(())
}

pub fn clear_autostart() -> Result<bool> {
    let key = match RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(RUN_KEY, KEY_WRITE) {
        Ok(k) => k,
        Err(_) => return Ok(false),
    };
    match key.delete_value(APP_ID) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow!("remove the Run entry: {e}")),
    }
}

/// The current Run entry, if any.
pub fn autostart_entry() -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .ok()?
        .get_value::<String, _>(APP_ID)
        .ok()
}

/// `HKCU\Software\Classes\AppUserModelId\replaycut`: name and icon Windows
/// shows on toasts from this app id.
pub fn register_app_id(display_name: &str, icon: &Path) -> Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey(AUMID_KEY)
        .context("create the AppUserModelId key")?;
    key.set_value("DisplayName", &display_name)?;
    key.set_value("IconUri", &path_str(icon))?;
    Ok(())
}

pub fn unregister_app_id() -> bool {
    RegKey::predef(HKEY_CURRENT_USER)
        .delete_subkey_all(AUMID_KEY)
        .is_ok()
}

// ------------------------------------------------------------------ processes

pub enum Elevated {
    /// The elevated script ran and exited with this code.
    Exit(u32),
    /// The user declined the UAC prompt.
    Cancelled,
}

/// Run a PowerShell script elevated (one UAC prompt) and wait for it.
pub fn run_elevated_powershell(script: &str) -> Result<Elevated> {
    let encoded = base64(&utf16le(script));
    let params = wide(&format!(
        "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -EncodedCommand {encoded}"
    ));
    let verb = wide("runas");
    let file = wide("powershell.exe");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: the struct and its strings outlive the call; the process
    // handle is waited on and closed here.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                return Ok(Elevated::Cancelled);
            }
            return Err(anyhow!("cannot start the elevated step: {e}"));
        }
        if info.hProcess.is_invalid() {
            return Err(anyhow!("elevated step started without a process handle"));
        }
        WaitForSingleObject(info.hProcess, INFINITE);
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        Ok(Elevated::Exit(code))
    }
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

/// Plain base64 (RFC 4648) for `-EncodedCommand`; small enough not to need a crate.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Start the service so that it survives the installer's console closing:
/// detached, and told not to attach to any console.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(exe)
        .args(args)
        .env("REPLAYCUT_NO_CONSOLE", "1")
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {}", exe.display()))?;
    Ok(())
}

/// Run a console tool without a window and return its stdout (lossy) and success.
pub fn run_hidden(program: &str, args: &[&str]) -> Result<(bool, String)> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()
        .with_context(|| format!("cannot run {program}"))?;
    Ok((out.status.success(), decode_console(&out.stdout)))
}

/// Console tools print UTF-16 (schtasks /XML) or the OEM code page; both
/// are close enough to ASCII for what the installer reads.
pub fn decode_console(bytes: &[u8]) -> String {
    let zeros = bytes.iter().filter(|b| **b == 0).count();
    if bytes.len() >= 2 && zeros > bytes.len() / 4 {
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// End every other replaycut.exe process (a stuck instance, an old copy).
pub fn kill_other_instances() {
    let me = std::process::id().to_string();
    let _ = run_hidden(
        "taskkill",
        &[
            "/F",
            "/FI",
            "IMAGENAME eq replaycut.exe",
            "/FI",
            &format!("PID ne {me}"),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn run_entry_is_quoted_and_silent() {
        let v = autostart_value(Path::new(
            r"C:\Users\me\AppData\Local\replaycut\app\replaycut.exe",
        ));
        assert_eq!(
            v,
            r#""C:\Users\me\AppData\Local\replaycut\app\replaycut.exe" --no-browser"#
        );
    }

    #[test]
    fn console_output_decoding() {
        assert_eq!(decode_console(b"plain"), "plain");
        let utf16: Vec<u8> = "<Task/>"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(decode_console(&utf16), "<Task/>");
    }
}
