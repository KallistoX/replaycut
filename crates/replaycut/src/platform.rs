//! Small Windows integrations that need no installer: recycle bin, the
//! replay hotkey, the clipboard. Other platforms get honest errors or no-ops
//! so the service still builds and runs there for development.

use std::path::Path;

use anyhow::Result;

/// Move a file to the recycle bin (never a permanent delete).
pub fn recycle(path: &Path) -> Result<()> {
    trash::delete(path).map_err(|e| anyhow::anyhow!("recycle {}: {e}", path.display()))
}

/// Press F9 with a 250 ms hold, which OBS registers as its global hotkey.
#[cfg(windows)]
pub fn press_f9() -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };
    const VK_F9: u8 = 0x78;
    // SAFETY: plain Win32 calls with constant arguments; no pointers involved.
    unsafe {
        keybd_event(VK_F9, 0, KEYBD_EVENT_FLAGS(0), 0);
        std::thread::sleep(std::time::Duration::from_millis(250));
        keybd_event(VK_F9, 0, KEYEVENTF_KEYUP, 0);
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn press_f9() -> Result<()> {
    anyhow::bail!("sending the replay hotkey is only supported on Windows")
}
