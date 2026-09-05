//! Secrets live in the Windows Credential Manager as generic credentials.
//! The secret blob is stored as UTF-16, the way the 1.x service did it, so
//! entries can be copied between the two.

use anyhow::Result;

pub const NEXTCLOUD: &str = "replaycut/nextcloud";
pub const DISCORD_WEBHOOK: &str = "replaycut/discord-webhook";
pub const OBS_WEBSOCKET: &str = "replaycut/obs-websocket";
/// OneDrive: user = account name, secret = the OAuth refresh token (since 2.5).
pub const ONEDRIVE: &str = "replaycut/onedrive";
/// S3: user = access key id, secret = secret access key (since 2.5).
pub const S3: &str = "replaycut/s3";
/// WebDAV: user and password of the DAV login (since 2.5).
pub const WEBDAV: &str = "replaycut/webdav";

#[derive(Debug, Clone)]
pub struct Credential {
    pub user: String,
    pub secret: String,
}

#[cfg(windows)]
mod win {
    use super::Credential;
    use anyhow::{Context, Result};
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::ERROR_NOT_FOUND;
    use windows::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_FLAGS,
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn read(target: &str) -> Result<Option<Credential>> {
        let target_w = wide(target);
        let mut ptr: *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: valid null-terminated target string; the pointer is freed with CredFree.
        let res =
            unsafe { CredReadW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None, &mut ptr) };
        if let Err(e) = res {
            if e.code() == ERROR_NOT_FOUND.to_hresult() {
                return Ok(None);
            }
            return Err(e).with_context(|| format!("CredReadW {target}"));
        }
        // SAFETY: CredReadW succeeded, so ptr points to a valid CREDENTIALW until CredFree.
        let cred = unsafe {
            let c = &*ptr;
            let user = if c.UserName.is_null() {
                String::new()
            } else {
                c.UserName.to_string().unwrap_or_default()
            };
            let secret = if c.CredentialBlobSize == 0 || c.CredentialBlob.is_null() {
                String::new()
            } else {
                let units = std::slice::from_raw_parts(
                    c.CredentialBlob as *const u16,
                    c.CredentialBlobSize as usize / 2,
                );
                String::from_utf16_lossy(units)
            };
            CredFree(ptr as *const _);
            Credential { user, secret }
        };
        Ok(Some(cred))
    }

    pub fn write(target: &str, user: &str, secret: &str) -> Result<()> {
        let mut target_w = wide(target);
        let mut user_w = wide(user);
        let mut blob: Vec<u16> = secret.encode_utf16().collect();
        let cred = CREDENTIALW {
            Flags: CRED_FLAGS(0),
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target_w.as_mut_ptr()),
            Comment: PWSTR::null(),
            LastWritten: Default::default(),
            CredentialBlobSize: (blob.len() * 2) as u32,
            CredentialBlob: blob.as_mut_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: std::ptr::null_mut(),
            TargetAlias: PWSTR::null(),
            UserName: PWSTR(user_w.as_mut_ptr()),
        };
        // SAFETY: all pointers reference buffers that outlive the call.
        unsafe { CredWriteW(&cred, 0) }.with_context(|| format!("CredWriteW {target}"))
    }

    pub fn delete(target: &str) -> Result<bool> {
        let target_w = wide(target);
        // SAFETY: valid null-terminated target string.
        match unsafe { CredDeleteW(PCWSTR(target_w.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == ERROR_NOT_FOUND.to_hresult() => Ok(false),
            Err(e) => Err(e).with_context(|| format!("CredDeleteW {target}")),
        }
    }
}

#[cfg(windows)]
pub fn read(target: &str) -> Result<Option<Credential>> {
    win::read(target)
}
#[cfg(windows)]
pub fn write(target: &str, user: &str, secret: &str) -> Result<()> {
    win::write(target, user, secret)
}
#[cfg(windows)]
pub fn delete(target: &str) -> Result<bool> {
    win::delete(target)
}

#[cfg(not(windows))]
pub fn read(_target: &str) -> Result<Option<Credential>> {
    Ok(None)
}
#[cfg(not(windows))]
pub fn write(_target: &str, _user: &str, _secret: &str) -> Result<()> {
    anyhow::bail!("credential storage is only available on Windows")
}
#[cfg(not(windows))]
pub fn delete(_target: &str) -> Result<bool> {
    Ok(false)
}
