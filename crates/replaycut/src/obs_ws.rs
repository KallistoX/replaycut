//! obs-websocket 5 client (built into OBS 28 and newer). One task keeps
//! the connection: Hello, Identify with the SHA-256 handshake, requests
//! through a channel with a per-request reply, events into the shared
//! status, reconnect with backoff. Read-only apart from the two harmless
//! actions the callers make (`SaveReplayBuffer`, `StartReplayBuffer`).
//!
//! Protocol: https://github.com/obsproject/obs-websocket/blob/master/docs/generated/protocol.md

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::Message;

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STATUS_POLL: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const AFTER_EXIT: Duration = Duration::from_secs(5);
/// EventSubscription bits: General (1) | Config (2) | Outputs (64).
const EVENT_SUBSCRIPTIONS: u64 = 1 | 2 | 64;

/// Where to connect and with what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObsConfig {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
}

impl ObsConfig {
    pub fn url(&self) -> String {
        format!("ws://{}:{}", self.host, self.port)
    }
}

/// A saved replay reported by OBS.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SavedReplay {
    pub path: String,
    pub at: String,
}

/// What the rest of the service and the UI see.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsStatus {
    pub enabled: bool,
    pub connected: bool,
    /// Why there is no connection, in plain words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws_version: Option<String>,
    pub replay_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_saved: Option<SavedReplay>,
    /// True right after `ExitStarted`, until the next connection.
    pub obs_closing: bool,
    /// Profile, video and inputs as last read (see `obs_status`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facts: Option<crate::obs_status::Facts>,
}

/// What happened on the connection; the service reacts (scanner, toasts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObsEvent {
    Connected,
    Disconnected,
    ReplaySaved(String),
    /// `true` = running. Carries whether OBS is still up (false after ExitStarted).
    ReplayStateChanged(bool),
    ProfileChanged,
}

struct Request {
    kind: String,
    data: Value,
    reply: oneshot::Sender<Result<Value>>,
}

/// The handle every caller uses: status, requests, reconfiguration.
pub struct ObsHandle {
    status: Mutex<ObsStatus>,
    config: Mutex<ObsConfig>,
    requests: Mutex<Option<mpsc::Sender<Request>>>,
    reconnect: Notify,
    events: mpsc::Sender<ObsEvent>,
}

impl ObsHandle {
    pub fn new(config: ObsConfig, events: mpsc::Sender<ObsEvent>) -> Arc<Self> {
        let status = ObsStatus {
            enabled: config.enabled,
            ..ObsStatus::default()
        };
        Arc::new(Self {
            status: Mutex::new(status),
            config: Mutex::new(config),
            requests: Mutex::new(None),
            reconnect: Notify::new(),
            events,
        })
    }

    pub fn status(&self) -> ObsStatus {
        self.status.lock().clone()
    }

    pub fn config(&self) -> ObsConfig {
        self.config.lock().clone()
    }

    /// New host, port, password or enabled flag: drop the connection and
    /// connect again (or stop trying).
    pub fn reconfigure(&self, config: ObsConfig) {
        {
            let mut status = self.status.lock();
            status.enabled = config.enabled;
        }
        *self.config.lock() = config;
        self.reconnect.notify_one();
    }

    /// Connect now instead of waiting out the backoff.
    pub fn reconnect_now(&self) {
        self.reconnect.notify_one();
    }

    /// Send a request and wait for its response data.
    pub async fn request(&self, kind: &str, data: Value) -> Result<Value> {
        let tx = self
            .requests
            .lock()
            .clone()
            .ok_or_else(|| anyhow!("OBS is not connected"))?;
        let (reply, rx) = oneshot::channel();
        tx.send(Request {
            kind: kind.to_string(),
            data,
            reply,
        })
        .await
        .map_err(|_| anyhow!("OBS is not connected"))?;
        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => bail!("OBS connection closed while waiting for {kind}"),
            Err(_) => bail!("OBS did not answer {kind} within 5 s"),
        }
    }

    pub fn set_facts(&self, facts: crate::obs_status::Facts) {
        self.status.lock().facts = Some(facts);
    }

    fn set<F: FnOnce(&mut ObsStatus)>(&self, f: F) {
        f(&mut self.status.lock());
    }

    fn emit(&self, event: ObsEvent) {
        let _ = self.events.try_send(event);
    }
}

