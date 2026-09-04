//! Facts about the OBS configuration read through obs-websocket, and the
//! check rows the OBS page, the wizard and the diagnostics derive from
//! them. Read-only; every suggestion names the OBS menu path.

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

use crate::obs_ws::ObsHandle;
use crate::settings::Settings;

/// The recording profile as OBS reports it.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Profile {
    pub name: String,
    /// `Simple` or `Advanced`.
    pub mode: String,
    pub rec_path: Option<String>,
    pub format: Option<String>,
    pub encoder: Option<String>,
    /// Replay buffer length in seconds.
    pub replay_seconds: Option<u32>,
    /// Advanced mode: which of the tracks 1-6 are written (bitmask).
    pub rec_tracks: u32,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Video {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub name: String,
    pub kind: String,
    /// Tracks 1-6 this input is mixed into (empty for non-audio inputs).
    pub tracks: Vec<u32>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    pub profile: Profile,
    pub video: Video,
    pub inputs: Vec<Input>,
    pub checked_at: String,
}

/// What kind of sound an input is, for the track heuristic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Microphone,
    Desktop,
    Application,
    Other,
}

pub fn role_of(kind: &str) -> Role {
    let k = kind.to_ascii_lowercase();
    if k.contains("input_capture") {
        Role::Microphone
    } else if k.contains("process_output_capture") || k.contains("application") {
        Role::Application
    } else if k.contains("output_capture") {
        Role::Desktop
    } else {
        Role::Other
    }
}

/// Bitmask `RecTracks` -> the track numbers that are written.
pub fn tracks_from_mask(mask: u32) -> Vec<u32> {
    (1..=6).filter(|t| mask & (1 << (t - 1)) != 0).collect()
}

/// `inputAudioTracks` object `{"1": true, "2": false, ...}` -> track numbers.
pub fn tracks_from_object(v: &Value) -> Vec<u32> {
    let mut out: Vec<u32> = v
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(_, on)| on.as_bool() == Some(true))
                .filter_map(|(k, _)| k.parse::<u32>().ok())
                .collect()
        })
        .unwrap_or_default();
    out.sort_unstable();
    out
}

/// Encoder id -> codec name the browser hint understands.
pub fn codec_of_encoder(encoder: &str) -> &'static str {
    let e = encoder.to_ascii_lowercase();
    if e.contains("hevc") || e.contains("h265") {
        "hevc"
    } else if e.contains("av1") {
        "av1"
    } else {
        "h264"
    }
}

fn same_folder(a: &str, b: &Path) -> bool {
    let norm = |s: &str| {
        s.replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase()
    };
    norm(a) == norm(&b.to_string_lossy())
}

async fn param(handle: &ObsHandle, category: &str, name: &str) -> Option<String> {
    let v = handle
        .request(
            "GetProfileParameter",
            json!({ "parameterCategory": category, "parameterName": name }),
        )
        .await
        .ok()?;
    let value = v["parameterValue"].as_str()?;
    (!value.is_empty()).then(|| value.to_string())
}

