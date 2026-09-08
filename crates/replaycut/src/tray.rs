//! The tray icon: Open, Copy address, Show QR code, Pause scanning, Check for
//! updates, Open log folder, Quit; tooltip with the clip count, the share
//! progress, "paused" or "update available"; icon state normal / job running
//! / last job failed.
//! Windows wants the icon's message loop on the thread that created it, so
//! `win::run` owns the main thread and the tokio runtime lives on another
//! one. On Linux the icon is a StatusNotifierItem on the session bus (the
//! protocol KDE, Waybar and GNOME's AppIndicator extension show), served by a
//! task on the runtime. Either way the service pokes the tray through
//! `TrayHandle::refresh` whenever the state changed; the tray then re-reads
//! the state and updates what differs.

use crate::state::AppState;

/// What the tray shows, derived from the state. Platform-neutral.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayInfo {
    pub clips: usize,
    /// Percent of the running share, if one runs.
    pub sharing: Option<u8>,
    pub last_failed: bool,
    /// Shares waiting behind the running one.
    pub queued: usize,
    /// "Pause scanning" is ticked.
    pub paused: bool,
    /// A newer release is known.
    pub update: Option<String>,
    /// Devices waiting for a sign-in (since 2.8).
    pub pending: usize,
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
impl TrayInfo {
    pub fn of(state: &AppState) -> Self {
        let inner = state.inner.lock();
        let sharing = inner
            .current_job
            .as_ref()
            .and_then(|id| inner.jobs.get(id))
            .map(|j| j.percent);
        Self {
            clips: inner.clips.len(),
            sharing,
            last_failed: inner
                .last
                .as_ref()
                .is_some_and(|j| j.ok == Some(false) && !j.cancelled),
            queued: inner.queue.len(),
            paused: state.scanning_paused(),
            update: state
                .update
                .lock()
                .latest
                .as_ref()
                .map(|l| l.version.clone()),
            pending: state.pairing.pending_count(),
        }
    }

    /// The menu entry that opens the approve page (since 2.8).
    pub fn sign_in_label(&self) -> String {
        match self.pending {
            0 => "No sign-in requests".to_string(),
            1 => "1 sign-in request".to_string(),
            n => format!("{n} sign-in requests"),
        }
    }

    pub fn tooltip(&self) -> String {
        // a request runs out in two minutes: it comes first
        if self.pending > 0 {
            return format!("replaycut - {}", self.sign_in_label().to_lowercase());
        }
        if let Some(p) = self.sharing {
            return if self.queued > 0 {
                format!("replaycut - sharing ... {p} % (+{} queued)", self.queued)
            } else {
                format!("replaycut - sharing ... {p} %")
            };
        }
        if self.paused {
            return "replaycut - paused".to_string();
        }
        if let Some(v) = &self.update {
            return format!("replaycut - update available ({v})");
        }
        format!(
            "replaycut - {} clip{}",
            self.clips,
            if self.clips == 1 { "" } else { "s" }
        )
    }

    pub fn icon(&self) -> IconState {
        if self.sharing.is_some() {
            IconState::Busy
        } else if self.last_failed {
            IconState::Error
        } else {
            IconState::Normal
        }
    }
}

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Normal,
    Busy,
    Error,
}