/// `base64(sha256(base64(sha256(password + salt)) + challenge))`
pub fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let b64 = base64::engine::general_purpose::STANDARD;
    let secret = b64.encode(Sha256::digest(format!("{password}{salt}").as_bytes()));
    b64.encode(Sha256::digest(format!("{secret}{challenge}").as_bytes()))
}

/// Plain words for a close code from the server.
fn close_reason(code: u16, password_required: bool, have_password: bool) -> String {
    match code {
        4009 => "wrong password".into(),
        4003 if password_required && !have_password => "password required".into(),
        4010 => "OBS needs a newer replaycut (unsupported RPC version)".into(),
        4011 => "session invalidated by OBS".into(),
        _ => format!("connection closed by OBS (code {code})"),
    }
}

/// The client task. Runs until the process ends.
pub async fn run(handle: Arc<ObsHandle>) {
    let mut backoff = BACKOFF_MIN;
    let mut last_reason: Option<String> = None;
    loop {
        let config = handle.config();
        if !config.enabled {
            handle.set(|s| {
                s.connected = false;
                s.reason = Some("disabled in the settings".into());
            });
            handle.reconnect.notified().await;
            backoff = BACKOFF_MIN;
            continue;
        }
        match session(&handle, &config).await {
            Ok(SessionEnd::Reconfigured) => {
                backoff = BACKOFF_MIN;
                last_reason = None;
                continue;
            }
            Ok(SessionEnd::ObsExit) => {
                handle.set(|s| {
                    s.connected = false;
                    s.obs_closing = true;
                    s.reason = Some("OBS closed".into());
                });
                handle.emit(ObsEvent::Disconnected);
                tracing::info!("OBS closed - reconnecting when it is back");
                last_reason = Some("OBS closed".into());
                tokio::select! {
                    _ = tokio::time::sleep(AFTER_EXIT) => {}
                    _ = handle.reconnect.notified() => {}
                }
                backoff = BACKOFF_MIN;
            }
            Ok(SessionEnd::Closed(reason)) | Err(SessionError(reason)) => {
                let was_connected = handle.status().connected;
                handle.set(|s| {
                    s.connected = false;
                    s.reason = Some(reason.clone());
                });
                if was_connected {
                    handle.emit(ObsEvent::Disconnected);
                }
                // One log line per new reason, not one per attempt.
                if last_reason.as_deref() != Some(reason.as_str()) {
                    if was_connected || reason.contains("password") {
                        tracing::warn!("OBS: {reason}");
                    } else {
                        tracing::debug!("OBS: {reason}");
                    }
                    last_reason = Some(reason);
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = handle.reconnect.notified() => { backoff = BACKOFF_MIN; continue; }
                }
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

enum SessionEnd {
    Reconfigured,
    ObsExit,
    Closed(String),
}

struct SessionError(String);

impl From<anyhow::Error> for SessionError {
    fn from(e: anyhow::Error) -> Self {
        SessionError(format!("{e:#}"))
    }
}

/// One connection from TCP connect to close.
async fn session(handle: &Arc<ObsHandle>, config: &ObsConfig) -> Result<SessionEnd, SessionError> {
    let url = config.url();
    let connect =
        tokio::time::timeout(REQUEST_TIMEOUT, tokio_tungstenite::connect_async(&url)).await;
    let (ws, _) = match connect {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(SessionError(connect_error(&e, config))),
        Err(_) => return Err(SessionError(format!("no answer from {url} within 5 s"))),
    };
    let (mut sink, mut stream) = ws.split();

    // Hello
    let hello = next_json(&mut stream).await?;
    if hello["op"] != 0 {
        return Err(SessionError("unexpected first message from OBS".into()));
    }
    let d = &hello["d"];
    let ws_version = d["obsWebSocketVersion"].as_str().unwrap_or("?").to_string();
    let password_required = d["authentication"].is_object();
    let mut identify = json!({
        "op": 1,
        "d": { "rpcVersion": 1, "eventSubscriptions": EVENT_SUBSCRIPTIONS }
    });
    if password_required {
        let Some(password) = config.password.as_deref().filter(|p| !p.is_empty()) else {
            return Err(SessionError("password required".into()));
        };
        let salt = d["authentication"]["salt"].as_str().unwrap_or("");
        let challenge = d["authentication"]["challenge"].as_str().unwrap_or("");
        identify["d"]["authentication"] = json!(auth_string(password, salt, challenge));
    }
    sink.send(Message::Text(identify.to_string().into()))
        .await
        .map_err(|e| SessionError(format!("cannot send Identify: {e}")))?;

    // Identified or a close
    let identified = loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => {
                let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if v["op"] == 2 {
                    break v;
                }
            }
            Some(Ok(Message::Close(frame))) => {
                let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                return Err(SessionError(close_reason(
                    code,
                    password_required,
                    config.password.is_some(),
                )));
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(SessionError(format!("{e}"))),
            None => return Err(SessionError("connection closed during Identify".into())),
        }
    };
    let _ = identified;

    // connected
    let (req_tx, mut req_rx) = mpsc::channel::<Request>(16);
    *handle.requests.lock() = Some(req_tx);
    handle.set(|s| {
        s.connected = true;
        s.reason = None;
        s.obs_closing = false;
        s.ws_version = Some(ws_version.clone());
    });
    tracing::info!("OBS connected ({url}, obs-websocket {ws_version})");
    handle.emit(ObsEvent::Connected);

    let mut pending: HashMap<String, (oneshot::Sender<Result<Value>>, Instant)> = HashMap::new();
    let mut counter: u64 = 0;
    let mut poll = tokio::time::interval(STATUS_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ping = tokio::time::interval(Duration::from_secs(20));
    let (status_tx, mut status_rx) = mpsc::channel::<Result<Value>>(2);
    let end = loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(text))) => {
                    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                    match v["op"].as_u64() {
                        Some(7) => {
                            let id = v["d"]["requestId"].as_str().unwrap_or("").to_string();
                            if let Some((reply, _)) = pending.remove(&id) {
                                let d = &v["d"];
                                let result = if d["requestStatus"]["result"] == true {
                                    Ok(d["responseData"].clone())
                                } else {
                                    Err(anyhow!(
                                        "{}: {}",
                                        d["requestType"].as_str().unwrap_or("request"),
                                        d["requestStatus"]["comment"].as_str().unwrap_or("failed")
                                    ))
                                };
                                let _ = reply.send(result);
                            }
                        }
                        Some(5) => {
                            if let Some(end) = handle_event(handle, &v["d"]) {
                                break end;
                            }
                        }
                        _ => {}
                    }
                }
                Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                Some(Ok(Message::Close(frame))) => {
                    let code = frame.map(|f| u16::from(f.code)).unwrap_or(0);
                    break SessionEnd::Closed(close_reason(code, password_required, config.password.is_some()));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => break SessionEnd::Closed(format!("{e}")),
                None => break SessionEnd::Closed("connection closed by OBS".into()),
            },
            Some(req) = req_rx.recv() => {
                counter += 1;
                let id = format!("rc-{counter}");
                let frame = json!({ "op": 6, "d": { "requestType": req.kind, "requestId": id, "requestData": req.data } });
                if let Err(e) = sink.send(Message::Text(frame.to_string().into())).await {
                    let _ = req.reply.send(Err(anyhow!("cannot send {}: {e}", req.kind)));
                    break SessionEnd::Closed(format!("{e}"));
                }
                pending.insert(id, (req.reply, Instant::now()));
            }
            _ = poll.tick() => {
                // Cheap heartbeat that also catches a state change without an event.
                counter += 1;
                let id = format!("rc-{counter}");
                let frame = json!({ "op": 6, "d": { "requestType": "GetReplayBufferStatus", "requestId": id, "requestData": {} } });
                if sink.send(Message::Text(frame.to_string().into())).await.is_ok() {
                    let (tx, rx) = oneshot::channel();
                    pending.insert(id, (tx, Instant::now()));
                    let status_tx = status_tx.clone();
                    tokio::spawn(async move { if let Ok(r) = rx.await { let _ = status_tx.send(r).await; } });
                }
                pending.retain(|_, (_, at)| at.elapsed() < REQUEST_TIMEOUT * 2);
            }
            Some(status) = status_rx.recv() => {
                if let Ok(v) = status {
                    if let Some(active) = v["outputActive"].as_bool() {
                        let changed = handle.status().replay_active != active;
                        handle.set(|s| s.replay_active = active);
                        if changed { handle.emit(ObsEvent::ReplayStateChanged(active)); }
                    }
                }
            }
            _ = ping.tick() => { let _ = sink.send(Message::Ping(Vec::new().into())).await; }
            _ = handle.reconnect.notified() => break SessionEnd::Reconfigured,
        }
    };
    *handle.requests.lock() = None;
    handle.set(|s| {
        s.connected = false;
        s.facts = None;
    });
    let _ = sink.close().await;
    Ok(end)
}

