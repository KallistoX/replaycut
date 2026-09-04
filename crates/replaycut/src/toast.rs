//! Desktop notifications ("toasts") through WinRT. Windows only shows them
//! for a registered application id (`replaycut install` writes that
//! registration); without it the first failure is logged once and the
//! service carries on. In dry-run mode toasts are only logged.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::state::{AppState, Clip, Job};

/// What a toast shows: two lines and, on click, a page to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub text: String,
    pub url: Option<String>,
}

impl Toast {
    /// A new replay was written by OBS.
    pub fn clip_saved(clip: &Clip, ui_url: &str) -> Self {
        Self {
            title: "Clip saved".into(),
            text: format!(
                "{} ({} s). Trim it at {ui_url}",
                clip.name,
                clip.duration.round() as i64
            ),
            url: Some(ui_url.to_string()),
        }
    }

    /// A share finished, successfully or not. `uploaded` says whether a
    /// storage integration ran (then the link is in the clipboard).
    pub fn share_result(job: &Job, uploaded: bool, ui_url: &str) -> Self {
        let seconds = job.seconds.round() as i64;
        if job.ok != Some(true) {
            return Self {
                title: "Share failed".into(),
                text: job.error.clone().unwrap_or_else(|| "unknown error".into()),
                url: Some(ui_url.to_string()),
            };
        }
        let size = job
            .size_mb
            .map(|m| format!("{m} MB"))
            .unwrap_or_else(|| "? MB".into());
        if uploaded {
            let discord = job
                .discord
                .as_deref()
                .map(|d| format!(". Discord: {d}"))
                .unwrap_or_default();
            Self {
                title: "Clip shared, link copied".into(),
                text: format!("{size}, {seconds} s{discord}"),
                url: job.link.clone().or_else(|| Some(ui_url.to_string())),
            }
        } else {
            Self {
                title: "Clip ready".into(),
                text: format!(
                    "{} ({size}, {seconds} s)",
                    job.file.as_deref().unwrap_or("shared clip")
                ),
                url: Some(ui_url.to_string()),
            }
        }
    }

    /// The toast XML: generic template, two text lines, a click opens `url`.
    pub fn xml(&self) -> String {
        let launch = match &self.url {
            Some(u) => format!(" activationType=\"protocol\" launch=\"{}\"", escape(u)),
            None => String::new(),
        };
        format!(
            "<toast{launch}><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
            escape(&self.title),
            escape(&self.text)
        )
    }
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

static UNAVAILABLE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Show a toast, or log it in dry-run mode. Never blocks the caller.
pub fn show(state: &AppState, toast: Toast) {
    if state.dry_run {
        tracing::info!("dry run: toast '{}' - {}", toast.title, toast.text);
        return;
    }
    tracing::info!("toast '{}' - {}", toast.title, toast.text);
    tokio::task::spawn_blocking(move || {
        if let Err(e) = show_now(&toast) {
            if UNAVAILABLE_LOGGED.swap(true, Ordering::Relaxed) {
                tracing::debug!("toast failed: {e}");
            } else {
                tracing::warn!(
                    "toast failed: {e} - notifications need the app registration written by `replaycut install`; further failures are logged at debug level"
                );
            }
        }
    });
}

#[cfg(windows)]
fn show_now(toast: &Toast) -> anyhow::Result<()> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(toast.xml()))?;
    let notification = ToastNotification::CreateToastNotification(&doc)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(
        crate::platform::APP_ID,
    ))?;
    notifier.Show(&notification)?;
    Ok(())
}

#[cfg(not(windows))]
fn show_now(_toast: &Toast) -> anyhow::Result<()> {
    anyhow::bail!("toasts are only supported on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            ok: Some(true),
            seconds: 7.4,
            size_mb: Some(1.25),
            file: Some("Replay_2026-09-04_1-8.mp4".into()),
            link: Some("https://cloud.example.com/s/abc".into()),
            discord: Some("posted".into()),
            ..Job::default()
        }
    }

    #[test]
    fn xml_escapes_and_links() {
        let t = Toast {
            title: "A & B".into(),
            text: "<x>".into(),
            url: Some("http://localhost:8420/?a=1&b=2".into()),
        };
        let xml = t.xml();
        assert!(xml.starts_with(
            "<toast activationType=\"protocol\" launch=\"http://localhost:8420/?a=1&amp;b=2\">"
        ));
        assert!(xml.contains("<text>A &amp; B</text><text>&lt;x&gt;</text>"));
    }

    #[test]
    fn shared_toast_matches_the_1_4_wording() {
        let t = Toast::share_result(&job(), true, "http://localhost:8420/");
        assert_eq!(t.title, "Clip shared, link copied");
        assert_eq!(t.text, "1.25 MB, 7 s. Discord: posted");
        assert_eq!(t.url.as_deref(), Some("https://cloud.example.com/s/abc"));
    }

    #[test]
    fn local_only_share_and_failure() {
        let t = Toast::share_result(&job(), false, "http://localhost:8420/");
        assert_eq!(t.title, "Clip ready");
        assert_eq!(t.text, "Replay_2026-09-04_1-8.mp4 (1.25 MB, 7 s)");
        let failed = Job {
            ok: Some(false),
            error: Some("ffmpeg: boom".into()),
            ..Job::default()
        };
        let t = Toast::share_result(&failed, true, "http://localhost:8420/");
        assert_eq!(t.title, "Share failed");
        assert_eq!(t.text, "ffmpeg: boom");
    }

    #[test]
    fn clip_saved_toast() {
        let clip = Clip {
            name: "Replay 2026-09-04 11-40-00.mkv".into(),
            base: "Replay 2026-09-04 11-40-00".into(),
            path: String::new(),
            size: 0,
            duration: 59.6,
            tracks: 4,
            created: String::new(),
            preview: String::new(),
            status: "ready",
            codec: "hevc".into(),
            width: 1920,
            height: 1080,
            fps: 60.0,
        };
        let t = Toast::clip_saved(&clip, "http://localhost:8420/");
        assert_eq!(t.title, "Clip saved");
        assert_eq!(
            t.text,
            "Replay 2026-09-04 11-40-00.mkv (60 s). Trim it at http://localhost:8420/"
        );
    }
}
