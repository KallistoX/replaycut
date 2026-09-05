//! replaycut - clip manager for the OBS replay buffer.
//!
//! The executable has no console window of its own: started by double-click
//! or at sign-in it runs silently with a tray icon; started from a terminal
//! it attaches to that terminal for `--help`, `setup`, `test`, `stop` and
//! the log lines.

#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod admin;
mod auth;
mod credentials;
mod diagnostics;
mod http;
#[cfg(windows)]
mod install;
mod integrations;
mod lifecycle;
mod media;
#[cfg(windows)]
mod migrate;
mod obs;
mod obs_link;
mod obs_status;
mod obs_ws;
mod platform;
mod scanner;
mod settings;
mod setup;
mod share;
mod state;
mod toast;
mod tray;
mod update;
mod util;
#[cfg(windows)]
mod winshell;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::lifecycle::Shutdown;
use crate::media::Media;
use crate::settings::Settings;
use crate::state::{AppState, Boot, Overrides, Runtime, VERSION};

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
    /// Wait for a running instance on the same port to exit first (used by the restart).
    #[arg(long, global = true, hide = true)]
    wait_for_exit: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Run the service (default).
    Run,
    /// Configure the integrations interactively (secrets go to the Credential Manager).
    Setup,
    /// Check the enabled integrations and their credentials.
    Test,
    /// Encode part of the newest clip with every encoder profile and print the timings.
    Bench {
        /// Seconds of the clip to encode.
        #[arg(long, default_value_t = 30)]
        seconds: u32,
    },
    /// Stop the running service.
    Stop,
    /// Install or update replaycut for this user (files, shortcuts, optional autostart and firewall rule).
    Install,
    /// Remove the installation; settings and clips stay unless --purge.
    Uninstall {
        /// Also delete settings, state, logs and the stored credentials (asks first).
        #[arg(long)]
        purge: bool,
    },
    /// Start replaycut at sign-in: on, off or status.
    Autostart {
        #[arg(value_enum)]
        mode: AutostartMode,
    },
}

#[cfg(windows)]
use crate::install::AutostartMode;

/// Placeholder so the command line parses on other platforms.
#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum AutostartMode {
    On,
    Off,
    Status,
}

fn main() -> ExitCode {
    // Before clap prints anything: reach the terminal we were started from.
    // The installer starts the service with REPLAYCUT_NO_CONSOLE so that it
    // does not bind to the installer's window.
    let console =
        std::env::var_os("REPLAYCUT_NO_CONSOLE").is_none() && platform::attach_parent_console();
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
    let settings_existed = settings_path.is_file();
    let mut settings = Settings::load_or_create(&settings_path)?;
    let overrides = Overrides {
        clip_dir: cli.clip_dir.clone(),
        port: cli.port,
        bind: cli.bind.clone(),
        ui_file: cli.ui.clone(),
        log_level: cli.log_level.clone(),
    };
    overrides.apply(&mut settings);
    settings
        .validate()
        .with_context(|| format!("invalid settings in {}", settings_path.display()))?;

    match cli.command {
        Some(Command::Setup) => runtime()?.block_on(setup::run(&settings_path, &mut settings)),
        Some(Command::Test) => runtime()?.block_on(setup::test(&settings)),
        Some(Command::Bench { seconds }) => runtime()?.block_on(bench(&settings, seconds)),
        Some(Command::Stop) => stop(settings.port),
        #[cfg(windows)]
        Some(Command::Install) => install::install(
            &runtime()?,
            &mut settings,
            &settings_path,
            &data_dir,
            settings_existed,
        ),
        #[cfg(windows)]
        Some(Command::Uninstall { purge }) => {
            install::uninstall(purge, settings.port, &settings_path, &data_dir)
        }
        #[cfg(windows)]
        Some(Command::Autostart { mode }) => install::autostart(mode),
        #[cfg(not(windows))]
        Some(Command::Install | Command::Uninstall { .. } | Command::Autostart { .. }) => {
            anyhow::bail!("this command is only supported on Windows")
        }
        Some(Command::Run) | None => run_service(
            &cli,
            settings,
            &settings_path,
            overrides,
            &data_dir,
            console,
        ),
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("cannot start the async runtime")
}

/// `replaycut bench`: the encoder profiles head to head on the newest clip.
async fn bench(settings: &Settings, seconds: u32) -> Result<()> {
    let media =
        Media::locate()?.with_resource_limits(settings.ffmpeg_priority, settings.ffmpeg_threads());
    let newest = std::fs::read_dir(&settings.clip_dir)
        .with_context(|| format!("cannot read {}", settings.clip_dir.display()))?
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("mkv"))
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
        .ok_or_else(|| anyhow::anyhow!("no .mkv in {}", settings.clip_dir.display()))?;
    let info = media.video_info(&newest).await;
    println!(
        "replaycut {} bench: {} s of {} ({} {}x{} @ {:.0} fps), ffmpeg {}",
        state::VERSION,
        seconds,
        newest.display(),
        info.codec,
        info.width,
        info.height,
        info.fps,
        media.ffmpeg.display()
    );
    println!("profile      encoder       wall s    cpu s   x real       MB  note");
    for r in media.bench(&newest, seconds).await? {
        let cpu = r
            .cpu
            .map(|c| format!("{c:.1}"))
            .unwrap_or_else(|| "-".into());
        let speed = if r.ok && r.wall > 0.0 {
            format!("{:.1}", seconds as f64 / r.wall)
        } else {
            "-".into()
        };
        println!(
            "{:<12} {:<11} {:>8.1} {:>8} {:>8} {:>8.1}  {}",
            r.label,
            r.encoder,
            r.wall,
            cpu,
            speed,
            r.size_mb,
            if r.ok {
                "ok".to_string()
            } else {
                format!("failed: {}", r.error)
            }
        );
    }
    println!("Send this table to the maintainer; the defaults per GPU vendor are set from it.");
    Ok(())
}

