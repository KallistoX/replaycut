//! replaycut - clip manager for the OBS replay buffer.
//!
//! The executable has no console window of its own: started by double-click
//! or at sign-in it runs silently with a tray icon; started from a terminal
//! it attaches to that terminal for `--help`, `setup`, `test`, `stop` and
//! the log lines.

#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod credentials;
mod http;
mod integrations;
mod lifecycle;
mod media;
mod platform;
mod scanner;
mod settings;
mod setup;
mod share;
mod state;
mod toast;
mod tray;
mod util;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::integrations::Integrations;
use crate::lifecycle::Shutdown;
use crate::media::Media;
use crate::settings::Settings;
use crate::state::{AppState, Paths, VERSION};

#[derive(Parser, Debug)]
#[command(version, about = "Clip manager for the OBS replay buffer")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Directory for settings.json, state files and logs.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Settings file (default: <data-dir>/settings.json).
    #[arg(long, global = true)]
    settings: Option<PathBuf>,
    /// Override clipDir from the settings.
    #[arg(long, global = true)]
    clip_dir: Option<PathBuf>,
    /// Override port from the settings.
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Override the bind address (e.g. 127.0.0.1 for local testing).
    #[arg(long, global = true)]
    bind: Option<String>,
    /// Override the UI file.
    #[arg(long, global = true)]
    ui: Option<PathBuf>,
    /// Override the log level (error, warn, info, debug, trace).
    #[arg(long, global = true)]
    log_level: Option<String>,
    /// Encode for real but simulate uploads, posts, the hotkey and the clipboard.
    #[arg(long, global = true)]
    dry_run: bool,
    /// Do not open the browser when the service starts (used at sign-in).
    #[arg(long, global = true)]
    no_browser: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Run the service (default).
    Run,
    /// Configure the integrations interactively (secrets go to the Credential Manager).
    Setup,
    /// Check the enabled integrations and their credentials.
    Test,
    /// Stop the running service.
    Stop,
}

fn main() -> ExitCode {
    // Before clap prints anything: reach the terminal we were started from.
    let console = platform::attach_parent_console();
    let cli = Cli::parse();
    match real_main(cli, console) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let text = format!("{e:#}");
            eprintln!("error: {text}");
            tracing::error!("{text}");
            if !console {
                platform::fatal_dialog(&format!("replaycut cannot start:\n\n{text}"));
            }
            ExitCode::FAILURE
        }
    }
}

