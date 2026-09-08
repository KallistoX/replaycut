//! Platform integrations that need no installer: recycle bin, the replay
//! hotkey, the clipboard, the parent console for a windowless executable,
//! the single-instance guard, `replaycut stop`, the browser. Windows and
//! Linux have real implementations (`win`, `linux`); other platforms get
//! honest errors or no-ops so the service still builds and runs there for
//! development.

use std::path::Path;

use anyhow::Result;

/// Application user model id: the toast registration and the start menu
/// shortcut (both written by `replaycut install`) use the same id.
#[cfg(windows)]
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
#[cfg(not(target_os = "linux"))]
pub fn copy_text(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new()?;
    clipboard.set_text(text.to_string())?;
    Ok(())
}

/// Put text into the clipboard (the direct link after a share).
#[cfg(target_os = "linux")]
pub fn copy_text(text: &str) -> Result<()> {
    linux::copy_text(text)
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
    spawn_detached(&exe, &refs)
}

/// Start `exe` so that it outlives this process and its terminal: without a
/// console on Windows, in a session of its own on Linux.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> Result<()> {
    #[cfg(windows)]
    {
        crate::winshell::spawn_detached(exe, args)
    }
    #[cfg(not(windows))]
    {
        use anyhow::Context as _;
        let mut cmd = std::process::Command::new(exe);
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(target_os = "linux")]
        linux::own_session(&mut cmd);
        cmd.spawn()
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
/// Resident set of this process in MB, from `/proc/self/status`.
#[cfg(target_os = "linux")]
pub fn process_memory_mb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kb: u64 = status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb / 1024)
}
#[cfg(not(any(windows, target_os = "linux")))]
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
/// Free bytes on the file system of `dir`, as an unprivileged user sees them.
#[cfg(target_os = "linux")]
pub fn free_space(dir: &Path) -> Option<u64> {
    let vfs = rustix::fs::statvfs(dir).ok()?;
    Some(vfs.f_bavail.saturating_mul(vfs.f_frsize))
}
#[cfg(not(any(windows, target_os = "linux")))]
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

