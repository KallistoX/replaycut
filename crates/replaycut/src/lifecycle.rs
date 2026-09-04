//! Process lifecycle: one shutdown request shared by every trigger (Ctrl+C,
//! the console closing, the stop event, Quit in the tray menu) and a panic
//! hook that writes to the log before the default hook prints to stderr.

use std::sync::Arc;

use tokio::sync::watch;

/// Cloneable handle for requesting and awaiting the shutdown. The first
/// reason wins and is what the log reports.
#[derive(Clone)]
pub struct Shutdown {
    tx: Arc<watch::Sender<Option<&'static str>>>,
    rx: watch::Receiver<Option<&'static str>>,
}

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(None);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Request the shutdown. Later calls keep the first reason.
    pub fn request(&self, reason: &'static str) {
        let first = self.tx.send_if_modified(|v| {
            if v.is_none() {
                *v = Some(reason);
                true
            } else {
                false
            }
        });
        if first {
            tracing::info!("shutdown requested: {reason}");
        } else {
            tracing::debug!("shutdown already requested ({reason} ignored)");
        }
    }

    /// Resolve with the reason once a shutdown was requested.
    pub async fn wait(&self) -> &'static str {
        let mut rx = self.rx.clone();
        loop {
            if let Some(reason) = *rx.borrow_and_update() {
                return reason;
            }
            if rx.changed().await.is_err() {
                return "shutdown channel closed";
            }
        }
    }
}

/// Log panics with location and backtrace, then let the default hook run.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic payload".to_string());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(
            "panic in thread '{}' at {location}: {message}\n{backtrace}",
            thread.name().unwrap_or("?")
        );
        default(info);
    }));
}

/// Console signals: Ctrl+C, Ctrl+Break and, on Windows, the console window
/// closing, sign-out and shutdown. Each one requests the shutdown with its name.
pub async fn console_signals(shutdown: Shutdown) {
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        let s = shutdown.clone();
        let ctrl_c = async move {
            if let Ok(mut sig) = windows::ctrl_c() {
                if sig.recv().await.is_some() {
                    s.request("Ctrl+C");
                }
            }
        };
        let s = shutdown.clone();
        let ctrl_break = async move {
            if let Ok(mut sig) = windows::ctrl_break() {
                if sig.recv().await.is_some() {
                    s.request("Ctrl+Break");
                }
            }
        };
        let s = shutdown.clone();
        let ctrl_close = async move {
            if let Ok(mut sig) = windows::ctrl_close() {
                if sig.recv().await.is_some() {
                    s.request("console closed");
                }
            }
        };
        let s = shutdown.clone();
        let ctrl_logoff = async move {
            if let Ok(mut sig) = windows::ctrl_logoff() {
                if sig.recv().await.is_some() {
                    s.request("sign-out");
                }
            }
        };
        let s = shutdown;
        let ctrl_shutdown = async move {
            if let Ok(mut sig) = windows::ctrl_shutdown() {
                if sig.recv().await.is_some() {
                    s.request("system shutdown");
                }
            }
        };
        tokio::join!(ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown);
    }
    #[cfg(not(windows))]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let s = shutdown.clone();
        let term = async move {
            if let Ok(mut sig) = signal(SignalKind::terminate()) {
                if sig.recv().await.is_some() {
                    s.request("SIGTERM");
                }
            }
        };
        let int = async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown.request("Ctrl+C");
            }
        };
        tokio::join!(term, int);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn first_reason_wins() {
        let s = Shutdown::new();
        s.request("first");
        s.request("second");
        assert_eq!(s.wait().await, "first");
    }

    #[tokio::test]
    async fn wait_resolves_when_requested_later() {
        let s = Shutdown::new();
        let waiter = {
            let s = s.clone();
            tokio::spawn(async move { s.wait().await })
        };
        tokio::task::yield_now().await;
        s.request("later");
        assert_eq!(waiter.await.unwrap(), "later");
    }
}
