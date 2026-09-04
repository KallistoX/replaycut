//! Windows integrations that need no installer: recycle bin, the replay
//! hotkey, the clipboard, the parent console for a windowless executable,
//! the single-instance mutex, the stop event, the browser. Other platforms
//! get honest errors or no-ops so the service still builds and runs there
//! for development.

use std::path::Path;

use anyhow::Result;

/// Application user model id: the toast registration and the start menu
/// shortcut (both written by `replaycut install`) use the same id.
pub const APP_ID: &str = "replaycut";

/// Kernel object names carry the port, so a test instance on another port
/// can run next to the installed one; `replaycut stop` reads the same
/// settings and therefore uses the same names.
#[cfg(windows)]
fn mutex_name(port: u16) -> String {
    format!("Global\\replaycut-{port}")
}

#[cfg(windows)]
fn stop_event_name(port: u16) -> String {
    format!("Global\\replaycut-stop-{port}")
}

/// Move a file to the recycle bin (never a permanent delete).
pub fn recycle(path: &Path) -> Result<()> {
    trash::delete(path).map_err(|e| anyhow::anyhow!("recycle {}: {e}", path.display()))
}

/// Put text into the clipboard (the direct link after a share).
pub fn copy_text(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_string())?;
    Ok(())
}

/// The IPv4 address other devices reach this machine at: the source
/// address of a UDP socket "connected" to a documentation address (no
/// packet is sent).
pub fn primary_ipv4() -> Option<std::net::Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

/// Start a second copy of this process with the same arguments plus
/// `--no-browser --wait-for-exit`; it waits until this one has released
/// the single-instance mutex and then takes over.
pub fn spawn_self_for_restart() -> Result<()> {
    use anyhow::Context as _;
    let exe = std::env::current_exe().context("current executable")?;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    for flag in ["--no-browser", "--wait-for-exit"] {
        if !args.iter().any(|a| a == flag) {
            args.push(flag.to_string());
        }
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    #[cfg(windows)]
    {
        crate::winshell::spawn_detached(&exe, &refs)
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new(&exe)
            .args(&refs)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", exe.display()))?;
        Ok(())
    }
}

/// Working set of this process in MB (Windows only).
#[cfg(windows)]
pub fn process_memory_mb() -> Option<u64> {
    use windows::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the struct is sized and zeroed; the call fills it.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, size) }.as_bool();
    ok.then_some(counters.WorkingSetSize as u64 / 1_048_576)
}
#[cfg(not(windows))]
pub fn process_memory_mb() -> Option<u64> {
    None
}

/// Free bytes on the volume of `dir`.
#[cfg(windows)]
pub fn free_space(dir: &Path) -> Option<u64> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut free = 0u64;
    // SAFETY: out-pointers to locals; the path is a valid wide string.
    let ok = unsafe {
        GetDiskFreeSpaceExW(&HSTRING::from(dir.as_os_str()), Some(&mut free), None, None)
    }
    .is_ok();
    ok.then_some(free)
}
#[cfg(not(windows))]
pub fn free_space(_dir: &Path) -> Option<u64> {
    None
}

/// Whether the sign-in entry exists (Windows only).
#[cfg(windows)]
pub fn autostart_enabled() -> bool {
    crate::winshell::autostart_entry().is_some()
}
#[cfg(not(windows))]
pub fn autostart_enabled() -> bool {
    false
}

/// `Some(true)` when the firewall rule "replaycut" exists, `None` when the
/// question cannot be answered on this platform.
#[cfg(windows)]
pub fn firewall_rule_present() -> Option<bool> {
    crate::winshell::run_hidden(
        "netsh",
        &["advfirewall", "firewall", "show", "rule", "name=replaycut"],
    )
    .ok()
    .map(|(ok, out)| ok && out.to_ascii_lowercase().contains("replaycut"))
}
#[cfg(not(windows))]
pub fn firewall_rule_present() -> Option<bool> {
    None
}

