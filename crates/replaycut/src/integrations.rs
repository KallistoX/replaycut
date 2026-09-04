//! Optional integrations of the share pipeline. A **storage** turns the
//! encoded file into a link, a **notify** target posts that link. Each is
//! independently enabled in the settings; `--dry-run` replaces both with
//! simulations that never touch the network.

use std::hash::{BuildHasher, Hasher};
use std::path::Path;

use anyhow::Result;

use crate::settings::Settings;

#[derive(Debug, Clone)]
pub struct Published {
    /// Share page.
    pub page: String,
    /// Direct download URL (`<page>/download`).
    pub direct: String,
    /// Remote path, `/<folder>/<month>/<file>`.
    pub path: String,
}

pub enum Storage {
    DryRun { folder: String },
}

pub enum Notify {
    DryRun,
}

pub struct Integrations {
    pub storage: Option<Storage>,
    pub notify: Option<Notify>,
}

impl Integrations {
    pub fn from_settings(settings: &Settings, dry_run: bool) -> Self {
        if dry_run {
            return Self {
                storage: Some(Storage::DryRun {
                    folder: settings.integrations.nextcloud.folder.clone(),
                }),
                notify: Some(Notify::DryRun),
            };
        }
        if settings.integrations.nextcloud.enabled {
            tracing::warn!("the Nextcloud integration is enabled but not implemented yet - uploads are skipped");
        }
        if settings.integrations.discord.enabled {
            tracing::warn!(
                "the Discord integration is enabled but not implemented yet - posts are skipped"
            );
        }
        Self {
            storage: None,
            notify: None,
        }
    }

    pub fn describe(&self) -> String {
        let s = match &self.storage {
            Some(Storage::DryRun { .. }) => "storage: dry run",
            None => "storage: none",
        };
        let n = match &self.notify {
            Some(Notify::DryRun) => "notify: dry run",
            None => "notify: none",
        };
        format!("{s}, {n}")
    }
}

impl Storage {
    pub fn remote_path(&self, month: &str, file_name: &str) -> String {
        match self {
            Storage::DryRun { folder } => format!("/{folder}/{month}/{file_name}"),
        }
    }

    pub async fn publish(&self, file: &Path, month: &str) -> Result<Published> {
        let name = file
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let path = self.remote_path(month, &name);
        match self {
            Storage::DryRun { .. } => {
                tracing::info!("dry run: upload to {path} skipped");
                let page = format!("https://dry-run.invalid/s/{}", random_token(16));
                Ok(Published {
                    direct: format!("{page}/download"),
                    page,
                    path,
                })
            }
        }
    }

    /// Delete remote files; missing ones do not count as errors. Returns how many were deleted.
    pub async fn delete(&self, paths: &[String]) -> Result<usize> {
        match self {
            Storage::DryRun { .. } => {
                tracing::info!("dry run: remote delete of {} path(s) skipped", paths.len());
                Ok(paths.len())
            }
        }
    }
}

impl Notify {
    /// Post a message; returns a human-readable status for the job's `discord` field.
    pub async fn post(&self, text: &str) -> Result<String> {
        match self {
            Notify::DryRun => {
                tracing::info!("dry run: post skipped: {text}");
                Ok("dry run: not posted".to_string())
            }
        }
    }
}

/// Lowercase hex from the standard library's randomly seeded hasher; good
/// enough for ids and fake links, no crate needed.
pub fn random_token(len: usize) -> String {
    let mut out = String::new();
    while out.len() < len {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(out.len() as u64);
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(len);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_hex_of_requested_length() {
        let t = random_token(8);
        assert_eq!(t.len(), 8);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_token(16), random_token(16));
    }
}