/// Read everything the OBS page shows. Failures leave fields empty; the
/// connection itself is not judged here.
pub async fn read_facts(handle: &ObsHandle) -> Facts {
    let mut facts = Facts::default();
    if let Ok(v) = handle.request("GetProfileList", json!({})).await {
        facts.profile.name = v["currentProfileName"].as_str().unwrap_or("").to_string();
    }
    let mode = param(handle, "Output", "Mode")
        .await
        .unwrap_or_else(|| "Simple".into());
    let advanced = mode.eq_ignore_ascii_case("Advanced");
    facts.profile.mode = if advanced { "Advanced" } else { "Simple" }.into();
    let (cat, path_key, enc_key) = if advanced {
        ("AdvOut", "RecFilePath", "RecEncoder")
    } else {
        ("SimpleOutput", "FilePath", "RecEncoder")
    };
    facts.profile.rec_path = param(handle, cat, path_key).await;
    facts.profile.format = param(handle, cat, "RecFormat2")
        .await
        .map(|f| f.to_ascii_lowercase());
    facts.profile.encoder = param(handle, cat, enc_key).await;
    if !advanced && facts.profile.encoder.is_none() {
        // Simple mode with "same as stream": the stream encoder applies.
        facts.profile.encoder = param(handle, "SimpleOutput", "StreamEncoder").await;
    }
    facts.profile.replay_seconds = param(handle, cat, "RecRBTime")
        .await
        .and_then(|s| s.parse().ok());
    facts.profile.rec_tracks = if advanced {
        param(handle, "AdvOut", "RecTracks")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    } else {
        1
    };
    if let Ok(v) = handle.request("GetVideoSettings", json!({})).await {
        facts.video.width = v["outputWidth"].as_u64().unwrap_or(0) as u32;
        facts.video.height = v["outputHeight"].as_u64().unwrap_or(0) as u32;
        let num = v["fpsNumerator"].as_f64().unwrap_or(0.0);
        let den = v["fpsDenominator"].as_f64().unwrap_or(1.0).max(1.0);
        facts.video.fps = ((num / den) * 100.0).round() / 100.0;
    }
    if let Ok(v) = handle.request("GetInputList", json!({})).await {
        for i in v["inputs"].as_array().cloned().unwrap_or_default() {
            let name = i["inputName"].as_str().unwrap_or("").to_string();
            let kind = i["inputKind"].as_str().unwrap_or("").to_string();
            if name.is_empty() {
                continue;
            }
            // Only audio-capable inputs answer; the rest are left with no tracks.
            let tracks = match handle
                .request("GetInputAudioTracks", json!({ "inputName": name }))
                .await
            {
                Ok(t) => tracks_from_object(&t["inputAudioTracks"]),
                Err(_) => Vec::new(),
            };
            facts.inputs.push(Input { name, kind, tracks });
        }
    }
    facts.checked_at = crate::util::now_local();
    facts
}

/// One row of the OBS page.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Check {
    pub id: &'static str,
    pub label: &'static str,
    /// `ok`, `warn`, `problem`, `skip`.
    pub status: &'static str,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// A button the UI offers: `start-replay-buffer`, `adopt-folder`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<&'static str>,
}

fn check(id: &'static str, label: &'static str, status: &'static str, detail: String) -> Check {
    Check {
        id,
        label,
        status,
        detail,
        fix: None,
        action: None,
    }
}

