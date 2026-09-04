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

#[cfg(windows)]
const MUTEX_NAME: &str = "Global\\replaycut";
#[cfg(windows)]
const STOP_EVENT_NAME: &str = "Global\\replaycut-stop";

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

    use super::{APP_ID, MUTEX_NAME, STOP_EVENT_NAME};

    fn wide(s: &str) -> HSTRING {
        HSTRING::from(s)
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

    pub fn claim_single_instance() -> Result<Option<SingleInstance>> {
        // SAFETY: CreateMutexW with a constant name and default security.
        unsafe {
            let h = CreateMutexW(None, false, &wide(MUTEX_NAME))
                .with_context(|| format!("cannot create {MUTEX_NAME}"))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                return Ok(None);
            }
            Ok(Some(SingleInstance(h.0 as isize)))
        }
    }

    /// True while a service process holds the single-instance mutex.
    pub fn instance_running() -> bool {
        // SAFETY: OpenMutexW with a constant name; the handle is closed at once.
        unsafe {
            match OpenMutexW(
                SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000),
                false,
                &wide(MUTEX_NAME),
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
        pub fn create() -> Result<Self> {
            // SAFETY: CreateEventW with a constant name; manual reset, not signalled.
            let h = unsafe { CreateEventW(None, true, false, &wide(STOP_EVENT_NAME)) }
                .with_context(|| format!("cannot create {STOP_EVENT_NAME}"))?;
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
    pub fn signal_stop() -> Result<bool> {
        // SAFETY: OpenEventW/SetEvent with a constant name; the handle is closed at once.
        unsafe {
            let h = match OpenEventW(EVENT_MODIFY_STATE, false, &wide(STOP_EVENT_NAME)) {
                Ok(h) => h,
                Err(e) if e.code() == ERROR_FILE_NOT_FOUND.to_hresult() => return Ok(false),
                Err(e) => return Err(anyhow!("cannot open {STOP_EVENT_NAME}: {e}")),
            };
            let r = SetEvent(h);
            let _ = CloseHandle(h);
            r.with_context(|| format!("cannot signal {STOP_EVENT_NAME}"))?;
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
    attach_parent_console, claim_single_instance, fatal_dialog, instance_running, open_url,
    press_f9, set_app_id, signal_stop, StopEvent,
};

// ------------------------------------------------------------ other platforms

#[cfg(not(windows))]
mod other {
    use anyhow::Result;

    pub fn press_f9() -> Result<()> {
        anyhow::bail!("sending the replay hotkey is only supported on Windows")
    }

    pub fn attach_parent_console() -> bool {
        true
    }

    pub fn set_app_id() {}

    pub struct SingleInstance;

    pub fn claim_single_instance() -> Result<Option<SingleInstance>> {
        Ok(Some(SingleInstance))
    }

    pub fn instance_running() -> bool {
        false
    }

    pub struct StopEvent;

    impl StopEvent {
        pub fn create() -> Result<Self> {
            Ok(Self)
        }
        pub fn wait(&self) {
            loop {
                std::thread::park();
            }
        }
    }

    pub fn signal_stop() -> Result<bool> {
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
    attach_parent_console, claim_single_instance, fatal_dialog, instance_running, open_url,
    press_f9, set_app_id, signal_stop, StopEvent,
};

/// Ask a running instance to stop and wait for it. `Ok(false)` when none ran.
pub fn stop_instance(timeout: std::time::Duration) -> Result<bool> {
    if !signal_stop()? {
        return Ok(false);
    }
    let started = std::time::Instant::now();
    while instance_running() {
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