fn real_main(cli: Cli, console: bool) -> Result<()> {
    let data_dir = cli
        .data_dir
        .clone()
        .unwrap_or_else(settings::default_data_dir);
    let settings_path = cli
        .settings
        .clone()
        .unwrap_or_else(|| data_dir.join("settings.json"));
    let mut settings = Settings::load_or_create(&settings_path)?;
    if let Some(d) = cli.clip_dir.clone() {
        settings.clip_dir = d;
    }
    if let Some(p) = cli.port {
        settings.port = p;
    }
    if let Some(b) = cli.bind.clone() {
        settings.bind = b;
    }
    if let Some(u) = cli.ui.clone() {
        settings.ui_file = u;
    }
    if let Some(l) = cli.log_level.clone() {
        settings.log_level = l;
    }
    settings
        .validate()
        .with_context(|| format!("invalid settings in {}", settings_path.display()))?;

    match cli.command {
        Some(Command::Setup) => runtime()?.block_on(setup::run(&settings_path, &mut settings)),
        Some(Command::Test) => runtime()?.block_on(setup::test(&settings)),
        Some(Command::Stop) => stop(),
        Some(Command::Run) | None => run_service(&cli, settings, &data_dir, console),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}

/// `replaycut stop`: signal the stop event and wait for the instance to go.
fn stop() -> Result<()> {
    if !platform::signal_stop()? {
        println!("replaycut is not running");
        return Ok(());
    }
    let started = Instant::now();
    while platform::instance_running() {
        if started.elapsed() > Duration::from_secs(15) {
            anyhow::bail!("replaycut is still running 15 s after the stop request");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!("replaycut stopped");
    Ok(())
}

fn run_service(cli: &Cli, settings: Settings, data_dir: &Path, console: bool) -> Result<()> {
    let _log_guard = init_logging(&data_dir.join("logs"), &settings.log_level, console)?;
    lifecycle::install_panic_hook();
    platform::set_app_id();
    tracing::info!(
        "replaycut {VERSION} starting {}",
        if console {
            "from a console"
        } else {
            "without a console (double-click, shortcut or sign-in)"
        }
    );

    let ui_url = format!("http://localhost:{}/", settings.port);
    let Some(_instance) = platform::claim_single_instance()? else {
        if cli.no_browser {
            tracing::info!("replaycut is already running");
        } else {
            tracing::info!("replaycut is already running - opening {ui_url}");
            if let Err(e) = platform::open_url(&ui_url) {
                tracing::warn!("cannot open {ui_url}: {e}");
            }
        }
        return Ok(());
    };

    let runtime = runtime()?;
    let shutdown = Shutdown::new();
    let (state, listener) = runtime.block_on(startup(settings, data_dir, cli.dry_run))?;

    // `replaycut stop` sets this event; a plain thread waits on it.
    let stop_event = platform::StopEvent::create()?;
    {
        let shutdown = shutdown.clone();
        std::thread::Builder::new()
            .name("stop-event".into())
            .spawn(move || {
                stop_event.wait();
                shutdown.request("stop event");
            })
            .context("cannot start the stop-event thread")?;
    }
    runtime.spawn(lifecycle::console_signals(shutdown.clone()));

    // Double-click and shortcuts open the browser; a terminal user and the
    // sign-in entry (--no-browser) do not want that.
    let open_browser = !console && !cli.no_browser;

    #[cfg(windows)]
    {
        let tray = tray::TrayHandle::for_current_thread();
        let _ = state.tray.set(tray);
        let service = {
            let state = state.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("service".into())
                .spawn(move || {
                    let result = runtime.block_on(serve(state, listener, shutdown, open_browser));
                    tray.quit();
                    result
                })
                .context("cannot start the service thread")?
        };
        if let Err(e) = tray::run(state.clone(), shutdown.clone()) {
            tracing::warn!("tray icon unavailable: {e:#} - running without it");
        }
        service
            .join()
            .map_err(|_| anyhow::anyhow!("service thread panicked"))??;
    }
    #[cfg(not(windows))]
    {
        runtime.block_on(serve(state.clone(), listener, shutdown, open_browser))?;
    }
    tracing::info!("replaycut stopped");
    Ok(())
}

/// Everything that can fail before the service is up: tools, encoder,
/// state files, the listening socket.
async fn startup(
    settings: Settings,
    data_dir: &Path,
    dry_run: bool,
) -> Result<(Arc<AppState>, tokio::net::TcpListener)> {
    let media =
        Media::locate()?.with_resource_limits(settings.ffmpeg_priority, settings.ffmpeg_threads());
    let encoder = media.detect_encoder(&settings.encoder).await?;
    let ui_file = resolve_ui_file(&settings.ui_file);
    if !ui_file.is_file() {
        tracing::warn!(
            "UI file {} not found - GET / will fail until it exists",
            ui_file.display()
        );
    }
    let paths = Paths::new(&settings.clip_dir, data_dir, ui_file);
    let bind = format!("{}:{}", settings.bind, settings.port);
    let integrations = Integrations::build(&settings, dry_run)?;
    let state = Arc::new(AppState::load(
        settings,
        paths,
        media,
        encoder,
        integrations,
        dry_run,
    )?);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    tracing::info!(
        "replaycut {VERSION} started: clips {}, http://{bind}/, encoder {}, ffmpeg {}, ffmpeg threads {}, priority {:?}, {}{}",
        state.paths.clip_dir.display(),
        state.encoder.name,
        state.media.ffmpeg.display(),
        state.settings.ffmpeg_threads(),
        state.settings.ffmpeg_priority,
        state.integrations.describe(),
        if state.dry_run { " [DRY RUN: uploads, posts, hotkey, clipboard and toasts are simulated]" } else { "" }
    );
    Ok((state, listener))
}

/// Serve until a shutdown is requested, then give open connections a moment.
async fn serve(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    shutdown: Shutdown,
    open_browser: bool,
) -> Result<()> {
    tokio::spawn(scanner::run(state.clone()));
    if open_browser {
        let url = state.ui_url();
        if let Err(e) = platform::open_url(&url) {
            tracing::warn!("cannot open {url}: {e}");
        }
    }
    let graceful = {
        let shutdown = shutdown.clone();
        async move {
            shutdown.wait().await;
        }
    };
    let server = axum::serve(listener, http::router(state)).with_graceful_shutdown(graceful);
    let deadline = async {
        shutdown.wait().await;
        tokio::time::sleep(Duration::from_secs(5)).await;
    };
    tokio::select! {
        r = server => r.context("http server")?,
        _ = deadline => tracing::warn!("connections still open 5 s after the shutdown request - stopping anyway"),
    }
    Ok(())
}

fn resolve_ui_file(configured: &Path) -> PathBuf {
    if configured.is_absolute() {
        return configured.to_path_buf();
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = &exe_dir {
        let candidate = dir.join(configured);
        if candidate.is_file() {
            return candidate;
        }
    }
    let cwd = std::env::current_dir()
        .map(|d| d.join(configured))
        .unwrap_or_else(|_| configured.to_path_buf());
    if cwd.is_file() {
        return cwd;
    }
    exe_dir.map(|d| d.join(configured)).unwrap_or(cwd)
}

/// Rolling daily log file; the console copy only when there is a console.
fn init_logging(
    logs_dir: &Path,
    level: &str,
    console: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    std::fs::create_dir_all(logs_dir)?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("replaycut")
        .filename_suffix("log")
        .max_log_files(7)
        .build(logs_dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level));
    let stdout_layer = console.then(|| {
        tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_timer(local_time())
            .with_writer(std::io::stdout)
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(stdout_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_timer(local_time())
                .with_writer(file_writer),
        )
        .init();
    Ok(guard)
}

fn local_time() -> tracing_subscriber::fmt::time::ChronoLocal {
    tracing_subscriber::fmt::time::ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".into())
}