/// `replaycut stop`: signal the stop event and wait for the instance to go.
fn stop(port: u16) -> Result<()> {
    if platform::stop_instance(port, Duration::from_secs(15))? {
        println!("replaycut stopped");
    } else {
        println!("replaycut is not running");
    }
    Ok(())
}

fn run_service(
    cli: &Cli,
    settings: Settings,
    settings_path: &Path,
    overrides: Overrides,
    data_dir: &Path,
    console: bool,
) -> Result<()> {
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
    let port = settings.port;
    if cli.wait_for_exit {
        // Started by `POST /api/restart`: the old process is still shutting down.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while platform::instance_running(port) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let Some(_instance) = platform::claim_single_instance(port)? else {
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
    let Startup {
        state,
        listener,
        obs_events,
    } = runtime.block_on(startup(
        settings,
        settings_path,
        overrides,
        data_dir,
        cli.dry_run,
    ))?;
    let _ = state.shutdown.set(shutdown.clone());

    // `replaycut stop` sets this event; a plain thread waits on it.
    let stop_event = platform::StopEvent::create(port)?;
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
        let handle = runtime.handle().clone();
        let service = {
            let state = state.clone();
            let shutdown = shutdown.clone();
            std::thread::Builder::new()
                .name("service".into())
                .spawn(move || {
                    let result = runtime.block_on(serve(
                        state,
                        listener,
                        obs_events,
                        shutdown,
                        open_browser,
                    ));
                    tray.quit();
                    result
                })
                .context("cannot start the service thread")?
        };
        if let Err(e) = tray::run(state.clone(), shutdown.clone(), handle) {
            tracing::warn!("tray icon unavailable: {e:#} - running without it");
        }
        service
            .join()
            .map_err(|_| anyhow::anyhow!("service thread panicked"))??;
    }
    #[cfg(not(windows))]
    {
        runtime.block_on(serve(
            state.clone(),
            listener,
            obs_events,
            shutdown,
            open_browser,
        ))?;
    }
    tracing::info!("replaycut stopped");
    Ok(())
}

/// Everything that can fail before the service is up: tools, encoder,
/// state files, the listening socket.
async fn startup(
    settings: Settings,
    settings_path: &Path,
    overrides: Overrides,
    data_dir: &Path,
    dry_run: bool,
) -> Result<Startup> {
    let media_base = Media::locate()?;
    let runtime = Runtime::build(&media_base, &settings, dry_run, None).await?;
    let (obs_events_tx, obs_events_rx) = tokio::sync::mpsc::channel(32);
    let obs = obs_ws::ObsHandle::new(obs_link::config_from(&settings), obs_events_tx);
    let ui_file = resolve_ui_file(&settings.ui_file);
    if !ui_file.is_file() {
        tracing::warn!(
            "UI file {} not found - GET / will fail until it exists",
            ui_file.display()
        );
    }
    let bind = format!("{}:{}", settings.bind, settings.port);
    let state = Arc::new(AppState::load(Boot {
        settings,
        settings_path: settings_path.to_path_buf(),
        overrides,
        data_dir: data_dir.to_path_buf(),
        ui_file,
        media_base,
        runtime,
        dry_run,
        obs,
    })?);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    tracing::info!(
        "replaycut {VERSION} started: clips {}, http://{bind}/, encoder {}, ffmpeg {}, ffmpeg threads {}, priority {:?}, {}{}",
        state.paths().clip_dir.display(),
        state.runtime().encoder.name,
        state.media_base.ffmpeg.display(),
        state.settings().ffmpeg_threads(),
        state.settings().ffmpeg_priority,
        state.runtime().integrations.describe(),
        if state.dry_run { " [DRY RUN: uploads, posts, hotkey, clipboard and toasts are simulated]" } else { "" }
    );
    Ok(Startup {
        state,
        listener,
        obs_events: obs_events_rx,
    })
}

/// What `startup` hands to the server: the state, the socket and the
/// receiver of OBS events (consumed by one task in `serve`).
struct Startup {
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    obs_events: tokio::sync::mpsc::Receiver<obs_ws::ObsEvent>,
}

/// Serve until a shutdown is requested, then give open connections a moment.
async fn serve(
    state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    obs_events: tokio::sync::mpsc::Receiver<obs_ws::ObsEvent>,
    shutdown: Shutdown,
    open_browser: bool,
) -> Result<()> {
    if let Some(j) = update::cleanup_after_start(&state.data_dir) {
        let mut u = state.update.lock();
        u.just_updated = true;
        u.updated_notes = Some(j.notes);
        u.updated_url = Some(j.url);
    }
    tokio::spawn(scanner::run(state.clone()));
    tokio::spawn(obs_ws::run(state.obs.clone()));
    tokio::spawn(obs_link::react(
        state.clone(),
        state.obs.clone(),
        obs_events,
    ));
    tokio::spawn(obs_link::refresh_loop(state.obs.clone()));
    tokio::spawn(crate::state::quota_loop(state.clone()));
    if state.settings().check_updates {
        tokio::spawn(update::run(state.clone()));
    }
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
    let app = http::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    let server = axum::serve(listener, app).with_graceful_shutdown(graceful);
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