/// An event from OBS; `Some` ends the session (ExitStarted).
fn handle_event(handle: &Arc<ObsHandle>, d: &Value) -> Option<SessionEnd> {
    let data = &d["eventData"];
    match d["eventType"].as_str().unwrap_or("") {
        "ReplayBufferStateChanged" => {
            if let Some(active) = data["outputActive"].as_bool() {
                let changed = handle.status().replay_active != active;
                handle.set(|s| s.replay_active = active);
                if changed {
                    handle.emit(ObsEvent::ReplayStateChanged(active));
                }
            }
        }
        "ReplayBufferSaved" => {
            let path = data["savedReplayPath"].as_str().unwrap_or("").to_string();
            handle.set(|s| {
                s.last_saved = Some(SavedReplay {
                    path: path.clone(),
                    at: crate::util::now_local(),
                })
            });
            handle.emit(ObsEvent::ReplaySaved(path));
        }
        "CurrentProfileChanged" => handle.emit(ObsEvent::ProfileChanged),
        "ExitStarted" => return Some(SessionEnd::ObsExit),
        _ => {}
    }
    None
}

async fn next_json<S>(stream: &mut S) -> Result<Value, SessionError>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let msg = tokio::time::timeout(REQUEST_TIMEOUT, stream.next())
        .await
        .map_err(|_| SessionError("OBS did not send Hello within 5 s".into()))?;
    match msg {
        Some(Ok(Message::Text(text))) => Ok(serde_json::from_str(&text).unwrap_or(Value::Null)),
        Some(Ok(Message::Close(_))) | None => {
            Err(SessionError("connection closed before Hello".into()))
        }
        Some(Ok(_)) => Ok(Value::Null),
        Some(Err(e)) => Err(SessionError(format!("{e}"))),
    }
}