/// Lower-case computer name for the address other devices use.
pub fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|h| h.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "localhost".to_string())
}

// ---------------------------------------------------------------- Windows

#[cfg(windows)]
mod win {
    use anyhow::{anyhow, Context, Result};
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, HANDLE, HWND,
        INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileType, FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_UNKNOWN, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::Threading::{
        CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject,
        EVENT_MODIFY_STATE, INFINITE, SYNCHRONIZATION_ACCESS_RIGHTS,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };
    use windows::Win32::UI::Shell::{SetCurrentProcessExplicitAppUserModelID, ShellExecuteW};
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND, SW_SHOWNORMAL,
    };

    use super::{mutex_name, stop_event_name, APP_ID};

    fn wide(s: &str) -> HSTRING {
        HSTRING::from(s)
    }

    /// Put a file into the clipboard as a file object (CF_HDROP), so Ctrl+V
    /// in Discord or Explorer pastes the file itself.
    pub fn copy_file(path: &std::path::Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::GlobalFree;
        use windows::Win32::System::DataExchange::{
            CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
        };
        use windows::Win32::System::Memory::{
            GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
        };
        const CF_HDROP: u32 = 15;
        const DROPFILES_LEN: usize = 20; // pFiles u32, pt POINT, fNC BOOL, fWide BOOL
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0u16, 0u16]).collect();
        let bytes = DROPFILES_LEN + wide.len() * 2;
        // SAFETY: a fresh moveable global block of `bytes` bytes is filled
        // with a DROPFILES header and the wide path; ownership moves to the
        // clipboard on success and is freed here on failure.
        unsafe {
            let block = GlobalAlloc(GMEM_MOVEABLE, bytes).context("GlobalAlloc")?;
            let p = GlobalLock(block) as *mut u8;
            if p.is_null() {
                let _ = GlobalFree(Some(block));
                anyhow::bail!("GlobalLock failed");
            }
            std::ptr::write_bytes(p, 0, bytes);
            std::ptr::write_unaligned(p as *mut u32, DROPFILES_LEN as u32);
            std::ptr::write_unaligned(p.add(16) as *mut i32, 1); // fWide
            std::ptr::copy_nonoverlapping(
                wide.as_ptr() as *const u8,
                p.add(DROPFILES_LEN),
                wide.len() * 2,
            );
            let _ = GlobalUnlock(block);
            let mut opened = Err(anyhow!("clipboard busy"));
            for _ in 0..5 {
                if OpenClipboard(None).is_ok() {
                    opened = Ok(());
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if let Err(e) = opened {
                let _ = GlobalFree(Some(block));
                return Err(e);
            }
            let result = EmptyClipboard().context("EmptyClipboard").and_then(|()| {
                SetClipboardData(CF_HDROP, Some(HANDLE(block.0)))
                    .map(|_| ())
                    .context("SetClipboardData")
            });
            let _ = CloseClipboard();
            if let Err(e) = result {
                let _ = GlobalFree(Some(block));
                return Err(e);
            }
        }
        Ok(())
    }

    /// Press F9 with a 250 ms hold, which OBS registers as its global hotkey.
    pub fn press_f9() -> Result<()> {
        const VK_F9: u8 = 0x78;
        // SAFETY: plain Win32 calls with constant arguments; no pointers involved.
        unsafe {
            keybd_event(VK_F9, 0, KEYBD_EVENT_FLAGS(0), 0);
            std::thread::sleep(std::time::Duration::from_millis(250));
            keybd_event(VK_F9, 0, KEYEVENTF_KEYUP, 0);
        }
        Ok(())
    }

    /// The executable is built without a console window. When it was started
    /// from a terminal, attach to that terminal so `--help`, `setup`, `test`
    /// and log lines are visible there. Returns `false` when there is no
    /// parent console (double-click, shortcut, sign-in).
    pub fn attach_parent_console() -> bool {
        // SAFETY: AttachConsole has no preconditions; a failure only means
        // there is no parent console.
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) }.is_err() {
            return false;
        }
        reopen_std_handle(
            STD_INPUT_HANDLE,
            "CONIN$",
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
        );
        reopen_std_handle(STD_OUTPUT_HANDLE, "CONOUT$", FILE_GENERIC_WRITE.0);
        reopen_std_handle(STD_ERROR_HANDLE, "CONOUT$", FILE_GENERIC_WRITE.0);
        true
    }

    /// A GUI process starts without usable standard handles unless the caller
    /// redirected them (a pipe). Keep a redirected handle, otherwise open the
    /// console device so the Rust standard streams reach the terminal.
    fn reopen_std_handle(which: STD_HANDLE, device: &str, access: u32) {
        // SAFETY: handle queries and CreateFileW on a device name; the handle
        // stays open for the life of the process on purpose.
        unsafe {
            let current = GetStdHandle(which).unwrap_or_default();
            let usable = !current.is_invalid()
                && current != INVALID_HANDLE_VALUE
                && GetFileType(current) != FILE_TYPE_UNKNOWN;
            if usable {
                return;
            }
            if let Ok(h) = CreateFileW(
                &wide(device),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            ) {
                let _ = SetStdHandle(which, h);
            }
        }
    }

    /// Tell Windows which app this process is; toasts and the taskbar group by it.
    pub fn set_app_id() {
        // SAFETY: constant wide string, no other preconditions.
        if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&wide(APP_ID)) } {
            tracing::debug!("SetCurrentProcessExplicitAppUserModelID: {e}");
        }
    }

    /// Held by the running service; a second start sees `None`.
    pub struct SingleInstance(isize);

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateMutexW and is closed once.
            unsafe {
                let _ = CloseHandle(HANDLE(self.0 as _));
            }
        }
    }

    pub fn claim_single_instance(port: u16) -> Result<Option<SingleInstance>> {
        let name = mutex_name(port);
        // SAFETY: CreateMutexW with an owned name and default security.
        unsafe {
            let h = CreateMutexW(None, false, &wide(&name))
                .with_context(|| format!("cannot create {name}"))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return Ok(None);
            }
            Ok(Some(SingleInstance(h.0 as isize)))
        }
    }

    /// True while a service process holds the single-instance mutex.
    pub fn instance_running(port: u16) -> bool {
        // SAFETY: OpenMutexW with an owned name; the handle is closed at once.
        unsafe {
            match OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                false,
                &wide(&mutex_name(port)),
            ) {
                Ok(h) => {
                    let _ = CloseHandle(h);
                    true
                }
                Err(_) => false,
            }
        }
    }

    /// Named event `replaycut stop` sets; the service waits on it.
    pub struct StopEvent(isize);

    // SAFETY: a kernel handle may be used from any thread.
    unsafe impl Send for StopEvent {}

    impl StopEvent {
        pub fn create(port: u16) -> Result<Self> {
            let name = stop_event_name(port);
            // SAFETY: CreateEventW with an owned name; manual reset, not signalled.
            let h = unsafe { CreateEventW(None, true, false, &wide(&name)) }
                .with_context(|| format!("cannot create {name}"))?;
            Ok(Self(h.0 as isize))
        }

        /// Block until the event is signalled.
        pub fn wait(&self) {
            // SAFETY: the handle is valid for the life of self.
            let r = unsafe { WaitForSingleObject(HANDLE(self.0 as _), INFINITE) };
            if r != WAIT_OBJECT_0 {
                tracing::warn!("waiting for the stop event failed: {r:?}");
                loop {
                    std::thread::park();
                }
            }
        }
    }

    impl Drop for StopEvent {
        fn drop(&mut self) {
            // SAFETY: the handle came from CreateEventW and is closed once.
            unsafe {
                let _ = CloseHandle(HANDLE(self.0 as _));
            }
        }
    }

    /// Signal the running service to stop. `Ok(false)` when none is running.
    pub fn signal_stop(port: u16) -> Result<bool> {
        let name = stop_event_name(port);
        // SAFETY: OpenEventW/SetEvent with an owned name; the handle is closed at once.
        unsafe {
            let h = match OpenEventW(EVENT_MODIFY_STATE, false, &wide(&name)) {
                Ok(h) => h,
                Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(false),
                Err(e) => return Err(anyhow!("cannot open {name}: {e}")),
            };
            let r = SetEvent(h);
            let _ = CloseHandle(h);
            r.with_context(|| format!("cannot signal {name}"))?;
            Ok(true)
        }
    }

    /// Open a URL in the default browser.
    pub fn open_url(url: &str) -> Result<()> {
        let verb = wide("open");
        let target = wide(url);
        // SAFETY: ShellExecuteW with wide strings that outlive the call.
        let r = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(target.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        if r.0 as isize > 32 {
            Ok(())
        } else {
            Err(anyhow!("ShellExecute returned {}", r.0 as isize))
        }
    }

    /// Modal error box for fatal start-up errors when there is no console.
    pub fn fatal_dialog(text: &str) {
        let text = wide(text);
        let title = wide("replaycut");
        // SAFETY: MessageBoxW with wide strings that outlive the call.
        unsafe {
            MessageBoxW(
                None::<HWND>,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
            );
        }
    }
}

