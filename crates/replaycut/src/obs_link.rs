//! Glue between the obs-websocket client and the service: the config from
//! settings plus the Credential Manager, and the reactions to events
//! (scanner wake-up, replay-buffer toast, fact refresh).

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::credentials;
use crate::obs_status;
use crate::obs_ws::{self, ObsConfig, ObsEvent, ObsHandle};
use crate::settings::Settings;
use crate::state::AppState;
use crate::toast::{self, Toast};

/// Host, port and enabled from the settings, the password from the
/// Credential Manager.
pub fn config_from(settings: &Settings) -> ObsConfig {
    let password = credentials::read(credentials::OBS_WEBSOCKET)
        .ok()
        .flatten()
        .map(|c| c.secret)
        .filter(|p| !p.is_empty());
    ObsConfig {
        enabled: settings.obs.enabled,
        host: settings.obs.host.clone(),
        port: settings.obs.port,
        password,
    }
}

/// Re-reads the facts every 30 s while connected (profile edits in OBS
/// do not all raise events).
pub async fn refresh_loop(handle: Arc<ObsHandle>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        if handle.status().connected {
            let facts = obs_status::read_facts(&handle).await;
            if handle.status().connected {
                handle.set_facts(facts);
            }
        }
    }
}

/// Reacts to what the client reports. Runs until the process ends.
pub async fn react(state: Arc<AppState>, handle: Arc<ObsHandle>, mut rx: mpsc::Receiver<ObsEvent>) {
    let mut was_active = false;
    while let Some(event) = rx.recv().await {
        match event {
            ObsEvent::Connected => {
                obs_ws::refresh_basics(&handle).await;
                handle.set_facts(obs_status::read_facts(&handle).await);
                was_active = handle.status().replay_active;
                state.tray_changed();
            }
            ObsEvent::Disconnected => {
                was_active = false;
                state.tray_changed();
            }
            ObsEvent::ReplaySaved(path) => {
                tracing::info!("OBS saved a replay: {path}");
                // The scanner still applies its rules (age, exclusive open);
                // waking it just saves the poll interval.
                state.scan_wake.notify_one();
            }
            ObsEvent::ReplayStateChanged(active) => {
                let status = handle.status();
                if !active && was_active && !status.obs_closing {
                    tracing::warn!(
                        "OBS replay buffer stopped - F9 does nothing until it runs again"
                    );
                    toast::show(&state, Toast::replay_buffer_stopped(&state.ui_url()));
                } else if active {
                    tracing::info!("OBS replay buffer running");
                }
                was_active = active;
                state.tray_changed();
            }
            ObsEvent::ProfileChanged => {
                obs_ws::refresh_basics(&handle).await;
                handle.set_facts(obs_status::read_facts(&handle).await);
            }
        }
    }
}