/// Lower-case computer name for the address other devices use. Windows
/// exports it as `COMPUTERNAME`; Linux shells do not export `HOSTNAME`, so
/// the kernel is asked first there.
pub fn hostname() -> String {
    #[cfg(target_os = "linux")]
    if let Some(name) = linux::hostname() {
        return name;
    }
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

/// Ask the file manager to show the file selected (the freedesktop
/// `FileManager1` interface every major file manager implements); when
/// nothing answers, open the folder itself.
#[cfg(target_os = "linux")]
pub fn open_folder_select(path: &Path) -> Result<()> {
    if let Err(e) = linux::show_in_file_manager(path) {
        tracing::debug!("FileManager1.ShowItems failed: {e:#} - opening the folder instead");
        let folder = path.parent().unwrap_or(path);
        return open_url(&folder.to_string_lossy());
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn open_folder_select(_path: &Path) -> Result<()> {
    anyhow::bail!("opening the folder is only supported on Windows and Linux")
}

// ------------------------------------------------------------------ Linux

/// The single-instance guard and `replaycut stop` share one lock file:
/// the service holds an exclusive `flock` on it with its PID inside, a
/// second start sees the lock taken, and `stop` reads the PID and sends
/// SIGTERM, which `lifecycle::console_signals` turns into a shutdown. The
/// kernel drops the lock with the process, so a crash leaves nothing stale.
#[cfg(target_os = "linux")]
pub mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::path::PathBuf;

    use anyhow::{Context, Result};
    use rustix::fs::{flock, FlockOperation};
    use rustix::process::{kill_process, Pid, Signal};

    /// `$XDG_RUNTIME_DIR/replaycut-<port>.lock`, else the temp dir; the port
    /// keeps a test instance apart from the installed one.
    fn lock_path(port: u16) -> PathBuf {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|d| d.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        dir.join(format!("replaycut-{port}.lock"))
    }

    fn open_lock(port: u16) -> Result<File> {
        let path = lock_path(port);
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))
    }

    /// Take the lock without waiting; `false` while another process holds it.
    fn try_lock(file: &File) -> bool {
        flock(file, FlockOperation::NonBlockingLockExclusive).is_ok()
    }

    /// Held by the running service; closing the file releases the lock.
    pub struct SingleInstance(#[allow(dead_code)] File);

    pub fn claim_single_instance(port: u16) -> Result<Option<SingleInstance>> {
        let mut file = open_lock(port)?;
        if !try_lock(&file) {
            return Ok(None);
        }
        file.set_len(0).context("cannot truncate the lock file")?;
        write!(file, "{}", std::process::id()).context("cannot write the lock file")?;
        Ok(Some(SingleInstance(file)))
    }

    /// True while a service process holds the lock.
    pub fn instance_running(port: u16) -> bool {
        match open_lock(port) {
            Ok(file) => !try_lock(&file),
            Err(_) => false,
        }
    }

    /// Send SIGTERM to the process that holds the lock. `Ok(false)` when
    /// none does.
    pub fn signal_stop(port: u16) -> Result<bool> {
        let mut file = open_lock(port)?;
        if try_lock(&file) {
            return Ok(false);
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("cannot read {}", lock_path(port).display()))?;
        let pid = text
            .trim()
            .parse::<i32>()
            .ok()
            .and_then(Pid::from_raw)
            .with_context(|| format!("no pid in {}", lock_path(port).display()))?;
        kill_process(pid, Signal::TERM)
            .with_context(|| format!("cannot signal process {}", pid.as_raw_nonzero()))?;
        Ok(true)
    }

    /// Whether a terminal is attached: the counterpart of the Windows parent
    /// console. Started from a desktop entry or systemd there is none, and
    /// the service then opens the browser like the Windows shortcut does.
    pub fn attach_parent_console() -> bool {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal() || std::io::stderr().is_terminal()
    }

    /// The kernel's node name, lower-cased; `None` when it is empty.
    pub fn hostname() -> Option<String> {
        let uname = rustix::system::uname();
        let name = uname.nodename().to_str().ok()?.trim();
        (!name.is_empty()).then(|| name.to_ascii_lowercase())
    }

    /// Put the child into a session of its own so that a closing terminal
    /// (SIGHUP) or the parent's exit does not take it along.
    pub fn own_session(cmd: &mut std::process::Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no memory shared
        // with the parent, which is all pre_exec asks for.
        unsafe {
            cmd.pre_exec(|| rustix::process::setsid().map(|_| ()).map_err(Into::into));
        }
    }

    /// Run `f` on a thread of its own and wait for it. The D-Bus crates
    /// (zbus and everything on it) drive their traffic on a runtime of
    /// their own and refuse to block a tokio worker thread, and the
    /// callers here are synchronous functions that async code calls
    /// directly, so this is where the two are kept apart.
    pub fn off_runtime<T: Send>(f: impl FnOnce() -> Result<T> + Send) -> Result<T> {
        std::thread::scope(|s| {
            s.spawn(f)
                .join()
                .map_err(|_| anyhow::anyhow!("the D-Bus call panicked"))?
        })
    }

    /// The clipboard on X11 and Wayland is a promise: the owner serves the
    /// content until another program takes the clipboard over. A thread
    /// keeps serving this text, so a paste after the service moved on
    /// still finds it.
    pub fn copy_text(text: &str) -> Result<()> {
        use arboard::SetExtLinux;
        let mut clipboard = arboard::Clipboard::new()?;
        let text = text.to_string();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || {
                if let Err(e) = clipboard.set().wait().text(text) {
                    tracing::debug!("clipboard: {e}");
                }
            })
            .context("cannot start the clipboard thread")?;
        Ok(())
    }

    /// Put a file into the Wayland clipboard as `text/uri-list`, which file
    /// managers paste as the file itself. Served from a thread like the
    /// text. X11 sessions get an error; there is no Wayland to talk to.
    pub fn copy_file(path: &std::path::Path) -> Result<()> {
        use wl_clipboard_rs::copy::{MimeType, Options, Source};
        let uri = format!("{}\r\n", file_uri(path));
        let mut options = Options::new();
        options.foreground(true);
        let prepared = options
            .prepare_copy(
                Source::Bytes(uri.into_bytes().into_boxed_slice()),
                MimeType::Specific("text/uri-list".into()),
            )
            .context("cannot offer the file to the Wayland clipboard")?;
        std::thread::Builder::new()
            .name("clipboard-file".into())
            .spawn(move || {
                if let Err(e) = prepared.serve() {
                    tracing::debug!("clipboard (file): {e}");
                }
            })
            .context("cannot start the clipboard thread")?;
        Ok(())
    }

    /// `file://` URI of an absolute path, percent-encoded the way file
    /// managers expect it.
    pub fn file_uri(path: &std::path::Path) -> String {
        use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
        const KEEP: &AsciiSet = &NON_ALPHANUMERIC
            .remove(b'/')
            .remove(b'-')
            .remove(b'.')
            .remove(b'_')
            .remove(b'~');
        let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        format!(
            "file://{}",
            utf8_percent_encode(&absolute.to_string_lossy(), KEEP)
        )
    }

    /// `org.freedesktop.FileManager1.ShowItems` on the session bus.
    pub fn show_in_file_manager(path: &std::path::Path) -> Result<()> {
        let uri = file_uri(path);
        off_runtime(move || {
            let conn = zbus::blocking::Connection::session().context("session bus")?;
            let proxy = zbus::blocking::Proxy::new(
                &conn,
                "org.freedesktop.FileManager1",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1",
            )?;
            proxy.call_method("ShowItems", &(vec![uri.as_str()], ""))?;
            Ok(())
        })
    }

    /// A synthetic key press needs a way into the compositor that Wayland
    /// does not offer to ordinary programs; obs-websocket is the way.
    pub fn press_f9() -> Result<()> {
        anyhow::bail!(
            "sending a key press is not possible on Wayland - connect OBS through obs-websocket (Settings › OBS) so replaycut can save the replay directly"
        )
    }
}