#[cfg(windows)]
pub use win::{
    attach_parent_console, claim_single_instance, copy_file, fatal_dialog, instance_running,
    open_url, press_f9, set_app_id, signal_stop, StopEvent,
};

/// Open Explorer with the file selected.
#[cfg(windows)]
pub fn open_folder_select(path: &Path) -> Result<()> {
    use anyhow::Context as _;
    std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .context("cannot start explorer.exe")?;
    Ok(())
}

#[cfg(not(windows))]
pub fn open_folder_select(_path: &Path) -> Result<()> {
    anyhow::bail!("opening the folder is only supported on Windows")
}

// ------------------------------------------------------------ other platforms

#[cfg(not(windows))]
mod other {
    use anyhow::Result;

    pub fn press_f9() -> Result<()> {
        anyhow::bail!("sending the replay hotkey is only supported on Windows")
    }

    pub fn copy_file(_path: &std::path::Path) -> Result<()> {
        anyhow::bail!("copying a file object is only supported on Windows")
    }

    pub fn attach_parent_console() -> bool {
        true
    }

    pub fn set_app_id() {}

    pub struct SingleInstance;

    pub fn claim_single_instance(_port: u16) -> Result<Option<SingleInstance>> {
        Ok(Some(SingleInstance))
    }

    pub fn instance_running(_port: u16) -> bool {
        false
    }