#[cfg(windows)]
mod win {
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, GetSystemMetrics, PeekMessageW, PostThreadMessageW,
        TranslateMessage, MSG, PM_NOREMOVE, SM_CXSMICON, SM_CYSMICON, WM_APP, WM_QUIT,
    };

    use super::{IconState, TrayInfo};
    use crate::lifecycle::Shutdown;
    use crate::platform;
    use crate::state::{AppState, VERSION};
    use crate::toast::{self, Toast};
    use crate::update;

    const WM_REFRESH: u32 = WM_APP + 1;

    /// Posts to the tray thread; cheap and safe from any thread.
    #[derive(Debug, Clone, Copy)]
    pub struct TrayHandle {
        thread: u32,
    }

    impl TrayHandle {
        /// Must be called on the thread that will run the message loop. Also
        /// creates that thread's message queue so posts before `run` are kept.
        pub fn for_current_thread() -> Self {
            // SAFETY: PeekMessageW only forces the queue into existence here.
            unsafe {
                let mut msg = MSG::default();
                let _ = PeekMessageW(&mut msg, None, WM_REFRESH, WM_REFRESH, PM_NOREMOVE);
                Self {
                    thread: GetCurrentThreadId(),
                }
            }
        }

        pub fn refresh(&self) {
            self.post(WM_REFRESH);
        }

        /// End the message loop (after the service finished).
        pub fn quit(&self) {
            self.post(WM_QUIT);
        }

        fn post(&self, message: u32) {
            // SAFETY: thread messages without pointers.
            let _ = unsafe { PostThreadMessageW(self.thread, message, WPARAM(0), LPARAM(0)) };
        }
    }

    struct Icons {
        normal: Icon,
        busy: Icon,
        error: Icon,
    }

    impl Icons {
        fn load() -> Result<Self> {
            // SAFETY: GetSystemMetrics has no preconditions.
            let size = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
            let size = Some((size.0.max(16) as u32, size.1.max(16) as u32));
            let load = |id: u16| {
                Icon::from_resource(id, size).with_context(|| format!("icon resource {id}"))
            };
            Ok(Self {
                normal: load(1)?,
                busy: load(2)?,
                error: load(3)?,
            })
        }

        fn get(&self, state: IconState) -> &Icon {
            match state {
                IconState::Normal => &self.normal,
                IconState::Busy => &self.busy,
                IconState::Error => &self.error,
            }
        }
    }

    fn open_ui(state: &AppState) {
        let url = state.ui_url();
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    fn copy_address(state: &AppState) {
        let url = state.lan_url();
        match platform::copy_text(&url) {
            Ok(()) => tracing::info!("address copied: {url}"),
            Err(e) => tracing::warn!("clipboard: {e}"),
        }
    }

    /// "Show QR code": the settings page opens its address dialog for `#qr`.
    fn show_qr(state: &AppState) {
        let url = format!("{}settings#qr", state.ui_url());
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    /// "N sign-in requests": the approve page, over loopback (since 2.8).
    fn show_requests(state: &AppState) {
        let url = format!("{}approve", state.ui_url());
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    fn open_log_folder(state: &AppState) {
        let dir = state.data_dir.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = platform::open_folder(&dir) {
            tracing::warn!("cannot open {}: {e:#}", dir.display());
        }
    }

    /// "Check for updates": ask now, then say what came of it in a toast.
    fn check_updates(state: Arc<AppState>, handle: &tokio::runtime::Handle) {
        handle.spawn(async move {
            let t = match update::check(&state).await {
                Ok(Some(info)) => Toast::update_available(&info.version, &state.ui_url()),
                Ok(None) => Toast::up_to_date(VERSION),
                Err(e) => Toast::update_check_failed(&format!("{e:#}")),
            };
            toast::show(&state, t);
        });
    }

    /// Create the tray icon and run the message loop until `TrayHandle::quit`.
    /// `handle` runs the update check on the service runtime.
    pub fn run(
        state: Arc<AppState>,
        shutdown: Shutdown,
        handle: tokio::runtime::Handle,
    ) -> Result<()> {
        let menu = Menu::new();
        let open = MenuItem::with_id("open", "Open", true, None);
        let copy = MenuItem::with_id("copy", "Copy address", true, None);
        let qr = MenuItem::with_id("qr", "Show QR code", true, None);
        let mut info = TrayInfo::of(&state);
        let sign_in = MenuItem::with_id("signin", info.sign_in_label(), info.pending > 0, None);
        let pause = CheckMenuItem::with_id("pause", "Pause scanning", true, false, None);
        let check = MenuItem::with_id("check", "Check for updates", true, None);
        let logs = MenuItem::with_id("logs", "Open log folder", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        menu.append_items(&[
            &open,
            &copy,
            &qr,
            &sign_in,
            &PredefinedMenuItem::separator(),
            &pause,
            &check,
            &logs,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .context("tray menu")?;

        // Handlers run inside DispatchMessageW, on this thread.
        // The handler must be Send, so it cannot hold the CheckMenuItem: the
        // click toggles the state, the refresh below keeps the tick in step.
        let st = state.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| match e.id.0.as_str() {
            "open" => open_ui(&st),
            "copy" => copy_address(&st),
            "qr" => show_qr(&st),
            "signin" => show_requests(&st),
            "pause" => st.set_scanning_paused(!st.scanning_paused()),
            "check" => check_updates(st.clone(), &handle),
            "logs" => open_log_folder(&st),
            "quit" => shutdown.request("Quit in the tray menu"),
            _ => {}
        }));
        let st = state.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = e
            {
                open_ui(&st);
            }
        }));

        let icons = Icons::load()?;
        let tray = TrayIconBuilder::new()
            .with_tooltip(info.tooltip())
            .with_icon(icons.get(info.icon()).clone())
            .with_menu(Box::new(menu))
            .build()
            .context("tray icon")?;
        tracing::info!("tray icon ready");

        // SAFETY: the classic Win32 message loop; MSG is a plain struct.
        unsafe {
            let mut msg = MSG::default();
            loop {
                let r = GetMessageW(&mut msg, None, 0, 0);
                if r.0 == 0 {
                    break; // WM_QUIT
                }
                if r.0 == -1 {
                    anyhow::bail!("GetMessageW failed");
                }
                if msg.hwnd.is_invalid() && msg.message == WM_REFRESH {
                    let now = TrayInfo::of(&state);
                    if now != info {
                        if now.tooltip() != info.tooltip() {
                            let _ = tray.set_tooltip(Some(now.tooltip()));
                        }
                        if now.icon() != info.icon() {
                            let _ = tray.set_icon(Some(icons.get(now.icon()).clone()));
                        }
                        // paused through the API: keep the tick in step
                        if pause.is_checked() != now.paused {
                            pause.set_checked(now.paused);
                        }
                        if now.pending != info.pending {
                            sign_in.set_text(now.sign_in_label());
                            sign_in.set_enabled(now.pending > 0);
                        }
                        info = now;
                    }
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        drop(tray);
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{run, TrayHandle};

/// The Linux tray: a StatusNotifierItem served over D-Bus by `ksni`. The
/// icon comes as ARGB pixmaps rendered by `docs/design/icons/mkico` (no icon
/// theme entry is needed, so it also works from a development build), the
/// menu mirrors the Windows one, and a task waits for `TrayHandle::refresh`
/// to push the changed state to the host.
#[cfg(target_os = "linux")]
mod linux {
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
    use ksni::{Icon, ToolTip, TrayMethods};
    use tokio::sync::Notify;

    use super::{IconState, TrayInfo};
    use crate::lifecycle::Shutdown;
    use crate::platform;
    use crate::state::{AppState, VERSION};
    use crate::toast::{self, Toast};
    use crate::update;

    /// Pokes the tray task; cheap and safe from any thread.
    #[derive(Debug, Clone, Default)]
    pub struct TrayHandle {
        changed: Arc<Notify>,
    }

    impl TrayHandle {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn refresh(&self) {
            self.changed.notify_one();
        }
    }

    // 22 px is what panels show, 48 px lets a bigger tray scale down.
    const NORMAL: (&[u8], &[u8]) = (
        include_bytes!("../assets/tray-normal-22.argb"),
        include_bytes!("../assets/tray-normal-48.argb"),
    );
    const BUSY: (&[u8], &[u8]) = (
        include_bytes!("../assets/tray-busy-22.argb"),
        include_bytes!("../assets/tray-busy-48.argb"),
    );
    const ERROR: (&[u8], &[u8]) = (
        include_bytes!("../assets/tray-error-22.argb"),
        include_bytes!("../assets/tray-error-48.argb"),
    );

    fn pixmaps(state: IconState) -> Vec<Icon> {
        let (small, big) = match state {
            IconState::Normal => NORMAL,
            IconState::Busy => BUSY,
            IconState::Error => ERROR,
        };
        vec![
            Icon {
                width: 22,
                height: 22,
                data: small.to_vec(),
            },
            Icon {
                width: 48,
                height: 48,
                data: big.to_vec(),
            },
        ]
    }

    fn open_ui(state: &AppState) {
        let url = state.ui_url();
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    fn copy_address(state: &AppState) {
        let url = state.lan_url();
        match platform::copy_text(&url) {
            Ok(()) => tracing::info!("address copied: {url}"),
            Err(e) => tracing::warn!("clipboard: {e}"),
        }
    }

    /// "Show QR code": the settings page opens its address dialog for `#qr`.
    fn show_qr(state: &AppState) {
        let url = format!("{}settings#qr", state.ui_url());
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    /// "N sign-in requests": the approve page, over loopback (since 2.8).
    fn show_requests(state: &AppState) {
        let url = format!("{}approve", state.ui_url());
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }

    fn open_log_folder(state: &AppState) {
        let dir = state.data_dir.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = platform::open_folder(&dir) {
            tracing::warn!("cannot open {}: {e:#}", dir.display());
        }
    }

    /// "Check for updates": ask now, then say what came of it in a toast.
    /// The menu callback runs on the runtime, so the work is a task.
    fn check_updates(state: Arc<AppState>) {
        tokio::spawn(async move {
            let t = match update::check(&state).await {
                Ok(Some(info)) => Toast::update_available(&info.version, &state.ui_url()),
                Ok(None) => Toast::up_to_date(VERSION),
                Err(e) => Toast::update_check_failed(&format!("{e:#}")),
            };
            toast::show(&state, t);
        });
    }

    struct Item {
        state: Arc<AppState>,
        shutdown: Shutdown,
        info: TrayInfo,
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "replaycut".into()
        }

        fn title(&self) -> String {
            "replaycut".into()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            pixmaps(self.info.icon())
        }

        fn tool_tip(&self) -> ToolTip {
            ToolTip {
                title: self.info.tooltip(),
                ..Default::default()
            }
        }

        /// A left click opens the page, as on Windows.
        fn activate(&mut self, _x: i32, _y: i32) {
            open_ui(&self.state);
        }

        /// The menu is built from `info`; re-read it right before it shows so
        /// the tick and the request count are current.
        fn menu_about_to_show(&mut self) {
            self.info = TrayInfo::of(&self.state);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Open".into(),
                    activate: Box::new(|t: &mut Self| open_ui(&t.state)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Copy address".into(),
                    activate: Box::new(|t: &mut Self| copy_address(&t.state)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Show QR code".into(),
                    activate: Box::new(|t: &mut Self| show_qr(&t.state)),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: self.info.sign_in_label(),
                    enabled: self.info.pending > 0,
                    activate: Box::new(|t: &mut Self| show_requests(&t.state)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                CheckmarkItem {
                    label: "Pause scanning".into(),
                    checked: self.info.paused,
                    activate: Box::new(|t: &mut Self| {
                        t.state.set_scanning_paused(!t.state.scanning_paused());
                        t.info = TrayInfo::of(&t.state);
                    }),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Check for updates".into(),
                    activate: Box::new(|t: &mut Self| check_updates(t.state.clone())),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Open log folder".into(),
                    activate: Box::new(|t: &mut Self| open_log_folder(&t.state)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|t: &mut Self| t.shutdown.request("Quit in the tray menu")),
                    ..Default::default()
                }
                .into(),
            ]
        }

        /// The host (the panel) went away; keep the item so it is back when
        /// the panel is.
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            tracing::info!("tray host gone ({reason:?}) - the icon is back when it is");
            true
        }

        fn watcher_online(&self) {
            tracing::info!("tray host is back");
        }
    }

    /// Register the item and keep it in step with the state until the
    /// shutdown. An error means no tray host answered (a desktop without a
    /// tray); the service runs without the icon then.
    pub async fn run(state: Arc<AppState>, shutdown: Shutdown, handle: TrayHandle) -> Result<()> {
        let item = Item {
            state: state.clone(),
            shutdown: shutdown.clone(),
            info: TrayInfo::of(&state),
        };
        let sni = item.spawn().await.map_err(|e| anyhow!("{e}"))?;
        tracing::info!("tray icon ready");
        loop {
            tokio::select! {
                _ = handle.changed.notified() => {
                    let now = TrayInfo::of(&state);
                    let alive = sni
                        .update(|t| {
                            if t.info != now {
                                t.info = now.clone();
                            }
                        })
                        .await;
                    if alive.is_none() {
                        break;
                    }
                }
                _ = shutdown.wait() => break,
            }
        }
        sni.shutdown().await;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::{run, TrayHandle};

#[cfg(not(any(windows, target_os = "linux")))]
mod other {
    /// No tray on other platforms; the handle is a no-op.
    #[derive(Debug, Clone, Copy)]
    pub struct TrayHandle;

    #[allow(dead_code)]
    impl TrayHandle {
        pub fn for_current_thread() -> Self {
            Self
        }
        pub fn refresh(&self) {}
        pub fn quit(&self) {}
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
pub use other::TrayHandle;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tooltip_and_icon_follow_the_state() {
        let idle = TrayInfo {
            clips: 1,
            sharing: None,
            last_failed: false,
            queued: 0,
            paused: false,
            update: None,
            pending: 0,
        };
        assert_eq!(idle.tooltip(), "replaycut - 1 clip");
        assert_eq!(idle.icon(), IconState::Normal);
        assert_eq!(idle.sign_in_label(), "No sign-in requests");
        let many = TrayInfo {
            clips: 12,
            ..idle.clone()
        };
        assert_eq!(many.tooltip(), "replaycut - 12 clips");
        let busy = TrayInfo {
            sharing: Some(42),
            last_failed: true,
            ..idle.clone()
        };
        assert_eq!(busy.tooltip(), "replaycut - sharing ... 42 %");
        assert_eq!(busy.icon(), IconState::Busy);
        let failed = TrayInfo {
            last_failed: true,
            ..idle.clone()
        };
        assert_eq!(failed.icon(), IconState::Error);
        let paused = TrayInfo {
            paused: true,
            update: Some("9.9.0".into()),
            ..idle.clone()
        };
        assert_eq!(paused.tooltip(), "replaycut - paused");
        assert_eq!(paused.icon(), IconState::Normal);
        let update = TrayInfo {
            update: Some("9.9.0".into()),
            ..idle
        };
        assert_eq!(update.tooltip(), "replaycut - update available (9.9.0)");
        let sharing_wins = TrayInfo {
            sharing: Some(3),
            ..update.clone()
        };
        assert_eq!(sharing_wins.tooltip(), "replaycut - sharing ... 3 %");
        let with_queue = TrayInfo {
            queued: 2,
            ..sharing_wins
        };
        assert_eq!(
            with_queue.tooltip(),
            "replaycut - sharing ... 3 % (+2 queued)"
        );
        // a device waiting for an answer runs out in two minutes: it wins
        let asking = TrayInfo {
            pending: 1,
            ..with_queue.clone()
        };
        assert_eq!(asking.tooltip(), "replaycut - 1 sign-in request");
        assert_eq!(asking.sign_in_label(), "1 sign-in request");
        let two = TrayInfo {
            pending: 2,
            ..with_queue
        };
        assert_eq!(two.tooltip(), "replaycut - 2 sign-in requests");
    }
}