#[cfg(target_os = "linux")]
pub use linux::{
    attach_parent_console, claim_single_instance, copy_file, instance_running, press_f9,
    signal_stop,
};

// ------------------------------------------------------------ other platforms

/// What every non-Windows platform shares, plus the stubs for platforms
/// without an implementation of their own.
#[cfg(not(windows))]
mod other {
    use anyhow::Result;

    #[cfg(not(target_os = "linux"))]
    pub fn press_f9() -> Result<()> {
        anyhow::bail!("sending the replay hotkey is only supported on Windows")
    }

    #[cfg(not(target_os = "linux"))]
    pub fn copy_file(_path: &std::path::Path) -> Result<()> {
        anyhow::bail!("copying a file object is only supported on Windows and Linux")
    }

    pub fn set_app_id() {}

    pub fn open_url(url: &str) -> Result<()> {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
        Ok(())
    }

    pub fn fatal_dialog(text: &str) {
        eprintln!("{text}");
    }

    #[cfg(not(target_os = "linux"))]
    pub fn attach_parent_console() -> bool {
        true
    }

    #[cfg(not(target_os = "linux"))]
    pub struct SingleInstance;

    #[cfg(not(target_os = "linux"))]
    pub fn claim_single_instance(_port: u16) -> Result<Option<SingleInstance>> {
        Ok(Some(SingleInstance))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn instance_running(_port: u16) -> bool {
        false
    }

    #[cfg(not(target_os = "linux"))]
    pub fn signal_stop(_port: u16) -> Result<bool> {
        anyhow::bail!("`replaycut stop` is only supported on Windows and Linux")
    }
}

#[cfg(not(windows))]
pub use other::{fatal_dialog, open_url, set_app_id};

#[cfg(not(any(windows, target_os = "linux")))]
pub use other::{
    attach_parent_console, claim_single_instance, copy_file, instance_running, press_f9,
    signal_stop,
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

    #[cfg(target_os = "linux")]
    #[test]
    fn file_uri_keeps_slashes_and_encodes_the_rest() {
        let uri = super::linux::file_uri(std::path::Path::new("/home/you/Videos/Clip 1 & 2.mp4"));
        assert_eq!(uri, "file:///home/you/Videos/Clip%201%20%26%202.mp4");
    }
}