    pub struct StopEvent;

    impl StopEvent {
        pub fn create(_port: u16) -> Result<Self> {
            Ok(Self)
        }
        pub fn wait(&self) {
            loop {
                std::thread::park();
            }
        }
    }

    pub fn signal_stop(_port: u16) -> Result<bool> {
        anyhow::bail!("`replaycut stop` is only supported on Windows")
    }

    pub fn open_url(url: &str) -> Result<()> {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }

    pub fn fatal_dialog(text: &str) {
        eprintln!("{text}");
    }
}

#[cfg(not(windows))]
pub use other::{
    attach_parent_console, claim_single_instance, copy_file, fatal_dialog, instance_running,
    open_url, press_f9, set_app_id, signal_stop, StopEvent,
};

/// Ask a running instance to stop and wait for it. `Ok(false)` when none ran.
pub fn stop_instance(port: u16, timeout: std::time::Duration) -> Result<bool> {
    if !signal_stop(port)? {
        return Ok(false);
    }
    let started = std::time::Instant::now();
    while instance_running(port) {
        if started.elapsed() > timeout {
            anyhow::bail!(
                "replaycut is still running {} s after the stop request",
                timeout.as_secs()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn hostname_is_lower_case_and_non_empty() {
        let h = super::hostname();
        assert!(!h.is_empty());
        assert_eq!(h, h.to_ascii_lowercase());
    }
}