/// Facts against what replaycut expects. `replay_active` comes from the
/// live status, everything else from the facts.
pub fn checks(facts: &Facts, replay_active: bool, settings: &Settings) -> Vec<Check> {
    let mut out = Vec::new();
    let p = &facts.profile;

    // replay buffer
    let length = p
        .replay_seconds
        .map(|s| format!(" ({s} s)"))
        .unwrap_or_default();
    out.push(if replay_active {
        check("replay", "Replay buffer", "ok", format!("running{length}"))
    } else {
        let mut c = check(
            "replay",
            "Replay buffer",
            "problem",
            format!("stopped{length} - F9 does nothing until it runs"),
        );
        c.fix = Some("OBS main window › Controls › Start Replay Buffer, or the button here. It stops when OBS closes; the setting Settings › Output › Replay Buffer has no autostart, a scene collection can start it though.".into());
        c.action = Some("start-replay-buffer");
        c
    });

    // folder
    match &p.rec_path {
        Some(path) if same_folder(path, &settings.clip_dir) => out.push(check(
            "folder",
            "Recording folder",
            "ok",
            format!("OBS writes to {path} - same as replaycut watches"),
        )),
        Some(path) => {
            let mut c = check(
                "folder",
                "Recording folder",
                "problem",
                format!(
                    "OBS writes to {path}, replaycut watches {} - new replays will not show up",
                    settings.clip_dir.display()
                ),
            );
            c.fix = Some(format!(
                "Use the OBS folder in replaycut (button), or change OBS: Settings › Output › Recording › Recording Path to {}.",
                settings.clip_dir.display()
            ));
            c.action = Some("adopt-folder");
            out.push(c);
        }
        None => out.push(check(
            "folder",
            "Recording folder",
            "warn",
            "OBS did not report a recording folder".into(),
        )),
    }

    // format
    match p.format.as_deref() {
        Some("mkv") => out.push(check("format", "Recording format", "ok", "mkv".into())),
        Some(other) => {
            let mut c = check(
                "format",
                "Recording format",
                "problem",
                format!("OBS records {other}. Replays cannot be read while OBS writes them, and a crash loses the file."),
            );
            c.fix = Some("Set Settings › Output › Recording › Recording Format to mkv. Takes effect for the next replay; the buffer keeps running.".into());
            out.push(c);
        }
        None => out.push(check(
            "format",
            "Recording format",
            "warn",
            "OBS did not report the format".into(),
        )),
    }

    // encoder / codec
    match p.encoder.as_deref() {
        Some(enc) => {
            let codec = codec_of_encoder(enc);
            let video = if facts.video.width > 0 {
                format!(
                    " · {}x{} {} fps",
                    facts.video.width, facts.video.height, facts.video.fps
                )
            } else {
                String::new()
            };
            out.push(check(
                "codec",
                "Encoder",
                "ok",
                format!(
                    "{enc} ({}){video}. {}",
                    codec.to_ascii_uppercase(),
                    match codec {
                        "h264" => "Every browser plays the preview.",
                        "hevc" => "Chrome, Edge and Firefox on this PC play the preview; older phones may not.",
                        _ => "AV1: Chrome and Firefox play the preview, Safari may not.",
                    }
                ),
            ))
        }
        None => out.push(check(
            "codec",
            "Encoder",
            "warn",
            "OBS did not report the encoder".into(),
        )),
    }

    // audio tracks
    if p.mode != "Advanced" {
        let mut c = check(
            "tracks",
            "Audio tracks",
            "warn",
            "Simple output mode records one track (the mix). Sharing works; the choices 'Game only' and 'Game + microphone' need four tracks.".into(),
        );
        c.fix = Some("Optional: Settings › Output › Output Mode = Advanced, tab Recording, Audio Track 1-4; then in the mixer's Advanced Audio Properties tick track 2 for the microphone, 3 for the desktop (game), 4 for the voice chat.".into());
        out.push(c);
    } else {
        let written = tracks_from_mask(p.rec_tracks);
        let audio: Vec<&Input> = facts
            .inputs
            .iter()
            .filter(|i| !i.tracks.is_empty())
            .collect();
        let on = |t: u32| -> Vec<&Input> {
            audio
                .iter()
                .copied()
                .filter(|i| i.tracks.contains(&t))
                .collect()
        };
        let names = |v: &[&Input]| -> String {
            if v.is_empty() {
                "nothing".into()
            } else {
                v.iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        };
        let only_role =
            |v: &[&Input], role: Role| !v.is_empty() && v.iter().all(|i| role_of(&i.kind) == role);
        let t2 = on(2);
        let t3 = on(3);
        let t4 = on(4);
        let table = format!(
            "track 1: {} · track 2: {} · track 3: {} · track 4: {}",
            names(&on(1)),
            names(&t2),
            names(&t3),
            names(&t4)
        );
        if written.len() < 4 || !written.contains(&1) {
            let mut c = check(
                "tracks",
                "Audio tracks",
                "warn",
                format!(
                    "OBS writes track{} {} into the file. replaycut expects 1 = everything, 2 = microphone, 3 = game, 4 = voice chat. {table}",
                    if written.len() == 1 { "" } else { "s" },
                    written.iter().map(|t| t.to_string()).collect::<Vec<_>>().join(", ")
                ),
            );
            c.fix = Some("Settings › Output › Recording › Audio Track: tick 1, 2, 3 and 4.".into());
            out.push(c);
        } else if only_role(&t2, Role::Microphone)
            && only_role(&t3, Role::Desktop)
            && (t4.is_empty() || only_role(&t4, Role::Application))
        {
            out.push(check(
                "tracks",
                "Audio tracks",
                "ok",
                format!("4 tracks in the expected order. {table}"),
            ));
        } else {
            let mut c = check(
                "tracks",
                "Audio tracks",
                "warn",
                format!("4 tracks, but not in the expected order (2 = microphone, 3 = game, 4 = voice chat). {table}"),
            );
            c.fix = Some("In the mixer, Advanced Audio Properties: tick track 2 for the microphone only, track 3 for the desktop audio (the game) only, track 4 for the voice chat capture only. Track 1 keeps everything. Applies to the next replay.".into());
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            profile: Profile {
                name: "Gaming".into(),
                mode: "Advanced".into(),
                rec_path: Some("C:\\Users\\you\\Videos\\Clips".into()),
                format: Some("mkv".into()),
                encoder: Some("jim_nvenc".into()),
                replay_seconds: Some(300),
                rec_tracks: 15,
            },
            video: Video {
                width: 1920,
                height: 1080,
                fps: 60.0,
            },
            inputs: vec![
                Input {
                    name: "Mic".into(),
                    kind: "wasapi_input_capture".into(),
                    tracks: vec![1, 2],
                },
                Input {
                    name: "Desktop".into(),
                    kind: "wasapi_output_capture".into(),
                    tracks: vec![1, 3],
                },
                Input {
                    name: "Discord".into(),
                    kind: "wasapi_process_output_capture".into(),
                    tracks: vec![1, 4],
                },
                Input {
                    name: "Game".into(),
                    kind: "game_capture".into(),
                    tracks: vec![],
                },
            ],
            checked_at: String::new(),
        }
    }

    fn settings() -> Settings {
        Settings {
            clip_dir: "C:\\Users\\you\\Videos\\Clips".into(),
            ..Settings::default()
        }
    }

    #[test]
    fn masks_and_roles() {
        assert_eq!(tracks_from_mask(15), vec![1, 2, 3, 4]);
        assert_eq!(tracks_from_mask(1), vec![1]);
        assert_eq!(
            tracks_from_object(&json!({"1": true, "2": false, "3": true})),
            vec![1, 3]
        );
        assert_eq!(role_of("wasapi_input_capture"), Role::Microphone);
        assert_eq!(role_of("wasapi_output_capture"), Role::Desktop);
        assert_eq!(role_of("wasapi_process_output_capture"), Role::Application);
        assert_eq!(role_of("game_capture"), Role::Other);
        assert_eq!(codec_of_encoder("jim_nvenc"), "h264");
        assert_eq!(codec_of_encoder("jim_hevc_nvenc"), "hevc");
        assert_eq!(codec_of_encoder("jim_av1_nvenc"), "av1");
    }

    #[test]
    fn everything_matches() {
        let c = checks(&facts(), true, &settings());
        let by = |id: &str| c.iter().find(|x| x.id == id).unwrap();
        assert_eq!(by("replay").status, "ok");
        assert_eq!(by("folder").status, "ok");
        assert_eq!(by("format").status, "ok");
        assert_eq!(by("codec").status, "ok");
        assert_eq!(by("tracks").status, "ok", "{}", by("tracks").detail);
    }

    #[test]
    fn suggestions_with_paths_and_actions() {
        let mut f = facts();
        f.profile.format = Some("mp4".into());
        f.profile.rec_path = Some("D:\\Other".into());
        f.inputs[0].tracks = vec![1, 3];
        f.inputs[1].tracks = vec![1, 2];
        let c = checks(&f, false, &settings());
        let by = |id: &str| c.iter().find(|x| x.id == id).unwrap();
        assert_eq!(by("replay").status, "problem");
        assert_eq!(by("replay").action, Some("start-replay-buffer"));
        assert_eq!(by("folder").status, "problem");
        assert_eq!(by("folder").action, Some("adopt-folder"));
        assert!(by("format")
            .fix
            .as_deref()
            .unwrap()
            .contains("Recording Format"));
        assert_eq!(by("tracks").status, "warn");
        assert!(by("tracks")
            .fix
            .as_deref()
            .unwrap()
            .contains("Advanced Audio Properties"));
    }

    #[test]
    fn simple_mode_is_a_hint_not_a_problem() {
        let mut f = facts();
        f.profile.mode = "Simple".into();
        f.profile.rec_tracks = 1;
        let c = checks(&f, true, &settings());
        let t = c.iter().find(|x| x.id == "tracks").unwrap();
        assert_eq!(t.status, "warn");
        assert!(t.detail.contains("Simple output mode"));
    }

    #[test]
    fn folder_comparison_ignores_case_and_slashes() {
        assert!(same_folder(
            "c:/users/you/videos/clips/",
            Path::new("C:\\Users\\You\\Videos\\Clips")
        ));
        assert!(!same_folder(
            "C:\\Other",
            Path::new("C:\\Users\\you\\Videos")
        ));
    }
}
