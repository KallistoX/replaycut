//! Secrets live in the platform's credential store: on Windows the
//! Credential Manager as generic credentials (the secret blob as UTF-16, the
//! way the 1.x service did it, so entries can be copied between the two); on
//! Linux the freedesktop Secret Service (gnome-keyring, KWallet, KeePassXC)
//! in the default collection. Never in a file.

#[cfg(not(target_os = "linux"))]
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
/// YouTube: user = channel title, secret = the OAuth refresh token (since 2.6).
pub const YOUTUBE: &str = "replaycut/youtube";
/// The user's own Google OAuth client for YouTube: user = client id,
/// secret = client secret (since 2.6).
pub const YOUTUBE_CLIENT: &str = "replaycut/youtube-client";
/// X: user = @username, secret = the OAuth refresh token (since 2.6).
pub const X: &str = "replaycut/x";
/// Telegram: user = `bot`, secret = the bot token (since 2.6).
pub const TELEGRAM: &str = "replaycut/telegram";
/// Generic webhook: user = `secret`, secret = the HMAC secret (since 2.6).
pub const WEBHOOK_SECRET: &str = "replaycut/webhook-secret";

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

/// An item in the default collection carries `application=replaycut`,
/// `target=<name>` and `user=<user>` as attributes; the secret is the
/// password or token as UTF-8. The two lookup attributes are what
/// `secret-tool lookup application replaycut target <name>` needs.
#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashMap;

    use anyhow::{Context, Result};
    use secret_service::blocking::{Collection, SecretService};
    use secret_service::EncryptionType;

    use super::Credential;
    use crate::platform::linux::off_runtime;

    const APPLICATION: &str = "replaycut";

    fn with_collection<T: Send>(f: impl FnOnce(&Collection<'_>) -> Result<T> + Send) -> Result<T> {
        off_runtime(move || {
            let service = SecretService::connect(EncryptionType::Dh).context(
                "cannot reach the Secret Service (org.freedesktop.secrets) - a keyring such as gnome-keyring, KWallet or KeePassXC has to run in this session",
            )?;
            let collection = service
                .get_default_collection()
                .context("the Secret Service has no default collection")?;
            collection
                .ensure_unlocked()
                .context("cannot unlock the default keyring")?;
            f(&collection)
        })
    }

    fn lookup(target: &str) -> HashMap<&str, &str> {
        HashMap::from([("application", APPLICATION), ("target", target)])
    }

    pub fn read(target: &str) -> Result<Option<Credential>> {
        with_collection(|collection| {
            let Some(item) = collection.search_items(lookup(target))?.into_iter().next() else {
                return Ok(None);
            };
            item.ensure_unlocked()?;
            let user = item.get_attributes()?.remove("user").unwrap_or_default();
            let secret = String::from_utf8(item.get_secret()?)
                .with_context(|| format!("{target}: the secret is not UTF-8"))?;
            Ok(Some(Credential { user, secret }))
        })
    }

    pub fn write(target: &str, user: &str, secret: &str) -> Result<()> {
        with_collection(|collection| {
            let mut attributes = lookup(target);
            attributes.insert("user", user);
            collection
                .create_item(
                    &format!("replaycut {target}"),
                    attributes,
                    secret.as_bytes(),
                    true,
                    "text/plain",
                )
                .with_context(|| format!("cannot store {target}"))?;
            Ok(())
        })
    }

    pub fn delete(target: &str) -> Result<bool> {
        with_collection(|collection| {
            let items = collection.search_items(lookup(target))?;
            let found = !items.is_empty();
            for item in items {
                item.delete()
                    .with_context(|| format!("cannot delete {target}"))?;
            }
            Ok(found)
        })
    }
}

#[cfg(target_os = "linux")]
pub use linux::{delete, read, write};

#[cfg(not(any(windows, target_os = "linux")))]
pub fn read(_target: &str) -> Result<Option<Credential>> {
    Ok(None)
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn write(_target: &str, _user: &str, _secret: &str) -> Result<()> {
    anyhow::bail!("credential storage is only available on Windows and Linux")
}
#[cfg(not(any(windows, target_os = "linux")))]
pub fn delete(_target: &str) -> Result<bool> {
    Ok(false)
}
