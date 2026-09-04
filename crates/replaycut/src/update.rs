//! Update hint: once a minute after start and then daily, ask GitHub for
//! the latest release and remember it when it is newer than this build.
//! The result is only shown (`config.update` in `/api/clips`, a banner in
//! the UI); nothing is downloaded. `checkUpdates: false` switches it off.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::state::{AppState, VERSION};

const RELEASES_URL: &str = "https://api.github.com/repos/KallistoX/replaycut/releases/latest";
const FIRST_CHECK: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(10);

/// A release that is newer than the running build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
}

/// `major.minor.patch` plus whether a pre-release suffix is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    pub parts: (u64, u64, u64),
    pub pre_release: bool,
}

/// Parse `2.1.0`, `v2.1.0` or `2.0.0-dev`; anything else is `None`.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match s.split_once('-') {
        Some((c, p)) => (c, !p.is_empty()),
        None => (s, false),
    };
    let core = core.split_once('+').map_or(core, |(c, _)| c);
    let mut it = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = it.next()??;
    let minor = it.next().unwrap_or(Some(0))?;
    let patch = it.next().unwrap_or(Some(0))?;
    if it.next().is_some() {
        return None;
    }
    Some(Version {
        parts: (major, minor, patch),
        pre_release: pre,
    })
}

/// True when `tag` is a release newer than `current`. A pre-release build
/// counts as older than the release with the same number.
pub fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_version(tag), parse_version(current)) {
        (Some(t), Some(c)) => {
            t.parts > c.parts || (t.parts == c.parts && c.pre_release && !t.pre_release)
        }
        _ => false,
    }
}

async fn fetch_latest() -> anyhow::Result<(String, String)> {
    let client = reqwest::Client::builder()
        .user_agent(format!("replaycut/{VERSION}"))
        .timeout(TIMEOUT)
        .build()?;
    let v: serde_json::Value = client
        .get(RELEASES_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let tag = v["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no tag_name in the release document"))?;
    let url = v["html_url"].as_str().unwrap_or_default();
    Ok((tag.to_string(), url.to_string()))
}

/// Background task: first check after a minute, then daily.
pub async fn run(state: Arc<AppState>) {
    tokio::time::sleep(FIRST_CHECK).await;
    loop {
        match fetch_latest().await {
            Ok((tag, url)) => {
                if is_newer(&tag, VERSION) {
                    let info = UpdateInfo {
                        version: tag.trim_start_matches(['v', 'V']).to_string(),
                        url,
                    };
                    let changed = state.update.lock().replace(info.clone()) != Some(info.clone());
                    if changed {
                        tracing::info!(
                            "update available: replaycut {} ({})",
                            info.version,
                            info.url
                        );
                    }
                } else {
                    tracing::debug!("update check: {tag} is not newer than {VERSION}");
                    *state.update.lock() = None;
                }
            }
            Err(e) => tracing::debug!("update check failed: {e:#}"),
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(
            parse_version("v2.1.0"),
            Some(Version {
                parts: (2, 1, 0),
                pre_release: false
            })
        );
        assert_eq!(
            parse_version("2.0.0-dev"),
            Some(Version {
                parts: (2, 0, 0),
                pre_release: true
            })
        );
        assert_eq!(parse_version("2.1"), parse_version("2.1.0"));
        assert!(parse_version("latest").is_none());
        assert!(parse_version("1.2.3.4").is_none());
    }

    #[test]
    fn newer_means_higher_or_the_release_of_a_prerelease() {
        assert!(is_newer("v2.1.0", "2.0.0"));
        assert!(is_newer("v2.0.1", "2.0.0"));
        assert!(is_newer("v2.0.0", "2.0.0-dev"));
        assert!(!is_newer("v2.0.0", "2.0.0"));
        assert!(!is_newer("v1.9.9", "2.0.0"));
        assert!(!is_newer("v2.0.0-rc1", "2.0.0"));
        assert!(!is_newer("nonsense", "2.0.0"));
    }
}