fn connect_error(e: &tokio_tungstenite::tungstenite::Error, config: &ObsConfig) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match e {
        Error::Io(io) if io.kind() == std::io::ErrorKind::ConnectionRefused => format!(
            "nothing listens on {}:{} - is OBS running with the WebSocket server enabled?",
            config.host, config.port
        ),
        Error::Io(io) => format!("{}:{}: {io}", config.host, config.port),
        other => format!("{}:{}: {other}", config.host, config.port),
    }
}

/// Reads the whole status after a connection: version and replay buffer.
/// Failures are logged, not fatal; the fields stay `None`.
pub async fn refresh_basics(handle: &ObsHandle) {
    if let Ok(v) = handle.request("GetVersion", json!({})).await {
        handle.set(|s| {
            s.version = v["obsVersion"].as_str().map(str::to_string);
        });
    }
    if let Ok(v) = handle.request("GetReplayBufferStatus", json!({})).await {
        if let Some(active) = v["outputActive"].as_bool() {
            let changed = handle.status().replay_active != active;
            handle.set(|s| s.replay_active = active);
            if changed {
                handle.emit(ObsEvent::ReplayStateChanged(active));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn auth_matches_the_protocol_example() {
        // Inputs from docs/generated/protocol.md ("Creating an authentication
        // string"); the expected value was computed independently with
        // .NET SHA256 + Base64 over the same steps.
        assert_eq!(
            auth_string(
                "supersecretpassword",
                "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=",
                "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY="
            ),
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }

    #[test]
    fn close_reasons_are_plain_words() {
        assert_eq!(close_reason(4009, true, true), "wrong password");
        assert_eq!(close_reason(4003, true, false), "password required");
        assert!(close_reason(4000, false, false).contains("4000"));
    }

    /// A fake OBS: Hello (with or without auth), checks Identify, answers
    /// GetVersion and GetReplayBufferStatus, sends one event, then closes
    /// when told to. Returns the port and a sender that ends the server.
    pub(super) async fn fake_obs(password: Option<&'static str>) -> (u16, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let (mut sink, mut stream) = ws.split();
            let salt = "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAHo3ixNI=";
            let challenge = "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY=";
            let mut hello =
                json!({ "op": 0, "d": { "obsWebSocketVersion": "5.5.2", "rpcVersion": 1 } });
            if password.is_some() {
                hello["d"]["authentication"] = json!({ "challenge": challenge, "salt": salt });
            }
            sink.send(Message::Text(hello.to_string().into()))
                .await
                .unwrap();
            let identify: Value = match stream.next().await {
                Some(Ok(Message::Text(t))) => serde_json::from_str(&t).unwrap(),
                _ => return,
            };
            assert_eq!(identify["op"], 1);
            if let Some(pw) = password {
                let expected = auth_string(pw, salt, challenge);
                if identify["d"]["authentication"] != expected {
                    let frame = tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code:
                            tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(
                                4009,
                            ),
                        reason: "auth failed".into(),
                    };
                    let _ = sink.send(Message::Close(Some(frame))).await;
                    return;
                }
            }
            sink.send(Message::Text(
                json!({ "op": 2, "d": { "negotiatedRpcVersion": 1 } })
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
            loop {
                tokio::select! {
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(t))) => {
                            let v: Value = serde_json::from_str(&t).unwrap();
                            let id = v["d"]["requestId"].clone();
                            let data = match v["d"]["requestType"].as_str().unwrap_or("") {
                                "GetVersion" => json!({ "obsVersion": "30.2.3", "obsWebSocketVersion": "5.5.2" }),
                                "GetReplayBufferStatus" => json!({ "outputActive": true }),
                                "GetProfileList" => json!({ "currentProfileName": "Gaming", "profiles": ["Gaming"] }),
                                "GetProfileParameter" => {
                                    let name = v["d"]["requestData"]["parameterName"].as_str().unwrap_or("");
                                    let value = match name {
                                        "Mode" => "Advanced",
                                        "RecFilePath" => r"C:\Users\you\Videos\Clips",
                                        "RecFormat2" => "mkv",
                                        "RecEncoder" => "jim_hevc_nvenc",
                                        "RecTracks" => "15",
                                        "RecRBTime" => "300",
                                        _ => "",
                                    };
                                    json!({ "parameterValue": value, "defaultParameterValue": null })
                                }
                                "GetVideoSettings" => json!({ "baseWidth": 2560, "baseHeight": 1440, "outputWidth": 1920, "outputHeight": 1080, "fpsNumerator": 60, "fpsDenominator": 1 }),
                                "GetInputList" => json!({ "inputs": [
                                    { "inputName": "Mic", "inputKind": "wasapi_input_capture" },
                                    { "inputName": "Desktop", "inputKind": "wasapi_output_capture" },
                                    { "inputName": "Discord", "inputKind": "wasapi_process_output_capture" },
                                    { "inputName": "Game", "inputKind": "game_capture" }
                                ] }),
                                "GetInputAudioTracks" => {
                                    let input = v["d"]["requestData"]["inputName"].as_str().unwrap_or("");
                                    let tracks = match input {
                                        "Mic" => json!({ "1": true, "2": true, "3": false, "4": false, "5": false, "6": false }),
                                        "Desktop" => json!({ "1": true, "2": false, "3": true, "4": false, "5": false, "6": false }),
                                        "Discord" => json!({ "1": true, "2": false, "3": false, "4": true, "5": false, "6": false }),
                                        _ => {
                                            let reply = json!({ "op": 7, "d": { "requestType": "GetInputAudioTracks", "requestId": id, "requestStatus": { "result": false, "code": 604, "comment": "The specified input does not support audio." } } });
                                            sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                                            continue;
                                        }
                                    };
                                    json!({ "inputAudioTracks": tracks })
                                }
                                "SaveReplayBuffer" => {
                                    // answer, then the event OBS sends once the file is written
                                    let reply = json!({ "op": 7, "d": { "requestType": "SaveReplayBuffer", "requestId": id, "requestStatus": { "result": true, "code": 100 } } });
                                    sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                                    let event = json!({ "op": 5, "d": { "eventType": "ReplayBufferSaved", "eventIntent": 64, "eventData": { "savedReplayPath": "C:/Videos/Replay.mkv" } } });
                                    sink.send(Message::Text(event.to_string().into())).await.unwrap();
                                    continue;
                                }
                                "StopIt" => {
                                    let event = json!({ "op": 5, "d": { "eventType": "ReplayBufferStateChanged", "eventIntent": 64, "eventData": { "outputActive": false, "outputState": "OBS_WEBSOCKET_OUTPUT_STOPPED" } } });
                                    sink.send(Message::Text(event.to_string().into())).await.unwrap();
                                    let reply = json!({ "op": 7, "d": { "requestType": "StopIt", "requestId": id, "requestStatus": { "result": true, "code": 100 } } });
                                    sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                                    continue;
                                }
                                other => {
                                    let reply = json!({ "op": 7, "d": { "requestType": other, "requestId": id, "requestStatus": { "result": false, "code": 204, "comment": "unknown request" } } });
                                    sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                                    continue;
                                }
                            };
                            let reply = json!({ "op": 7, "d": { "requestType": v["d"]["requestType"], "requestId": id, "requestStatus": { "result": true, "code": 100 }, "responseData": data } });
                            sink.send(Message::Text(reply.to_string().into())).await.unwrap();
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                        _ => {}
                    },
                    _ = &mut stop_rx => {
                        let event = json!({ "op": 5, "d": { "eventType": "ExitStarted", "eventIntent": 1, "eventData": {} } });
                        let _ = sink.send(Message::Text(event.to_string().into())).await;
                        let _ = sink.close().await;
                        return;
                    }
                }
            }
        });
        (port, stop_tx)
    }

    async fn wait_until<F: Fn() -> bool>(f: F, what: &str) {
        for _ in 0..100 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for {what}");
    }

    #[tokio::test]
    async fn connects_with_password_requests_and_events() {
        let (port, stop) = fake_obs(Some("hunter2")).await;
        let (tx, mut rx) = mpsc::channel(16);
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                password: Some("hunter2".into()),
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        wait_until(|| handle.status().connected, "connection").await;
        assert_eq!(rx.recv().await, Some(ObsEvent::Connected));
        refresh_basics(&handle).await;
        assert_eq!(handle.status().version.as_deref(), Some("30.2.3"));
        assert!(handle.status().replay_active);
        assert_eq!(rx.recv().await, Some(ObsEvent::ReplayStateChanged(true)));

        let v = handle.request("SaveReplayBuffer", json!({})).await.unwrap();
        assert!(v.is_null());
        assert_eq!(
            rx.recv().await,
            Some(ObsEvent::ReplaySaved("C:/Videos/Replay.mkv".into()))
        );
        assert_eq!(
            handle.status().last_saved.unwrap().path,
            "C:/Videos/Replay.mkv"
        );

        handle.request("StopIt", json!({})).await.unwrap();
        assert_eq!(rx.recv().await, Some(ObsEvent::ReplayStateChanged(false)));
        assert!(!handle.status().replay_active);

        let err = handle.request("Nope", json!({})).await.unwrap_err();
        assert!(err.to_string().contains("unknown request"), "{err}");

        let _ = stop.send(());
        wait_until(|| !handle.status().connected, "disconnect").await;
        assert_eq!(rx.recv().await, Some(ObsEvent::Disconnected));
        assert!(handle.status().obs_closing);
        assert_eq!(handle.status().reason.as_deref(), Some("OBS closed"));
        task.abort();
    }

    #[tokio::test]
    async fn wrong_password_is_reported() {
        let (port, _stop) = fake_obs(Some("right")).await;
        let (tx, _rx) = mpsc::channel(16);
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                password: Some("wrong".into()),
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        wait_until(
            || handle.status().reason.as_deref() == Some("wrong password"),
            "reason",
        )
        .await;
        assert!(!handle.status().connected);
        task.abort();
    }

    #[tokio::test]
    async fn missing_password_and_nothing_listening() {
        let (port, _stop) = fake_obs(Some("secret")).await;
        let (tx, _rx) = mpsc::channel(16);
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                password: None,
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        wait_until(
            || handle.status().reason.as_deref() == Some("password required"),
            "reason",
        )
        .await;
        task.abort();

        let (tx, _rx) = mpsc::channel(16);
        let free = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port: free,
                password: None,
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        wait_until(
            || {
                handle
                    .status()
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.contains("nothing listens"))
            },
            "refused",
        )
        .await;
        assert!(handle.request("GetVersion", json!({})).await.is_err());
        task.abort();
    }

    #[tokio::test]
    async fn disabled_waits_and_reconfigure_connects() {
        let (port, _stop) = fake_obs(None).await;
        let (tx, _rx) = mpsc::channel(16);
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: false,
                host: "127.0.0.1".into(),
                port,
                password: None,
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        wait_until(
            || handle.status().reason.as_deref() == Some("disabled in the settings"),
            "disabled",
        )
        .await;
        handle.reconfigure(ObsConfig {
            enabled: true,
            host: "127.0.0.1".into(),
            port,
            password: None,
        });
        wait_until(|| handle.status().connected, "connection").await;
        task.abort();
    }
}

