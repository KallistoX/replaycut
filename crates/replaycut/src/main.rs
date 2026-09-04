//! replaycut - clip manager for the OBS replay buffer.

mod http;
mod media;
mod platform;
mod scanner;
mod settings;
mod state;
mod util;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::media::Media;
use crate::settings::Settings;
use crate::state::{AppState, Paths, VERSION};

#[derive(Parser, Debug)]
#[command(version, about = "Clip manager for the OBS replay buffer")]
struct Cli {
    /// Directory for settings.json, state files and logs.
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Settings file (default: <data-dir>/settings.json).
    #[arg(long)]
    settings: Option<PathBuf>,
    /// Override clipDir from the settings.
    #[arg(long)]
    clip_dir: Option<PathBuf>,
    /// Override port from the settings.
    #[arg(long)]
    port: Option<u16>,
    /// Override the bind address (e.g. 127.0.0.1 for local testing).
    #[arg(long)]
    bind: Option<String>,
    /// Override the UI file.
    #[arg(long)]
    ui: Option<PathBuf>,
    /// Override the log level (error, warn, info, debug, trace).
    #[arg(long)]
    log_level: Option<String>,
    /// Encode for real but simulate uploads, posts, the hotkey and the clipboard.
    #[arg(long)]
    dry_run: bool,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = cli
        .data_dir
        .clone()
        .unwrap_or_else(settings::default_data_dir);
    let settings_path = cli
        .settings
        .clone()
        .unwrap_or_else(|| data_dir.join("settings.json"));
    let mut settings = Settings::load_or_create(&settings_path)?;
    if let Some(d) = cli.clip_dir {
        settings.clip_dir = d;
    }
    if let Some(p) = cli.port {
        settings.port = p;
    }
    if let Some(b) = cli.bind {
        settings.bind = b;
    }
    if let Some(u) = cli.ui {
        settings.ui_file = u;
    }
    if let Some(l) = cli.log_level {
        settings.log_level = l;
    }
    settings
        .validate()
        .with_context(|| format!("invalid settings in {}", settings_path.display()))?;

    let _log_guard = init_logging(&data_dir.join("logs"), &settings.log_level)?;
    let media = Media::locate()?;
    let encoder = media.detect_encoder(&settings.encoder).await?;
    let ui_file = resolve_ui_file(&settings.ui_file);
    if !ui_file.is_file() {
        tracing::warn!(
            "UI file {} not found - GET / will fail until it exists",
            ui_file.display()
        );
    }
    let paths = Paths::new(&settings.clip_dir, &data_dir, ui_file);
    let bind = format!("{}:{}", settings.bind, settings.port);
    let state = Arc::new(AppState::load(
        settings,
        paths,
        media,
        encoder,
        cli.dry_run,
    )?);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot listen on {bind}"))?;
    tracing::info!(
        "replaycut {VERSION} started: clips {}, http://{bind}/, encoder {}, ffmpeg {}, ffmpeg threads {}, priority {:?}{}",
        state.paths.clip_dir.display(),
        state.encoder.name,
        state.media.ffmpeg.display(),
        state.settings.ffmpeg_threads(),
        state.settings.ffmpeg_priority,
        if state.dry_run { " [DRY RUN: uploads, posts, hotkey and clipboard are simulated]" } else { "" }
    );

    tokio::spawn(scanner::run(state.clone()));
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("replaycut stopped");
    Ok(())
}

fn resolve_ui_file(configured: &std::path::Path) -> PathBuf {
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

fn init_logging(
    logs_dir: &std::path::Path,
    level: &str,
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
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_timer(local_time())
                .with_writer(std::io::stdout),
        )
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown requested");
}