#[cfg(test)]
mod facts_tests {
    use super::tests::fake_obs;
    use super::*;
    use crate::obs_status::{checks, read_facts};

    #[tokio::test]
    async fn reads_facts_and_derives_checks() {
        let (port, _stop) = fake_obs(None).await;
        let (tx, _rx) = mpsc::channel(16);
        let handle = ObsHandle::new(
            ObsConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                password: None,
            },
            tx,
        );
        let task = tokio::spawn(run(handle.clone()));
        for _ in 0..100 {
            if handle.status().connected {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let facts = read_facts(&handle).await;
        assert_eq!(facts.profile.name, "Gaming");
        assert_eq!(facts.profile.mode, "Advanced");
        assert_eq!(facts.profile.format.as_deref(), Some("mkv"));
        assert_eq!(facts.profile.encoder.as_deref(), Some("jim_hevc_nvenc"));
        assert_eq!(facts.profile.replay_seconds, Some(300));
        assert_eq!(facts.profile.rec_tracks, 15);
        assert_eq!(
            (facts.video.width, facts.video.height, facts.video.fps),
            (1920, 1080, 60.0)
        );
        assert_eq!(facts.inputs.len(), 4);
        assert_eq!(facts.inputs[0].tracks, vec![1, 2]);
        assert!(
            facts.inputs[3].tracks.is_empty(),
            "video-only input has no tracks"
        );

        let settings = crate::settings::Settings {
            clip_dir: r"C:\Users\you\Videos\Clips".into(),
            ..crate::settings::Settings::default()
        };
        let rows = checks(&facts, true, &settings);
        assert!(rows.iter().all(|c| c.status == "ok"), "{rows:?}");
        assert!(rows
            .iter()
            .any(|c| c.id == "codec" && c.detail.contains("HEVC")));
        task.abort();
    }
}
