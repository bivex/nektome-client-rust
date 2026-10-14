use crate::{
    auth,
    domain::{AppConfig, Dialog},
    persistence::{Store, VoicePreferences},
    protocol,
    transport::{ChatTransport, VoiceTransportClient},
    voice,
    voice_session::{VoiceSession, VoiceState},
};
use base64::Engine as _;
use chrono::{Local, TimeZone};
use eframe::egui;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
#[cfg(feature = "webrtc-media")]
use std::collections::HashMap;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
};
use tokio::sync::mpsc as tokio_mpsc;

enum Command {
    Connect,
    Disconnect,
    SaveToken(String),
    ForgetToken,
    SaveWebToken(String),
    SaveVoicePreferences(VoicePreferences),
    Send(Value),
    Typing {
        dialog_id: i64,
        typing: bool,
    },
    Search,
    Stop,
    Leave(i64),
    Captcha(String),
    SolveChatCaptcha,
    VoiceConnect {
        web_mode: bool,
        proxy_url: Option<String>,
        server_host: String,
        preserve_media: bool,
    },
    FetchProxySeller(String),
    VoiceResetIdentity(bool),
    VoiceSetIdentity(String),
    VoiceImportEmulator,
    VoiceFetchCaptcha {
        url: String,
        proxy_url: Option<String>,
        server_host: String,
        web_mode: bool,
        user_agent: String,
        locale: Option<String>,
    },
    VoiceDisconnect,
    VoiceSearch {
        user_sex: String,
        peer_sex: String,
        user_age: Option<(i32, i32)>,
        peer_ages: Vec<(i32, i32)>,
        token: Option<String>,
        token_type: Option<String>,
        preferences: VoicePreferences,
    },
    VoiceStop,
    VoiceCount,
    VoiceMute(bool),
    VoiceHangup,
    VoiceTrouble,
    #[cfg(feature = "cpal-audio")]
    VoiceMicTest(bool),
    #[cfg(feature = "webrtc-media")]
    VoicePeerTimeout(String),
    VoiceEvent(Value),
    Shutdown,
}

struct Worker {
    commands: tokio_mpsc::UnboundedSender<Command>,
    notices: mpsc::Receiver<String>,
}

async fn send_chat(
    transport: Option<&ChatTransport>,
    payload: Value,
    notices: &mpsc::Sender<String>,
) -> bool {
    let Some(transport) = transport else {
        let _ = notices.send("send-error:chat is not connected".into());
        return false;
    };
    if let Err(error) = transport.send(protocol::socket_action(payload)).await {
        let _ = notices.send(format!("send-error:{error}"));
        false
    } else {
        true
    }
}

async fn send_voice(
    transport: Option<&VoiceTransportClient>,
    payload: Value,
    notices: &mpsc::Sender<String>,
) {
    let Some(transport) = transport else {
        let _ = notices.send("voice:send-error:voice transport is not connected".into());
        return;
    };
    if let Err(error) = transport.send(payload).await {
        let _ = notices.send(format!("voice:send-error:{error}"));
    }
}

struct ChatCaptchaBrowser {
    child: std::process::Child,
    profile: std::path::PathBuf,
}

impl Drop for ChatCaptchaBrowser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn chat_browser_executable() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];
    #[cfg(target_os = "windows")]
    let candidates = [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
    ];
    #[cfg(all(unix, not(target_os = "macos")))]
    let candidates = [
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/brave-browser",
    ];
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    let candidates: [&str; 0] = [];

    candidates
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
}

fn chat_token_from_cdp(value: &Value) -> Option<String> {
    value
        .pointer("/result/cookies")
        .and_then(Value::as_array)?
        .iter()
        .find(|cookie| cookie.get("name").and_then(Value::as_str) == Some("chat_token_user"))?
        .get("value")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| {
            (token.len() == 64 || token.len() == 32)
                && token.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_owned)
}

async fn solve_chat_captcha_in_browser() -> Result<String, String> {
    let executable = chat_browser_executable()
        .ok_or_else(|| "Chrome, Brave, or Edge was not found".to_owned())?;
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("could not reserve a browser debug port: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    drop(listener);

    let profile = std::env::temp_dir().join(format!(
        "nektome-chat-captcha-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&profile)
        .map_err(|error| format!("could not create the captcha browser profile: {error}"))?;
    let child = std::process::Command::new(executable)
        .arg(format!("--remote-debugging-port={port}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--new-window",
        ])
        .arg("https://nekto.me/chat/")
        .spawn()
        .map_err(|error| format!("could not start the captcha browser: {error}"))?;
    let mut browser = ChatCaptchaBrowser { child, profile };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|error| error.to_string())?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);

    let debugger_url = loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("timed out waiting for the captcha browser".into());
        }
        if browser
            .child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("the captcha browser was closed before verification".into());
        }
        let endpoint = format!("http://127.0.0.1:{port}/json");
        if let Ok(response) = client.get(&endpoint).send().await
            && let Ok(targets) = response.json::<Vec<Value>>().await
            && let Some(url) = targets.iter().find_map(|target| {
                let page_url = target.get("url").and_then(Value::as_str)?;
                page_url.contains("nekto.me/chat").then(|| {
                    target
                        .get("webSocketDebuggerUrl")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })?
            })
        {
            break url;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };

    let (mut socket, _) = tokio_tungstenite::connect_async(&debugger_url)
        .await
        .map_err(|error| format!("could not inspect the captcha browser: {error}"))?;
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"id":1,"method":"Network.enable"})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    let mut cookie_request_id = 1_u64;
    let mut captcha_submitted = false;

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("captcha verification timed out after five minutes".into());
        }
        if browser
            .child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_some()
        {
            return Err("the captcha browser was closed before verification".into());
        }
        let Ok(message) =
            tokio::time::timeout(std::time::Duration::from_millis(500), socket.next()).await
        else {
            continue;
        };
        let Some(message) = message else {
            return Err("the captcha browser inspector disconnected".into());
        };
        let message = message.map_err(|error| error.to_string())?;
        if let tokio_tungstenite::tungstenite::Message::Ping(payload) = message {
            let _ = socket
                .send(tokio_tungstenite::tungstenite::Message::Pong(payload))
                .await;
            continue;
        }
        let Some(text) = (match message {
            tokio_tungstenite::tungstenite::Message::Text(text) => Some(text),
            _ => None,
        }) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if event.get("id").and_then(Value::as_u64) == Some(cookie_request_id)
            && cookie_request_id > 1
            && let Some(token) = chat_token_from_cdp(&event)
        {
            return Ok(token);
        }
        let socket_payload = event
            .pointer("/params/response/payloadData")
            .and_then(Value::as_str);
        let frame_sent =
            event.get("method").and_then(Value::as_str) == Some("Network.webSocketFrameSent");
        let frame_received =
            event.get("method").and_then(Value::as_str) == Some("Network.webSocketFrameReceived");
        if frame_sent && socket_payload.is_some_and(|payload| payload.contains("captcha.verify")) {
            captcha_submitted = true;
        }
        if captcha_submitted
            && frame_received
            && socket_payload.is_some_and(|payload| payload.contains("captcha.verify"))
        {
            cookie_request_id += 1;
            socket
                .send(tokio_tungstenite::tungstenite::Message::Text(
                    serde_json::json!({"id":cookie_request_id,"method":"Network.getAllCookies"})
                        .to_string()
                        .into(),
                ))
                .await
                .map_err(|error| error.to_string())?;
        }
    }
}

fn chat_captcha_details(data: &Value) -> Option<(i64, i64, Option<String>)> {
    let id = data
        .get("code")
        .or_else(|| data.get("id"))
        .and_then(Value::as_i64);
    let description = data
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    let additional = data.get("additional").filter(|value| value.is_object());
    let public_key = additional
        .and_then(|value| value.get("publicKey"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if id != Some(600)
        && public_key.is_none()
        && !description.contains("captcha")
        && !description.contains("каптч")
    {
        return None;
    }
    Some((
        additional
            .and_then(|value| value.get("leftChats"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        additional
            .and_then(|value| value.get("needChats"))
            .and_then(Value::as_i64)
            .unwrap_or(0),
        public_key,
    ))
}

#[cfg(feature = "webrtc-media")]
async fn send_voice_events(
    transport: VoiceTransportClient,
    payloads: Vec<Value>,
    notices: mpsc::Sender<String>,
) {
    for payload in payloads {
        send_voice(Some(&transport), payload, &notices).await;
    }
}

#[cfg(feature = "webrtc-media")]
async fn apply_remote_candidate(
    peer: &std::sync::Arc<crate::webrtc_backend::WebRtcPeer>,
    candidate: String,
    mid: String,
    index: i32,
    notices: &mpsc::Sender<String>,
) -> bool {
    match peer
        .add_remote_candidate(candidate, Some(mid.clone()), Some(index as u16))
        .await
    {
        Ok(()) => false,
        Err(error) => {
            let _ = notices.send(format!(
                "voice:ice-candidate failed (mid={mid}, index={index}): {error}"
            ));
            true
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn flush_local_voice_candidates(
    local_sdp_sent: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    pending_local: &std::sync::Arc<std::sync::Mutex<Vec<(String, i32, String)>>>,
    transport: Option<&VoiceTransportClient>,
    connection_id: &str,
    notices: &mpsc::Sender<String>,
) {
    local_sdp_sent.store(true, Ordering::SeqCst);
    let candidates = if let Ok(mut pending) = pending_local.lock() {
        pending.drain(..).collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    if let Some(transport) = transport {
        for (mid, index, candidate) in candidates {
            let transport = transport.clone();
            let connection_id = connection_id.to_owned();
            let notices = notices.clone();
            tokio::spawn(async move {
                send_voice(
                    Some(&transport),
                    voice::ice_candidate(&connection_id, &mid, index, &candidate),
                    &notices,
                )
                .await;
            });
        }
    }
}

#[cfg(feature = "webrtc-media")]
fn event_matches_active_peer(event: &Value, active_peer: Option<&str>) -> bool {
    match event
        .get("connectionId")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(incoming) => active_peer.map(str::trim) == Some(incoming),
        None => active_peer.is_some(),
    }
}

fn captcha_url(raw: &str, server_host: Option<&str>, web_mode: bool) -> Result<String, String> {
    let endpoint = server_host
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.contains("://") {
                value
                    .replace("wss://", "https://")
                    .replace("ws://", "http://")
            } else {
                format!(
                    "https://{value}/{}",
                    if web_mode { "websocket" } else { "androiduk/" }
                )
            }
        })
        .unwrap_or_else(|| {
            std::env::var("NEKTOME_VOICE_SERVER")
                .unwrap_or_else(|_| crate::voice::SERVER.to_owned())
                .replace("wss://", "https://")
                .replace("ws://", "http://")
        });
    resolve_captcha_url(raw, &endpoint)
}

/// Normalise a captcha type received from the server into the uppercase wire
/// value the server accepts in outbound `scan-for-peer` (`tokenType`).
/// The companion Android app (v1.7.1) always sends `"IMAGE"`; the server also
/// accepts `"RECAPTCHA"`. Earlier server versions used the camelCase alias
/// `"reCaptcha"` in inbound events, which must be mapped to `"RECAPTCHA"`.
fn normalize_captcha_type(raw: &str) -> String {
    match raw.trim().to_ascii_uppercase().as_str() {
        "RECAPTCHA" | "RECAPTCHA2" | "RECAPTCHA3" => "RECAPTCHA".into(),
        "IMAGE" => "IMAGE".into(),
        _ => {
            // Fall back: any value that looks like a recaptcha alias maps to
            // RECAPTCHA; anything else is treated as an image challenge.
            if raw.to_ascii_lowercase().contains("recaptcha") {
                "RECAPTCHA".into()
            } else {
                "IMAGE".into()
            }
        }
    }
}

fn open_external_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let status = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .status();
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(url).status();
    #[cfg(all(unix, not(target_os = "macos")))]
    let status = std::process::Command::new("xdg-open").arg(url).status();
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    return Err("no supported system URL launcher".into());

    status
        .map_err(|error| format!("could not start system URL launcher: {error}"))
        .and_then(|status| {
            status
                .success()
                .then_some(())
                .ok_or_else(|| format!("system URL launcher exited with {status}"))
        })
}

fn open_external_path(path: &std::path::Path, reveal: bool) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        if reveal {
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(path);
        }
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        if reveal {
            command.arg("-R");
        }
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(if reveal {
            path.parent().unwrap_or(path)
        } else {
            path
        });
        command
    };
    #[cfg(not(any(target_os = "windows", target_os = "macos", unix)))]
    return Err("no supported system file launcher".into());

    command
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("could not start system file launcher: {error}"))
}

fn select_proxy_entry(input: &str, index: usize) -> Option<String> {
    let entries = crate::transport::proxy_entries(input);
    entries
        .get(index.min(entries.len().saturating_sub(1)))
        .cloned()
}

fn resolve_captcha_url(raw: &str, endpoint: &str) -> Result<String, String> {
    let parsed = match url::Url::parse(raw) {
        Ok(url) => url,
        Err(_) => url::Url::parse(endpoint)
            .map_err(|error| error.to_string())?
            .join(raw)
            .map_err(|error| error.to_string())?,
    };
    match parsed.scheme() {
        "http" | "https" => Ok(parsed.to_string()),
        scheme => Err(format!("captcha URL uses unsupported scheme {scheme}")),
    }
}

#[cfg(feature = "webrtc-media")]
fn queue_voice_candidate(
    pending: &mut Vec<(String, String, i32, String)>,
    connection_id: String,
    mid: String,
    index: i32,
    candidate: String,
) {
    if pending
        .iter()
        .any(|(queued_id, queued_mid, queued_index, queued_candidate)| {
            queued_id == &connection_id
                && queued_mid == &mid
                && *queued_index == index
                && queued_candidate == &candidate
        })
    {
        return;
    }
    pending.push((connection_id, mid, index, candidate));
    if pending.len() > 32 {
        pending.remove(0);
    }
}

fn parse_emulator_profile(raw: &str) -> Result<(String, Option<String>), String> {
    let profile: Value = serde_json::from_str(raw.trim())
        .map_err(|error| format!("profile JSON invalid: {error}"))?;
    let user_id = profile
        .get("user_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "user_id was not found".to_owned())?
        .to_owned();
    let connection_id = profile
        .get("connection_id_last")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    Ok((user_id, connection_id))
}

fn voice_error_details(event: &Value) -> (Option<String>, Option<String>) {
    fn text(value: Option<&Value>) -> Option<String> {
        value.and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|number| number.to_string()))
        })
    }

    let nested = event.get("error").filter(|value| value.is_object());
    let code = text(nested.and_then(|value| value.get("code"))).or_else(|| text(event.get("code")));
    let description = text(nested.and_then(|value| value.get("text")))
        .or_else(|| text(nested.and_then(|value| value.get("message"))))
        .or_else(|| text(nested.and_then(|value| value.get("description"))))
        .or_else(|| text(event.get("error")))
        .or_else(|| text(event.get("description")))
        .or_else(|| text(event.get("message")));
    (code, description)
}

impl Worker {
    fn new() -> Self {
        let (command_tx, mut command_rx) = tokio_mpsc::unbounded_channel::<Command>();
        let (notice_tx, notice_rx) = mpsc::channel::<String>();
        #[cfg(feature = "webrtc-media")]
        let timeout_command_tx = command_tx.clone();
        thread::spawn(move || {
            let runtime = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = notice_tx.send(format!("runtime error: {error}"));
                    return;
                }
            };
            #[allow(unused_assignments, unused_variables)]
            runtime.block_on(async move {
                let store = Store::new();
                let device = match store.load_or_create_device() {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = notice_tx.send(format!("storage error: {error}"));
                        return;
                    }
                };
                let key = auth::auth_key(&device.device_id);
                let auth_options = protocol::AuthOptions::from_env();
                let mut token = store.load_token().ok().flatten();
                let preferences = store.load_voice_preferences();
                let mut web_token = preferences.web_token.clone();
                let mut android_user_agent = preferences.android_user_agent.clone();
                let mut android_locale = preferences.android_locale.clone();
                let mut android_time_zone = preferences.android_time_zone.clone();
                #[cfg(feature = "webrtc-media")]
                let mut remote_audio_wav_path = preferences.remote_audio_wav_path.clone();
                #[cfg(feature = "webrtc-media")]
                let mut record_sessions = preferences.record_sessions;
                #[cfg(feature = "webrtc-media")]
                let mut recording_format = preferences.recording_format;
                #[cfg(feature = "webrtc-media")]
                let mut recording_parameters = preferences.recording_parameters;
                #[cfg(feature = "cpal-audio")]
                let mut echo_cancellation_enabled = preferences.echo_cancellation_enabled;
                #[cfg(feature = "cpal-audio")]
                let mut microphone_dsp_enabled = preferences.microphone_dsp_enabled;
                #[cfg(feature = "cpal-audio")]
                let mut microphone_warmth_enabled = preferences.microphone_warmth_enabled;
                #[cfg(feature = "cpal-audio")]
                let mut saturation_drive = preferences.saturation_drive;
                #[cfg(feature = "cpal-audio")]
                let mut saturation_asymmetry = preferences.saturation_asymmetry;
                #[cfg(feature = "cpal-audio")]
                let mut saturation_bias = preferences.saturation_bias;
                #[cfg(feature = "cpal-audio")]
                let mut presence_boost_db = preferences.presence_boost_db;
                #[cfg(feature = "cpal-audio")]
                let mut proximity_gain_db = preferences.proximity_gain_db;
                #[cfg(feature = "cpal-audio")]
                let mut air_boost_db = preferences.air_boost_db;
                #[cfg(feature = "cpal-audio")]
                let mut voice_changer_enabled = preferences.voice_changer_enabled;
                #[cfg(feature = "cpal-audio")]
                let mut voice_changer_model = preferences.voice_changer_model.clone();
                #[cfg(feature = "cpal-audio")]
                let mut voice_changer_pitch = preferences.voice_changer_pitch;
                #[cfg(feature = "cpal-audio")]
                let mut voice_changer_wet = preferences.voice_changer_wet;
                #[cfg(feature = "cpal-audio")]
                let mut microphone_noise_suppression_enabled =
                    preferences.microphone_noise_suppression_enabled;
                #[cfg(feature = "cpal-audio")]
                let mut microphone_gain_percent = preferences.microphone_gain_percent;
                #[cfg(feature = "cpal-audio")]
                let mut microphone_device_name = preferences.microphone_device_name.clone();
                #[cfg(any(feature = "cpal-audio", feature = "webrtc-media"))]
                let mut speaker_device_name = preferences.speaker_device_name.clone();
                #[cfg(feature = "cpal-audio")]
                let mut noise_gate_threshold = preferences.noise_gate_threshold;
                #[cfg(feature = "cpal-audio")]
                let mut compressor_threshold = preferences.compressor_threshold;
                #[cfg(feature = "cpal-audio")]
                let mut compressor_ratio = preferences.compressor_ratio;
                #[cfg(feature = "cpal-audio")]
                let mut makeup_gain = preferences.makeup_gain;
                #[cfg(feature = "webrtc-media")]
                let mut speaker_gain_percent = preferences.speaker_gain_percent;
                #[cfg(feature = "webrtc-media")]
                let mut speaker_dsp_enabled = preferences.speaker_dsp_enabled;
                #[cfg(all(feature = "cpal-audio", not(feature = "webrtc-media")))]
                let speaker_gain_percent = preferences.speaker_gain_percent;
                #[cfg(all(feature = "cpal-audio", not(feature = "webrtc-media")))]
                let speaker_dsp_enabled = preferences.speaker_dsp_enabled;
                if let Ok(value) = serde_json::to_string(&preferences) {
                    let _ = notice_tx.send(format!("voice-preferences:{value}"));
                }
                let mut voice_identity =
                    match store.load_or_create_voice_identity(&device.device_id) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = notice_tx.send(format!("voice identity error: {error}"));
                            return;
                        }
                    };
                if web_token.trim().is_empty()
                    && let Some(migrated_token) = voice_identity.web_token.take()
                {
                    web_token = migrated_token;
                    let mut migrated_preferences = preferences.clone();
                    migrated_preferences.web_token = web_token.clone();
                    let _ = store.save_voice_preferences(&migrated_preferences);
                    let _ = notice_tx.send(format!("voice:web-token:{web_token}"));
                    let _ = store.save_voice_identity(&voice_identity);
                }
                let _ = notice_tx.send(format!("voice:android-id:{}", voice_identity.user_id));
                let mut transport: Option<ChatTransport> = None;
                let mut chat_notice_bridge: Option<tokio::task::JoinHandle<()>> = None;
                let mut voice_transport: Option<VoiceTransportClient> = None;
                let mut voice_notice_bridge: Option<tokio::task::JoinHandle<()>> = None;
                let mut voice_probe_task: Option<tokio::task::JoinHandle<()>> = None;
                #[cfg(feature = "webrtc-media")]
                let mut voice_proxy: Option<String> = None;
                #[cfg(feature = "webrtc-media")]
                let mut pending_voice_candidates: Vec<(
                    String,
                    String,
                    i32,
                    String,
                )> = Vec::new();
                #[cfg(feature = "webrtc-media")]
                let mut voice_candidate_history: HashMap<
                    String,
                    Vec<(String, String, i32, String)>,
                > = HashMap::new();
                #[cfg(feature = "webrtc-media")]
                let mut voice_peer: Option<
                    std::sync::Arc<crate::webrtc_backend::WebRtcPeer>,
                > = None;
                #[cfg(feature = "webrtc-media")]
                let voice_media_ready =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                #[cfg(feature = "webrtc-media")]
                let voice_local_muted =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                #[cfg(feature = "webrtc-media")]
                let voice_peer_generation = std::sync::Arc::new(AtomicU64::new(0));
                #[cfg(feature = "webrtc-media")]
                let voice_local_sdp_sent =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                #[cfg(feature = "webrtc-media")]
                let voice_pending_local_candidates =
                    std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, i32, String)>::new()));
                #[cfg(feature = "cpal-audio")]
                let mut voice_microphone: Option<crate::audio::CpalMicrophone> = None;
                #[cfg(feature = "cpal-audio")]
                let mut voice_test_sink: Option<std::sync::Arc<dyn crate::audio::AudioSink>> = None;
                #[cfg(feature = "cpal-audio")]
                let mut voice_test_processor: Option<
                    std::sync::Arc<std::sync::Mutex<crate::audio::MicrophoneProcessor>>,
                > = None;
                while let Some(command) = command_rx.recv().await {
                    match command {
                        Command::Connect => {
                            if let Some(bridge) = chat_notice_bridge.take() {
                                bridge.abort();
                            }
                            if let Some(client) = &transport {
                                client.shutdown();
                            }
                            transport = None;
                            let (async_tx, mut async_rx) = tokio_mpsc::channel::<String>(64);
                            match ChatTransport::connect(async_tx).await {
                                Ok(client) => {
                                    let payload = token.as_deref().map_or_else(
                                        || {
                                            protocol::auth_get_token_with_options(
                                                &device.device_id,
                                                &device.device_name,
                                                &key,
                                                &auth_options,
                                            )
                                        },
                                        |value| protocol::auth_send_token_with_options(value, &key, &auth_options),
                                    );
                                    transport = Some(client);
                                    if !send_chat(transport.as_ref(), payload, &notice_tx).await {
                                        if let Some(client) = transport.take() {
                                            client.shutdown();
                                        }
                                        continue;
                                    }
                                    let bridge = notice_tx.clone();
                                    chat_notice_bridge = Some(tokio::spawn(async move {
                                        while let Some(notice) = async_rx.recv().await {
                                            let _ = bridge.send(notice);
                                        }
                                    }));
                                    let _ = notice_tx.send("connecting".into());
                                }
                                Err(error) => {
                                    let _ = notice_tx.send(format!("connect failed: {error}"));
                                }
                            }
                        }
                        Command::SaveToken(value) => {
                            if store.save_token(&value).is_ok() {
                                token = Some(value);
                            }
                        }
                        Command::ForgetToken => {
                            token = None;
                            if let Err(error) = store.clear_token() {
                                let _ = notice_tx.send(format!("token-clear-error:{error}"));
                            } else {
                                let _ = notice_tx.send("token-cleared".into());
                            }
                        }
                        Command::SaveWebToken(value) => {
                            web_token = value.trim().to_owned();
                            let mut updated = store.load_voice_preferences();
                            updated.web_token = web_token.clone();
                            let _ = store.save_voice_preferences(&updated);
                        }
                        Command::FetchProxySeller(api_key) => {
                            let preferences = store.load_voice_preferences();
                            let options = crate::proxy_seller::ProxySellerOptions {
                                list_name: preferences.proxy_seller_list_name,
                                ports: preferences.proxy_seller_ports_count,
                                rotation_seconds: preferences.proxy_seller_rotation_seconds,
                                whitelist_ip: preferences.proxy_seller_whitelist_ip,
                            };
                            match crate::proxy_seller::ProxySellerClient::with_options(
                                api_key, options,
                            ) {
                                Ok(client) => match client.fetch_resident_proxies().await {
                                    Ok(result) => {
                                        if let Ok(value) = serde_json::to_string(&result) {
                                            let _ = notice_tx
                                                .send(format!("proxy-seller:ok:{value}"));
                                        }
                                    }
                                    Err(error) => {
                                        let _ = notice_tx
                                            .send(format!("proxy-seller:error:{error}"));
                                    }
                                },
                                Err(error) => {
                                    let _ = notice_tx
                                        .send(format!("proxy-seller:error:{error}"));
                                }
                            }
                        }
                        Command::VoiceFetchCaptcha {
                            url,
                            proxy_url,
                            server_host,
                            web_mode,
                            user_agent,
                            locale,
                        } => {
                            let mut builder = reqwest::Client::builder()
                                .timeout(std::time::Duration::from_secs(15));
                            if let Some(proxy_url) = proxy_url.as_deref()
                                && let Ok(proxy) = reqwest::Proxy::all(
                                    crate::transport::normalize_proxy_url(proxy_url),
                                )
                            {
                                builder = builder.proxy(proxy);
                            }
                            let url = match captcha_url(
                                &url,
                                (!server_host.trim().is_empty()).then_some(server_host.as_str()),
                                web_mode,
                            ) {
                                Ok(url) => url,
                                Err(error) => {
                                    let _ = notice_tx
                                        .send(format!("voice:captcha-image-error:{error}"));
                                    continue;
                                }
                            };
                            let android_language =
                                locale.unwrap_or_else(protocol::local_locale);
                            let request = match builder.build() {
                                Ok(client) => {
                                    let mut request = client.get(&url);
                                    if web_mode {
                                        request = request
                                            .header("Origin", "https://nekto.me")
                                            .header("Referer", "https://nekto.me/audiochat")
                                            .header("User-Agent", &user_agent);
                                    } else {
                                        request = request
                                            .header("NektoMe-Chat-Version", "2")
                                            .header("App-Android-Version", "1.7.1")
                                            .header("App-Android-Code", "96")
                                            .header(
                                                "Android-Language",
                                                &android_language,
                                            )
                                            .header("User-Agent", &user_agent);
                                    }
                                    request.send().await
                                }
                                Err(error) => {
                                    let _ = notice_tx.send(format!(
                                        "voice:captcha-image-error:{error}"
                                    ));
                                    continue;
                                }
                            };
                            match request {
                                Ok(response) => match response.error_for_status() {
                                    Ok(response) => match response.bytes().await {
                                        Ok(bytes) if bytes.is_empty() => {
                                            let _ = notice_tx.send(
                                                "voice:captcha-image-error:server returned an empty captcha image"
                                                    .into(),
                                            );
                                        }
                                        Ok(bytes) => {
                                        let encoded =
                                            base64::engine::general_purpose::STANDARD.encode(bytes);
                                        let _ =
                                            notice_tx.send(format!("voice:captcha-image:{encoded}"));
                                        }
                                        Err(error) => {
                                            let _ = notice_tx.send(format!(
                                                "voice:captcha-image-error:{error}"
                                            ));
                                        }
                                    },
                                    Err(error) => {
                                        let _ = notice_tx.send(format!(
                                            "voice:captcha-image-error:{error}"
                                        ));
                                    }
                                },
                                Err(error) => {
                                    let _ = notice_tx
                                        .send(format!("voice:captcha-image-error:{error}"));
                                }
                            }
                        }
                        Command::SaveVoicePreferences(updated) => {
                            web_token = updated.web_token.clone();
                            android_user_agent = updated.android_user_agent.clone();
                            android_locale = updated.android_locale.clone();
                            android_time_zone = updated.android_time_zone.clone();
                            #[cfg(feature = "webrtc-media")]
                            {
                                remote_audio_wav_path = updated.remote_audio_wav_path.clone();
                                record_sessions = updated.record_sessions;
                                recording_format = updated.recording_format;
                                recording_parameters = updated.recording_parameters;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                speaker_gain_percent = updated.speaker_gain_percent;
                                speaker_dsp_enabled = updated.speaker_dsp_enabled;
                            }
                            #[cfg(any(feature = "cpal-audio", feature = "webrtc-media"))]
                            {
                                speaker_device_name = updated.speaker_device_name.clone();
                            }
                            #[cfg(feature = "cpal-audio")]
                            {
                                microphone_device_name = updated.microphone_device_name.clone();
                                microphone_dsp_enabled = updated.microphone_dsp_enabled;
                                microphone_warmth_enabled = updated.microphone_warmth_enabled;
                                saturation_drive = updated.saturation_drive;
                                saturation_asymmetry = updated.saturation_asymmetry;
                                saturation_bias = updated.saturation_bias;
                                presence_boost_db = updated.presence_boost_db;
                                proximity_gain_db = updated.proximity_gain_db;
                                air_boost_db = updated.air_boost_db;
                                voice_changer_enabled = updated.voice_changer_enabled;
                                voice_changer_model.clone_from(&updated.voice_changer_model);
                                voice_changer_pitch = updated.voice_changer_pitch;
                                voice_changer_wet = updated.voice_changer_wet;
                                microphone_noise_suppression_enabled =
                                    updated.microphone_noise_suppression_enabled;
                                microphone_gain_percent = updated.microphone_gain_percent;
                                noise_gate_threshold = updated.noise_gate_threshold;
                                compressor_threshold = updated.compressor_threshold;
                                compressor_ratio = updated.compressor_ratio;
                                makeup_gain = updated.makeup_gain;
                                echo_cancellation_enabled = updated.echo_cancellation_enabled;
                                if let Some(proc) = voice_test_processor.as_ref() {
                                    if let Ok(mut p) = proc.lock() {
                                        p.gain_percent = updated.microphone_gain_percent;
                                        p.configure_dynamics(
                                            updated.noise_gate_threshold,
                                            updated.compressor_threshold,
                                            updated.compressor_ratio,
                                            updated.makeup_gain,
                                        );
                                        p.configure_warmth(
                                            updated.microphone_warmth_enabled,
                                            updated.saturation_drive,
                                            updated.saturation_asymmetry,
                                            updated.saturation_bias,
                                        );
                                        p.configure_presence(updated.presence_boost_db);
                                        p.configure_proximity(updated.proximity_gain_db);
                                        p.configure_air(updated.air_boost_db);
                                        p.configure_voice_changer(
                                            updated.voice_changer_enabled,
                                            &updated.voice_changer_model,
                                            updated.voice_changer_pitch,
                                            updated.voice_changer_wet,
                                        );
                                        p.dsp_enabled = updated.microphone_dsp_enabled;
                                        p.noise_suppression_enabled =
                                            updated.microphone_noise_suppression_enabled;
                                    }
                                }
                            }
                            let _ = store.save_voice_preferences(&updated);
                        }
                        Command::Send(payload) => {
                            send_chat(transport.as_ref(), payload, &notice_tx).await;
                        }
                        Command::Typing { dialog_id, typing } => {
                            send_chat(
                                transport.as_ref(),
                                protocol::dialog_typing(dialog_id, typing, false),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::Search => {
                            let criteria = protocol::SearchCriteria {
                                my_sex: None,
                                wish_sex: None,
                                my_age: None,
                                wish_age: None,
                                include_adult: false,
                                role_play: false,
                                disable_voice: false,
                            };
                            send_chat(
                                transport.as_ref(),
                                protocol::search_run(&criteria),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::Stop => {
                            send_chat(
                                transport.as_ref(),
                                protocol::action("search.sendOut"),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::Leave(dialog_id) => {
                            send_chat(
                                transport.as_ref(),
                                protocol::leave_dialog(dialog_id),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::Captcha(solution) => {
                            send_chat(
                                transport.as_ref(),
                                protocol::captcha_verify(&solution),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::SolveChatCaptcha => {
                            let notices = notice_tx.clone();
                            tokio::spawn(async move {
                                let notice = match solve_chat_captcha_in_browser().await {
                                    Ok(token) => format!("chat-captcha-token:{token}"),
                                    Err(error) => format!("chat-captcha-browser-error:{error}"),
                                };
                                let _ = notices.send(notice);
                            });
                        }
                        Command::VoiceConnect {
                            web_mode,
                            proxy_url,
                            server_host,
                            preserve_media,
                        } => {
                            if let Some(task) = voice_probe_task.take() {
                                task.abort();
                            }
                            let _ = preserve_media;
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            #[cfg(feature = "webrtc-media")]
                            if !preserve_media {
                                voice_local_muted.store(false, Ordering::Relaxed);
                            }
                            if web_mode && web_token.trim().is_empty() {
                                web_token = crate::persistence::new_web_token();
                                let mut preferences = store.load_voice_preferences();
                                preferences.web_token = web_token.clone();
                                let _ = store.save_voice_preferences(&preferences);
                                let _ = notice_tx.send(format!("voice:web-token:{web_token}"));
                            }
                            if let Some(client) = &voice_transport {
                                client.shutdown();
                            }
                            voice_transport = None;
                            #[cfg(feature = "cpal-audio")]
                            if !preserve_media {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            #[cfg(feature = "webrtc-media")]
                            if !preserve_media {
                                voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            }
                            #[cfg(feature = "webrtc-media")]
                            if !preserve_media && let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            let (async_tx, mut async_rx) = tokio_mpsc::channel::<String>(64);
                            let proxy = proxy_url
                                .as_deref()
                                .filter(|value| !value.trim().is_empty());
                            #[cfg(feature = "webrtc-media")]
                            {
                                voice_proxy = proxy.map(str::to_owned);
                            }
                            let user_agent = (!android_user_agent.trim().is_empty())
                                .then_some(android_user_agent.as_str());
                            match VoiceTransportClient::connect(
                                web_mode,
                                async_tx,
                                proxy,
                                user_agent,
                                (!web_mode && !android_locale.trim().is_empty())
                                    .then_some(android_locale.as_str()),
                                (!server_host.trim().is_empty()).then_some(server_host.as_str()),
                            )
                            .await
                            {
                                Ok(client) => {
                                    #[cfg(feature = "webrtc-media")]
                                    {
                                        voice_proxy = client.active_proxy().map(str::to_owned);
                                    }
                                    if let Some(active_proxy) = client.active_proxy() {
                                        let _ = notice_tx.send(format!(
                                            "voice:active-proxy:{active_proxy}"
                                        ));
                                    }
                                    let locale = if !client.is_web()
                                        && !android_locale.trim().is_empty()
                                    {
                                        android_locale.clone()
                                    } else {
                                        protocol::local_locale()
                                    };
                                    let time_zone = if !client.is_web()
                                        && !android_time_zone.trim().is_empty()
                                    {
                                        android_time_zone.clone()
                                    } else {
                                        protocol::local_time_zone()
                                    };
                                    let registration = if client.is_web() {
                                        voice::register_web(
                                            &web_token,
                                            &locale,
                                            &time_zone,
                                        )
                                    } else {
                                        voice::register(
                                            &voice_identity.user_id,
                                            voice_identity.last_connection_id.as_deref(),
                                            voice_identity.resume_peer_id.as_deref(),
                                            &locale,
                                            &time_zone,
                                        )
                                    };
                                    match client.send(registration).await {
                                        Ok(()) => {
                                            voice_transport = Some(client);
                                            let bridge = notice_tx.clone();
                                            voice_notice_bridge = Some(tokio::spawn(async move {
                                                while let Some(notice) = async_rx.recv().await {
                                                    let _ = bridge.send(notice);
                                                }
                                            }));
                                            let _ = notice_tx.send("voice:connecting".into());
                                        }
                                        Err(error) => {
                                            client.shutdown();
                                            let _ = notice_tx.send(format!(
                                                "voice:register failed: {error}"
                                            ));
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ =
                                        notice_tx.send(format!("voice:connect failed: {error}"));
                                }
                            }
                        }
                        Command::VoiceDisconnect => {
                            if let Some(task) = voice_probe_task.take() {
                                task.abort();
                            }
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_candidate_history.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            if let Some(client) = &voice_transport {
                                client.shutdown();
                            }
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            voice_transport = None;
                            let _ = notice_tx.send("voice:disconnected".into());
                        }
                        Command::VoiceResetIdentity(web_mode) => {
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            if let Some(client) = &voice_transport {
                                client.shutdown();
                            }
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            voice_transport = None;
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            if !web_mode {
                                let identity = store.reset_voice_identity();
                                if let Ok(identity) = identity {
                                    voice_identity = identity;
                                    let _ = notice_tx.send(format!(
                                        "voice:android-id:{}",
                                        voice_identity.user_id
                                    ));
                                }
                            } else {
                                web_token = crate::persistence::new_web_token();
                                let mut preferences = store.load_voice_preferences();
                                preferences.web_token = web_token.clone();
                                let _ = store.save_voice_preferences(&preferences);
                                let _ = notice_tx.send(format!("voice:web-token:{web_token}"));
                            }
                            let _ = notice_tx.send("voice:identity-reset".into());
                        }
                        Command::VoiceSetIdentity(user_id) => {
                            let user_id = crate::persistence::normalize_voice_user_id(&user_id);
                            if user_id.is_empty() {
                                let _ = notice_tx
                                    .send("voice:identity-error:Android ID cannot be empty".into());
                                continue;
                            }
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            if let Some(client) = &voice_transport {
                                client.shutdown();
                            }
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            voice_transport = None;
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            voice_identity.user_id = user_id.clone();
                            voice_identity.last_connection_id = None;
                            voice_identity.resume_peer_id = None;
                            let _ = store.save_voice_identity(&voice_identity);
                            let _ = notice_tx.send(format!("voice:android-id:{user_id}"));
                            let _ = notice_tx.send("voice:identity-reset".into());
                        }
                        Command::VoiceImportEmulator => {
                            const DATABASE: &str =
                                "/data/data/com.nektome.chatruletka.voice/databases/preferences";
                            const QUERY: &str =
                                "SELECT preferences FROM preferences WHERE id=1;";
                            let result = tokio::process::Command::new("adb")
                                .args(["shell", "su", "0", "sqlite3", DATABASE, QUERY])
                                .output()
                                .await;
                            let output = match result {
                                Ok(output) if output.status.success() => output,
                                Ok(output) => {
                                    let error = String::from_utf8_lossy(&output.stderr);
                                    let _ = notice_tx.send(format!(
                                        "voice:emulator-import-error:{}",
                                        if error.trim().is_empty() {
                                            "adb command failed"
                                        } else {
                                            error.trim()
                                        }
                                    ));
                                    continue;
                                }
                                Err(error) => {
                                    let _ = notice_tx.send(format!(
                                        "voice:emulator-import-error:{error}"
                                    ));
                                    continue;
                                }
                            };
                            let raw = String::from_utf8_lossy(&output.stdout);
                            let (user_id, last_connection_id) = match parse_emulator_profile(&raw) {
                                Ok(profile) => profile,
                                Err(error) => {
                                    let _ = notice_tx
                                        .send(format!("voice:emulator-import-error:{error}"));
                                    continue;
                                }
                            };
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            if let Some(client) = voice_transport.take() {
                                client.shutdown();
                            }
                            voice_identity.user_id = user_id;
                            voice_identity.last_connection_id = last_connection_id;
                            voice_identity.resume_peer_id = None;
                            let _ = store.save_voice_identity(&voice_identity);
                            let emulator_agent = "NektoMeAudio96/2.1.0 (Linux; U; Android 10; Android SDK built for x86_64 Build/QSR1.210802.001)";
                            android_user_agent = emulator_agent.into();
                            let mut preferences = store.load_voice_preferences();
                            preferences.android_user_agent = emulator_agent.into();
                            let _ = store.save_voice_preferences(&preferences);
                            let _ = notice_tx.send(format!(
                                "voice:android-id:{}",
                                voice_identity.user_id
                            ));
                            let _ = notice_tx.send(format!(
                                "voice:android-ua:{emulator_agent}"
                            ));
                            let _ = notice_tx.send(format!(
                                "voice:emulator-imported:{}",
                                voice_identity.last_connection_id.as_deref().unwrap_or("none")
                            ));
                        }
                        Command::VoiceSearch {
                            user_sex,
                            peer_sex,
                            user_age,
                            peer_ages,
                            token,
                            token_type,
                            preferences,
                        } => {
                            let Some(transport) = voice_transport.as_ref() else {
                                let _ = notice_tx.send(
                                    "voice:send-error:voice transport is not connected".into(),
                                );
                                let _ = notice_tx.send("voice:search.stop".into());
                                continue;
                            };
                            #[cfg(feature = "cpal-audio")]
                            {
                                microphone_dsp_enabled = preferences.microphone_dsp_enabled;
                                microphone_noise_suppression_enabled =
                                    preferences.microphone_noise_suppression_enabled;
                                microphone_gain_percent = preferences.microphone_gain_percent;
                                microphone_warmth_enabled = preferences.microphone_warmth_enabled;
                                saturation_drive = preferences.saturation_drive;
                                saturation_asymmetry = preferences.saturation_asymmetry;
                                saturation_bias = preferences.saturation_bias;
                                presence_boost_db = preferences.presence_boost_db;
                                proximity_gain_db = preferences.proximity_gain_db;
                                air_boost_db = preferences.air_boost_db;
                                voice_changer_enabled = preferences.voice_changer_enabled;
                                voice_changer_model.clone_from(&preferences.voice_changer_model);
                                voice_changer_pitch = preferences.voice_changer_pitch;
                                voice_changer_wet = preferences.voice_changer_wet;
                                noise_gate_threshold = preferences.noise_gate_threshold;
                                compressor_threshold = preferences.compressor_threshold;
                                compressor_ratio = preferences.compressor_ratio;
                                makeup_gain = preferences.makeup_gain;
                                echo_cancellation_enabled = preferences.echo_cancellation_enabled;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                speaker_gain_percent = preferences.speaker_gain_percent;
                                speaker_dsp_enabled = preferences.speaker_dsp_enabled;
                                remote_audio_wav_path = preferences.remote_audio_wav_path.clone();
                                record_sessions = preferences.record_sessions;
                                recording_format = preferences.recording_format;
                            }
                            let _ = store.save_voice_preferences(&preferences);
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_candidate_history.clear();
                            }
                            send_voice(
                                Some(transport),
                                voice::scan_for_peer_with_token_type(
                                    &user_sex,
                                    &peer_sex,
                                    user_age,
                                    &peer_ages,
                                    token.as_deref(),
                                    token_type.as_deref(),
                                ),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::VoiceStop => {
                            if let Some(transport) = voice_transport.as_ref() {
                                #[cfg(feature = "webrtc-media")]
                                {
                                    pending_voice_candidates.clear();
                                    voice_candidate_history.clear();
                                }
                                send_voice(
                                    Some(transport),
                                    voice::type_only("stop-scan"),
                                    &notice_tx,
                                )
                                .await;
                            }
                        }
                        Command::VoiceCount if voice_transport.is_some() => {
                            send_voice(
                                voice_transport.as_ref(),
                                voice::users_count_request(),
                                &notice_tx,
                            )
                            .await;
                        }
                        Command::VoiceMute(muted) if voice_transport.is_some() => {
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(muted, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.as_ref() {
                                peer.set_muted(muted);
                            }
                            if let Some(connection_id) = voice_identity.resume_peer_id.as_deref() {
                                send_voice(
                                    voice_transport.as_ref(),
                                    voice::peer_mute(connection_id, muted),
                                    &notice_tx,
                                )
                                .await;
                            }
                        }
                        Command::VoiceHangup if voice_transport.is_some() => {
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            let connection_id = voice_identity.resume_peer_id.clone();
                            if let Some(connection_id) = connection_id.as_deref() {
                                send_voice(
                                    voice_transport.as_ref(),
                                    voice::connection_event("peer-disconnect", connection_id),
                                    &notice_tx,
                                )
                                .await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            if let Some(connection_id) = connection_id.as_deref() {
                                send_voice(
                                    voice_transport.as_ref(),
                                    voice::peer_connection(false, Some(connection_id)),
                                    &notice_tx,
                                )
                                .await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_candidate_history.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            voice_identity.resume_peer_id = None;
                            let _ = store.save_voice_identity(&voice_identity);
                        }
                        Command::VoiceTrouble if voice_transport.is_some() => {
                            if let Some(connection_id) = voice_identity.resume_peer_id.as_deref() {
                                send_voice(
                                    voice_transport.as_ref(),
                                    voice::connection_event("peer-trouble", connection_id),
                                    &notice_tx,
                                )
                                .await;
                            }
                        }
                        #[cfg(feature = "cpal-audio")]
                        Command::VoiceMicTest(enabled) => {
                            if enabled {
                                #[cfg(feature = "webrtc-media")]
                                if voice_peer.is_some() {
                                    let _ = notice_tx.send(
                                        "voice:mic-test:failed:stop the active call first".into(),
                                    );
                                    continue;
                                }
                                let sink = match crate::audio::default_sink(
                                    speaker_gain_percent,
                                    false,
                                    1.0,
                                    None,
                                    Some(&speaker_device_name),
                                ) {
                                    Ok(sink) => sink,
                                    Err(error) => {
                                        let _ = notice_tx
                                            .send(format!("voice:mic-test:failed:{error}"));
                                        continue;
                                    }
                                };
                                let mut processor = crate::audio::MicrophoneProcessor::new(48_000);
                                processor.gain_percent = microphone_gain_percent;
                                processor.configure_dynamics(
                                    noise_gate_threshold,
                                    compressor_threshold,
                                    compressor_ratio,
                                    makeup_gain,
                                );
                                processor.configure_warmth(
                                    microphone_warmth_enabled,
                                    saturation_drive,
                                    saturation_asymmetry,
                                    saturation_bias,
                                );
                                processor.configure_presence(presence_boost_db);
                                processor.configure_proximity(proximity_gain_db);
                                processor.configure_air(air_boost_db);
                                processor.configure_voice_changer(
                                    voice_changer_enabled,
                                    &voice_changer_model,
                                    voice_changer_pitch,
                                    voice_changer_wet,
                                );
                                processor.dsp_enabled = microphone_dsp_enabled;
                                processor.noise_suppression_enabled =
                                    microphone_noise_suppression_enabled;
                                processor.echo_cancellation_enabled = false;
                                let shared_processor =
                                    std::sync::Arc::new(std::sync::Mutex::new(processor));
                                voice_test_processor =
                                    Some(std::sync::Arc::clone(&shared_processor));

                                let callback_sink = std::sync::Arc::clone(&sink);
                                let callback_processor = std::sync::Arc::clone(&shared_processor);
                                let mut microphone = crate::audio::CpalMicrophone::new_with_device(
                                    Some(microphone_device_name.clone()),
                                    move |pcm, sample_rate| {
                                        if let Ok(mut proc) = callback_processor.lock() {
                                            proc.process(pcm, sample_rate);
                                        }
                                        crate::audio::record_microphone_level(pcm);
                                        callback_sink.write(pcm, sample_rate);
                                    },
                                );
                                if let Err(error) =
                                    crate::audio::MicrophoneCapture::start(&mut microphone)
                                {
                                    let _ = notice_tx
                                        .send(format!("voice:mic-test:failed:{error}"));
                                    continue;
                                }
                                voice_test_sink = Some(sink);
                                voice_microphone = Some(microphone);
                                let _ = notice_tx.send("voice:mic-test:started".into());
                            } else {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                                let _ = notice_tx.send("voice:mic-test:stopped".into());
                            }
                        }
                        #[cfg(feature = "webrtc-media")]
                        Command::VoicePeerTimeout(connection_id) => {
                            if voice_identity.resume_peer_id.as_deref() != Some(&connection_id) {
                                continue;
                            }
                            voice_local_muted.store(false, Ordering::Relaxed);
                            voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            pending_voice_candidates.clear();
                            voice_media_ready.store(false, std::sync::atomic::Ordering::Relaxed);
                            voice_identity.resume_peer_id = None;
                            let _ = store.save_voice_identity(&voice_identity);
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            if let Some(client) = &voice_transport {
                                send_voice(
                                    Some(client),
                                    voice::connection_event("peer-disconnect", &connection_id),
                                    &notice_tx,
                                )
                                .await;
                                send_voice(
                                    Some(client),
                                    voice::peer_connection(false, Some(&connection_id)),
                                    &notice_tx,
                                )
                                .await;
                            }
                            let _ = notice_tx.send("voice:peer-timeout".into());
                        }
                        Command::VoiceEvent(event) => {
                            let event_type = event.get("type").and_then(Value::as_str);
                            let active_peer_matches = match event
                                .get("connectionId")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                            {
                                Some(incoming) => voice_identity.resume_peer_id.as_deref()
                                    == Some(incoming),
                                None => voice_identity.resume_peer_id.is_some(),
                            };
                            #[cfg(feature = "webrtc-media")]
                            let event_matches_current_peer =
                                event_matches_active_peer(&event, voice_identity.resume_peer_id.as_deref());
                            if matches!(event_type, Some("peer-disconnect" | "peer-soft-disconnect"))
                                && active_peer_matches
                            {
                                let soft_disconnect = event_type == Some("peer-soft-disconnect");
                                #[cfg(feature = "webrtc-media")]
                                {
                                    voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                                    let media_was_ready = voice_media_ready.swap(
                                        false,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    if let Some(peer) = voice_peer.take() {
                                        let _ = peer.close().await;
                                    }
                                    pending_voice_candidates.clear();
                                    if media_was_ready
                                        && let Some(client) = &voice_transport
                                    {
                                        send_voice(
                                            Some(client),
                                            voice::peer_connection(
                                                false,
                                                voice_identity.resume_peer_id.as_deref(),
                                            ),
                                            &notice_tx,
                                        )
                                        .await;
                                    }
                                }
                                #[cfg(feature = "cpal-audio")]
                                {
                                    voice_microphone = None;
                                    voice_test_sink = None;
                                voice_test_processor = None;
                                }
                                if !soft_disconnect {
                                    voice_identity.resume_peer_id = None;
                                }
                                let _ = store.save_voice_identity(&voice_identity);
                            }
                            if event_type == Some("ban") {
                                #[cfg(feature = "webrtc-media")]
                                {
                                    voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                                    pending_voice_candidates.clear();
                                    if let Some(peer) = voice_peer.take() {
                                        let _ = peer.close().await;
                                    }
                                }
                                #[cfg(feature = "cpal-audio")]
                                {
                                    voice_microphone = None;
                                    voice_test_sink = None;
                                voice_test_processor = None;
                                }
                                if let Some(client) = &voice_transport {
                                    client.shutdown();
                                }
                                if let Some(bridge) = voice_notice_bridge.take() {
                                    bridge.abort();
                                }
                                voice_transport = None;
                                if let Ok(identity) = store.reset_voice_identity() {
                                    voice_identity = identity;
                                    let _ = notice_tx.send("voice:identity-reset".into());
                                }
                            }
                            if event.get("type").and_then(Value::as_str) == Some("registered") {
                                let registration_succeeded = event
                                    .get("success")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                if let Some(connection_id) = event
                                    .get("connectionId")
                                    .and_then(Value::as_str)
                                    .map(str::trim)
                                    .filter(|value| !value.is_empty())
                                {
                                    voice_identity.last_connection_id =
                                        Some(connection_id.to_owned());
                                }
                                if registration_succeeded {
                                    // `peerSuccess` is a one-shot resume hint. The
                                    // active media peer keeps it while signaling
                                    // reconnects; an idle connection clears it so
                                    // a later search cannot resume an old peer.
                                    #[cfg(feature = "webrtc-media")]
                                    if voice_peer.is_none() {
                                        voice_identity.resume_peer_id = None;
                                    }
                                    #[cfg(not(feature = "webrtc-media"))]
                                    {
                                        voice_identity.resume_peer_id = None;
                                    }
                                }
                                let _ = store.save_voice_identity(&voice_identity);
                                if !registration_succeeded {
                                    #[cfg(feature = "webrtc-media")]
                                    {
                                    voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                                    if let Some(peer) = voice_peer.take() {
                                        let _ = peer.close().await;
                                    }
                                    pending_voice_candidates.clear();
                                    voice_media_ready.store(
                                        false,
                                        std::sync::atomic::Ordering::Relaxed,
                                    );
                                    }
                                    #[cfg(feature = "cpal-audio")]
                                    {
                                        voice_microphone = None;
                                        voice_test_sink = None;
                                voice_test_processor = None;
                                    }
                                    if let Some(bridge) = voice_notice_bridge.take() {
                                        bridge.abort();
                                    }
                                    if let Some(client) = voice_transport.take() {
                                        client.shutdown();
                                    }
                                    continue;
                                }
                                let Some(client) = voice_transport.as_ref() else {
                                    continue;
                                };
                                if client.is_web() {
                                    let user_agent =
                                        VoiceTransportClient::DEFAULT_WEB_USER_AGENT;
                                    let token = if web_token.is_empty() {
                                        &voice_identity.user_id
                                    } else {
                                        &web_token
                                    };
                                    let locale = protocol::local_locale();
                                    let time_zone = protocol::local_time_zone();
                                    let fpt_token = voice::web_registration_fpt_token(&event, token);
                                    send_voice(
                                        Some(client),
                                        voice::web_set_fpt(user_agent, &fpt_token, &locale),
                                        &notice_tx,
                                    )
                                    .await;
                                    if let Some(internal_id) = event.get("internal_id").and_then(Value::as_i64) {
                                        send_voice(
                                            Some(client),
                                            voice::web_agent(token, internal_id),
                                            &notice_tx,
                                        )
                                        .await;
                                        send_voice(
                                            Some(client),
                                            voice::web_fpt(token, internal_id, user_agent, &time_zone),
                                            &notice_tx,
                                        )
                                        .await;
                                    }
                                }
                                // Establish the browser fingerprint before making
                                // secondary requests. The legacy Web transport
                                // sends set-fpt immediately after registration.
                                #[cfg(feature = "webrtc-media")]
                                if voice_peer.is_some()
                                    && voice_media_ready.load(Ordering::Relaxed)
                                    && let Some(connection_id) =
                                        voice_identity.resume_peer_id.as_deref()
                                {
                                    for payload in voice::media_ready_events(
                                        connection_id,
                                        voice_local_muted.load(Ordering::Relaxed),
                                    ) {
                                        send_voice(Some(client), payload, &notice_tx).await;
                                    }
                                }
                                send_voice(
                                    Some(client),
                                    voice::users_count_request(),
                                    &notice_tx,
                                )
                                .await;
                            }
                            if let Some(voice_transport) = &voice_transport {
                                if event.get("type").and_then(Value::as_str)
                                    == Some("ping-server-request")
                                {
                                    if let Some(echo) = event.get("echo") {
                                        send_voice(
                                            Some(voice_transport),
                                            voice::ping_server_response(Some(echo.clone())),
                                            &notice_tx,
                                        )
                                        .await;
                                    } else {
                                        let servers = event
                                            .get("list")
                                            .and_then(Value::as_array)
                                            .map(|values| {
                                                values
                                                    .iter()
                                                    .filter_map(Value::as_str)
                                                    .map(str::to_owned)
                                                    .collect::<Vec<_>>()
                                            })
                                            .unwrap_or_default();
                                        let attempts = event
                                            .get("attempts")
                                            .and_then(Value::as_i64)
                                            .unwrap_or(1)
                                            as i32;
                                        if let Some(task) = voice_probe_task.take() {
                                            task.abort();
                                        }
                                        let probe_transport = voice_transport.clone();
                                        let probe_notices = notice_tx.clone();
                                        voice_probe_task = Some(tokio::spawn(async move {
                                            let results =
                                                voice::probe_servers(&servers, attempts).await;
                                            send_voice(
                                                Some(&probe_transport),
                                                voice::log_ping_results(results),
                                                &probe_notices,
                                            )
                                            .await;
                                        }));
                                    }
                                }
                                match event.get("type").and_then(Value::as_str) {
                                    Some("challenge")
                                    | Some("challenge-request")
                                    | Some("challenge-proof") => {
                                        send_voice(
                                            Some(voice_transport),
                                            voice::challenge_proof(&event),
                                            &notice_tx,
                                        )
                                        .await;
                                    }
                                    Some("challenge-sync") => {
                                        send_voice(
                                            Some(voice_transport),
                                            voice::challenge_ack(&event),
                                            &notice_tx,
                                        )
                                        .await;
                                    }
                                    Some("challenge-trace") => {
                                        send_voice(
                                            Some(voice_transport),
                                            voice::challenge_trace(&event),
                                            &notice_tx,
                                        )
                                        .await;
                                    }
                                    _ => {}
                                }
                            }
                            #[cfg(feature = "webrtc-media")]
                            if let Some(voice_transport) = &voice_transport {
                                match event.get("type").and_then(Value::as_str) {
                                    Some("peer-connect") => {
                                        #[cfg(feature = "cpal-audio")]
                                        {
                                            voice_microphone = None;
                                            voice_test_sink = None;
                                voice_test_processor = None;
                                        }
                                        let Some(connection_id) = event
                                            .get("connectionId")
                                            .and_then(Value::as_str)
                                            .map(str::trim)
                                            .filter(|value| !value.is_empty())
                                            .map(str::to_owned)
                                        else {
                                            let _ = notice_tx
                                                .send("voice:peer-connect missing connection id".into());
                                            continue;
                                        };
                                        if voice_peer.is_some()
                                            && voice_identity.resume_peer_id.as_deref()
                                                == Some(connection_id.as_str())
                                        {
                                            let _ = notice_tx
                                                .send("voice:duplicate-peer-connect ignored".into());
                                            continue;
                                        }
                                        let peer_generation = voice_peer_generation
                                            .fetch_add(1, Ordering::Relaxed)
                                            .wrapping_add(1);
                                        if let Some(previous_peer) = voice_peer.take() {
                                            let _ = previous_peer.close().await;
                                        }
                                        voice_local_muted.store(false, Ordering::Relaxed);
                                        voice_candidate_history.retain(|id, _| id == &connection_id);
                                        let ice_servers =
                                            voice::ice_servers_from_peer_event(&event);
                                        let session_recorder = if record_sessions {
                                            match crate::session_recorder::StereoSessionRecorder::create_in_downloads_with_parameters(
                                                recording_format,
                                                recording_parameters,
                                            ) {
                                                Ok(recorder) => Some(recorder),
                                                Err(error) => {
                                                    let _ = notice_tx.send(format!(
                                                        "voice:recording failed:{error}"
                                                    ));
                                                    None
                                                }
                                            }
                                        } else {
                                            None
                                        };
                                        let mut sink = crate::audio::default_sink(
                                            speaker_gain_percent,
                                            speaker_dsp_enabled,
                                            1.0,
                                            Some(remote_audio_wav_path.as_str()),
                                            Some(&speaker_device_name),
                                        )
                                        .ok();
                                        if let Some(recorder) = session_recorder.as_ref() {
                                            let recording_sink = recorder.remote_sink();
                                            sink = Some(match sink {
                                                Some(playback_sink) => crate::audio::combine_audio_sinks(
                                                    playback_sink,
                                                    recording_sink,
                                                ),
                                                None => recording_sink,
                                            });
                                        }
                                        match crate::webrtc_backend::WebRtcPeer::new(
                                            &ice_servers,
                                            sink,
                                            session_recorder.clone(),
                                            voice_proxy.as_deref(),
                                        )
                                            .await
                                        {
                                            Ok(peer) => {
                                                if let Some(recorder) = session_recorder.as_ref() {
                                                    let _ = notice_tx.send(format!(
                                                        "voice:recording started:{}",
                                                        recorder.path().display()
                                                    ));
                                                }
                                                let _ = notice_tx
                                                    .send(format!("voice:codec:{}", peer.codec_name()));
                                                let initiator = event
                                                    .get("initiator")
                                                    .and_then(Value::as_bool)
                                                    .unwrap_or(false);
                                                pending_voice_candidates.clear();
                                                if let Some(history) =
                                                    voice_candidate_history.get(&connection_id).cloned()
                                                {
                                                    for (id, mid, index, candidate) in history {
                                                        queue_voice_candidate(
                                                            &mut pending_voice_candidates,
                                                            id,
                                                            mid,
                                                            index,
                                                            candidate,
                                                        );
                                                    }
                                                }
                                                voice_media_ready.store(
                                                    false,
                                                    std::sync::atomic::Ordering::Relaxed,
                                                );
                                                voice_local_sdp_sent.store(false, Ordering::SeqCst);
                                                if let Ok(mut pending) = voice_pending_local_candidates.lock() {
                                                    pending.clear();
                                                }
                                                voice_identity.resume_peer_id =
                                                    Some(connection_id.clone());
                                                let _ = store.save_voice_identity(&voice_identity);
                                                let candidate_transport = voice_transport.clone();
                                                let candidate_connection_id = connection_id.clone();
                                                let candidate_generation = voice_peer_generation.clone();
                                                let candidate_notices = notice_tx.clone();
                                                let peer = std::sync::Arc::new(peer);
                                                let stream_transport = voice_transport.clone();
                                                let stream_connection_id = connection_id.clone();
                                                let stream_media_ready = voice_media_ready.clone();
                                                let stream_local_muted = voice_local_muted.clone();
                                                let stream_generation = voice_peer_generation.clone();
                                                let stream_notices = notice_tx.clone();
                                                peer.on_remote_audio_started(std::sync::Arc::new(
                                                    move || {
                                                        if stream_generation.load(Ordering::Relaxed)
                                                            != peer_generation
                                                        {
                                                            return;
                                                        }
                                                        if stream_media_ready.swap(
                                                            true,
                                                            std::sync::atomic::Ordering::Relaxed,
                                                        ) {
                                                            return;
                                                        }
                                                        let transport = stream_transport.clone();
                                                        let connection_id =
                                                            stream_connection_id.clone();
                                                        let notices = stream_notices.clone();
                                                        let local_muted = stream_local_muted.clone();
                                                        tokio::spawn(async move {
                                                            send_voice_events(
                                                                transport,
                                                                voice::media_ready_events(
                                                                    &connection_id,
                                                                    local_muted.load(Ordering::Relaxed),
                                                                )
                                                                    .to_vec(),
                                                                notices,
                                                            )
                                                            .await;
                                                        });
                                                    },
                                                ));
                                                let ice_transport = voice_transport.clone();
                                                let ice_connection_id = connection_id.clone();
                                                let ice_media_ready = voice_media_ready.clone();
                                                let ice_local_muted = voice_local_muted.clone();
                                                let ice_generation = voice_peer_generation.clone();
                                                let ice_notices = notice_tx.clone();
                                                peer.on_ice_connection_state_change(
                                                    std::sync::Arc::new(move |state| {
                                                        if ice_generation.load(Ordering::Relaxed)
                                                            != peer_generation
                                                        {
                                                            return;
                                                        }
                                                        let _ = ice_notices.send(format!("voice:ice-state:{state}"));
                                                        let is_up =
                                                            state == "connected" || state == "completed";
                                                        if is_up {
                                                            if ice_media_ready.swap(
                                                                true,
                                                                std::sync::atomic::Ordering::Relaxed,
                                                            ) {
                                                                return;
                                                            }
                                                            let transport = ice_transport.clone();
                                                            let connection_id =
                                                                ice_connection_id.clone();
                                                            let notices = ice_notices.clone();
                                                            let local_muted = ice_local_muted.clone();
                                                            tokio::spawn(async move {
                                                                send_voice_events(
                                                                    transport,
                                                                    voice::media_ready_events(
                                                                        &connection_id,
                                                                        local_muted.load(Ordering::Relaxed),
                                                                    )
                                                                    .to_vec(),
                                                                    notices,
                                                                )
                                                                .await;
                                                            });
                                                        } else if matches!(
                                                            state.as_str(),
                                                            "disconnected" | "failed" | "closed"
                                                        ) && ice_media_ready.swap(
                                                            false,
                                                            std::sync::atomic::Ordering::Relaxed,
                                                        ) {
                                                            let transport = ice_transport.clone();
                                                            let connection_id =
                                                                ice_connection_id.clone();
                                                            let notices = ice_notices.clone();
                                                            tokio::spawn(async move {
                                                                send_voice(
                                                                    Some(&transport),
                                                                    voice::peer_connection(
                                                                        false,
                                                                        Some(&connection_id),
                                                                    ),
                                                                    &notices,
                                                                )
                                                                .await;
                                                            });
                                                        }
                                                    }),
                                                );
                                                let timeout_media_ready = voice_media_ready.clone();
                                                let timeout_commands = timeout_command_tx.clone();
                                                let timeout_connection_id = connection_id.clone();
                                                let timeout_generation = voice_peer_generation.clone();
                                                tokio::spawn(async move {
                                                    tokio::time::sleep(std::time::Duration::from_secs(
                                                        VOICE_MEDIA_CONNECT_TIMEOUT_SECONDS,
                                                    )).await;
                                                    if timeout_generation.load(Ordering::Relaxed)
                                                        != peer_generation
                                                    {
                                                        return;
                                                    }
                                                    if !timeout_media_ready.load(std::sync::atomic::Ordering::Relaxed) {
                                                        let _ = timeout_commands.send(Command::VoicePeerTimeout(timeout_connection_id));
                                                    }
                                                });
                                                let local_sdp_sent = voice_local_sdp_sent.clone();
                                                let pending_local = voice_pending_local_candidates.clone();
                                                peer.on_local_candidate(std::sync::Arc::new(
                                                    move |mid, index, candidate| {
                                                        if candidate_generation.load(Ordering::Relaxed)
                                                            != peer_generation
                                                        {
                                                            return;
                                                        }
                                                        if local_sdp_sent.load(Ordering::SeqCst) {
                                                            let transport = candidate_transport.clone();
                                                            let connection_id =
                                                                candidate_connection_id.clone();
                                                            let notices = candidate_notices.clone();
                                                            tokio::spawn(async move {
                                                                send_voice(
                                                                    Some(&transport),
                                                                    voice::ice_candidate(
                                                                        &connection_id,
                                                                        &mid,
                                                                        index,
                                                                        &candidate,
                                                                    ),
                                                                    &notices,
                                                                )
                                                                .await;
                                                            });
                                                        } else if let Ok(mut pending) = pending_local.lock() {
                                                            pending.push((mid, index, candidate));
                                                        }
                                                    },
                                                ));
                                                #[cfg(feature = "cpal-audio")]
                                                {
                                                    let microphone = peer.microphone(
                                                        microphone_gain_percent,
                                                        microphone_dsp_enabled,
                                                        microphone_warmth_enabled,
                                                        saturation_drive,
                                                        saturation_asymmetry,
                                                        saturation_bias,
                                                        presence_boost_db,
                                                        proximity_gain_db,
                                                        air_boost_db,
                                                        voice_changer_enabled,
                                                        &voice_changer_model,
                                                        voice_changer_pitch,
                                                        voice_changer_wet,
                                                        microphone_noise_suppression_enabled,
                                                        echo_cancellation_enabled,
                                                        noise_gate_threshold,
                                                        compressor_threshold,
                                                        compressor_ratio,
                                                        makeup_gain,
                                                        session_recorder.clone(),
                                                    );
                                                    match microphone {
                                                        Ok(mut microphone) => {
                                                            microphone.set_preferred_device(Some(
                                                                microphone_device_name.clone(),
                                                            ));
                                                            if let Err(error) = crate::audio::MicrophoneCapture::start(&mut microphone) {
                                                                let _ = notice_tx.send(format!(
                                                                    "voice:microphone failed: {error}"
                                                                ));
                                                            } else {
                                                                let description = microphone
                                                                    .description()
                                                                    .unwrap_or("default input")
                                                                    .to_owned();
                                                                voice_microphone = Some(microphone);
                                                                let _ = notice_tx.send(format!(
                                                                    "voice:microphone active · {description}"
                                                                ));
                                                            }
                                                        }
                                                        Err(error) => {
                                                            let _ = notice_tx.send(format!(
                                                                "voice:microphone failed: {error}"
                                                            ));
                                                        }
                                                    }
                                                }
                                                voice_peer = Some(peer);
                                                if initiator
                                                    && let Some(peer) = voice_peer.as_ref()
                                                {
                                                    match peer.create_offer().await {
                                                        Ok(sdp) => {
                                                            send_voice(
                                                                Some(voice_transport),
                                                                voice::sdp_event(
                                                                    "offer",
                                                                    &connection_id,
                                                                    &sdp,
                                                                ),
                                                                &notice_tx,
                                                            )
                                                            .await;
                                                            flush_local_voice_candidates(
                                                                &voice_local_sdp_sent,
                                                                &voice_pending_local_candidates,
                                                                Some(voice_transport),
                                                                &connection_id,
                                                                &notice_tx,
                                                            );
                                                        }
                                                        Err(error) => {
                                                            let _ = notice_tx.send(format!(
                                                                "voice:sdp-error:offer:{error}"
                                                            ));
                                                        }
                                                    }
                                                }
                                            }
                                            Err(error) => {
                                                let _ = notice_tx
                                                    .send(format!("voice:peer failed: {error}"));
                                            }
                                        }
                                    }
                                    Some("offer") => {
                                        let Some(connection_id) = event
                                            .get("connectionId")
                                            .and_then(Value::as_str)
                                            .filter(|value| !value.trim().is_empty())
                                            .map(str::to_owned)
                                            .or_else(|| voice_identity.resume_peer_id.clone())
                                        else {
                                            continue;
                                        };
                                        if voice_identity.resume_peer_id.as_deref()
                                            != Some(connection_id.as_str())
                                        {
                                            continue;
                                        }
                                        let Some(peer) = voice_peer.as_ref() else {
                                            let _ = notice_tx
                                                .send("voice:sdp-error:offer received without an active peer".into());
                                            continue;
                                        };
                                        let Some(raw_sdp) = voice::sdp_payload(&event, "offer") else {
                                            let _ = notice_tx
                                                .send("voice:sdp-error:offer payload is missing".into());
                                            continue;
                                        };
                                        let (_, sdp) = match voice::parse_sdp_value(raw_sdp) {
                                            Ok(value) => value,
                                            Err(error) => {
                                                let _ = notice_tx.send(format!(
                                                    "voice:sdp-error:invalid offer: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let answer = match peer.accept_offer(sdp).await {
                                            Ok(answer) => answer,
                                            Err(error) => {
                                                let _ = notice_tx.send(format!(
                                                    "voice:sdp-error:offer rejected: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        let mut remaining = Vec::new();
                                                    for (
                                                        candidate_connection_id,
                                                        mid,
                                                        index,
                                                        candidate,
                                                    ) in pending_voice_candidates.drain(..)
                                                    {
                                                        if candidate_connection_id == connection_id
                                                        {
                                                        let _ = apply_remote_candidate(
                                                            peer,
                                                            candidate,
                                                            mid,
                                                            index,
                                                            &notice_tx,
                                                        )
                                                        .await;
                                                        } else {
                                                            remaining.push((
                                                                candidate_connection_id,
                                                                mid,
                                                                index,
                                                                candidate,
                                                            ));
                                                        }
                                                    }
                                        pending_voice_candidates = remaining;
                                        send_voice(
                                            Some(voice_transport),
                                            voice::sdp_event("answer", &connection_id, &answer),
                                            &notice_tx,
                                        )
                                        .await;
                                        flush_local_voice_candidates(
                                            &voice_local_sdp_sent,
                                            &voice_pending_local_candidates,
                                            Some(voice_transport),
                                            &connection_id,
                                            &notice_tx,
                                        );
                                    }
                                    Some("answer") => {
                                        let Some(connection_id) = event
                                            .get("connectionId")
                                            .and_then(Value::as_str)
                                            .filter(|value| !value.trim().is_empty())
                                            .map(str::to_owned)
                                            .or_else(|| voice_identity.resume_peer_id.clone())
                                        else {
                                            continue;
                                        };
                                        if voice_identity.resume_peer_id.as_deref()
                                            != Some(connection_id.as_str())
                                        {
                                            continue;
                                        }
                                        let Some(peer) = voice_peer.as_ref() else {
                                            let _ = notice_tx
                                                .send("voice:sdp-error:answer received without an active peer".into());
                                            continue;
                                        };
                                        let Some(raw_sdp) = voice::sdp_payload(&event, "answer") else {
                                            let _ = notice_tx
                                                .send("voice:sdp-error:answer payload is missing".into());
                                            continue;
                                        };
                                        let (_, sdp) = match voice::parse_sdp_value(raw_sdp) {
                                            Ok(value) => value,
                                            Err(error) => {
                                                let _ = notice_tx.send(format!(
                                                    "voice:sdp-error:invalid answer: {error}"
                                                ));
                                                continue;
                                            }
                                        };
                                        if let Err(error) = peer.accept_answer(sdp).await {
                                            let _ = notice_tx.send(format!(
                                                "voice:sdp-error:answer rejected: {error}"
                                            ));
                                            continue;
                                        }
                                        let mut remaining = Vec::new();
                                                    for (
                                                        candidate_connection_id,
                                                        mid,
                                                        index,
                                                        candidate,
                                                    ) in pending_voice_candidates.drain(..)
                                                    {
                                                        if candidate_connection_id == connection_id
                                                        {
                                                        let _ = apply_remote_candidate(
                                                            peer,
                                                            candidate,
                                                            mid,
                                                            index,
                                                            &notice_tx,
                                                        )
                                                        .await;
                                                        } else {
                                                            remaining.push((
                                                                candidate_connection_id,
                                                                mid,
                                                                index,
                                                                candidate,
                                                            ));
                                                        }
                                                    }
                                        pending_voice_candidates = remaining;
                                    }
                                    Some("ice-candidate") => {
                                        let Some(connection_id) = event
                                            .get("connectionId")
                                            .and_then(Value::as_str)
                                            .filter(|value| !value.trim().is_empty())
                                            .map(str::to_owned)
                                            .or_else(|| voice_identity.resume_peer_id.clone())
                                        else {
                                            continue;
                                        };
                                        if voice_identity.resume_peer_id.as_deref().is_some_and(
                                            |active_id| active_id != connection_id.as_str(),
                                        ) {
                                            continue;
                                        }
                                        if let Some(raw_candidate) = event.get("candidate")
                                            && let Ok((mid, index, candidate)) =
                                            voice::parse_candidate_value(raw_candidate)
                                        {
                                                queue_voice_candidate(
                                                    voice_candidate_history
                                                        .entry(connection_id.clone())
                                                        .or_default(),
                                                    connection_id.clone(),
                                                    mid.clone(),
                                                    index,
                                                    candidate.clone(),
                                                );
                                                if voice_candidate_history.len() > MAX_VOICE_CANDIDATE_PEERS {
                                                    let current_id = connection_id.clone();
                                                    let resume_id = voice_identity.resume_peer_id.clone();
                                                    voice_candidate_history.retain(|id, _| {
                                                        id == &current_id
                                                            || resume_id.as_deref() == Some(id)
                                                    });
                                                }
                                                let pending =
                                                    if let Some(peer) = voice_peer.as_ref() {
                                                    apply_remote_candidate(
                                                        peer,
                                                        candidate.clone(),
                                                        mid.clone(),
                                                        index,
                                                        &notice_tx,
                                                    )
                                                    .await
                                                } else {
                                                    true
                                                };
                                                if pending {
                                                    queue_voice_candidate(
                                                        &mut pending_voice_candidates,
                                                        connection_id,
                                                        mid,
                                                        index,
                                                        candidate,
                                                    );
                                                }
                                        }
                                    }
                                    Some("peer-disconnect") | Some("peer-soft-disconnect") => {
                                        if !event_matches_current_peer {
                                            continue;
                                        }
                                        let connection_id = event
                                            .get("connectionId")
                                            .and_then(Value::as_str)
                                            .map(str::trim)
                                            .filter(|value| !value.is_empty())
                                            .map(str::to_owned)
                                            .or_else(|| voice_identity.resume_peer_id.clone());
                                        if event.get("type").and_then(Value::as_str)
                                            == Some("peer-soft-disconnect")
                                        {
                                            voice_identity.resume_peer_id = connection_id.clone()
                                                .or_else(|| voice_identity.resume_peer_id.clone());
                                        } else {
                                            voice_identity.resume_peer_id = None;
                                        }
                                        let _ = store.save_voice_identity(&voice_identity);
                                        voice_local_muted.store(false, Ordering::Relaxed);
                                        voice_peer_generation.fetch_add(1, Ordering::Relaxed);
                                        if let Some(peer) = voice_peer.take() {
                                            let _ = peer.close().await;
                                        }
                                        if voice_media_ready.swap(
                                            false,
                                            std::sync::atomic::Ordering::Relaxed,
                                        ) {
                                            send_voice(
                                                Some(voice_transport),
                                                voice::peer_connection(
                                                    false,
                                                    connection_id.as_deref(),
                                                ),
                                                &notice_tx,
                                            )
                                            .await;
                                        }
                                        #[cfg(feature = "cpal-audio")]
                                        {
                                            voice_microphone = None;
                                            voice_test_sink = None;
                                voice_test_processor = None;
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            #[cfg(not(feature = "webrtc-media"))]
                            let _ = event;
                        }
                        Command::Disconnect => {
                            if let Some(bridge) = chat_notice_bridge.take() {
                                bridge.abort();
                            }
                            if let Some(client) = &transport {
                                let _ = client
                                    .send(protocol::socket_action(protocol::online_track(false)))
                                    .await;
                            }
                            transport.as_ref().map(ChatTransport::shutdown);
                            transport = None;
                            let _ = notice_tx.send("disconnected".into());
                        }
                        Command::Shutdown => {
                            if let Some(bridge) = chat_notice_bridge.take() {
                                bridge.abort();
                            }
                            if let Some(bridge) = voice_notice_bridge.take() {
                                bridge.abort();
                            }
                            if let Some(task) = voice_probe_task.take() {
                                task.abort();
                            }
                            #[cfg(feature = "webrtc-media")]
                            voice_local_muted.store(false, Ordering::Relaxed);
                            #[cfg(feature = "webrtc-media")]
                            if let Some(peer) = voice_peer.take() {
                                let _ = peer.close().await;
                            }
                            #[cfg(feature = "webrtc-media")]
                            {
                                pending_voice_candidates.clear();
                                voice_media_ready.store(
                                    false,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                            #[cfg(feature = "cpal-audio")]
                            {
                                voice_microphone = None;
                                voice_test_sink = None;
                                voice_test_processor = None;
                            }
                            if let Some(client) = &transport {
                                client.shutdown();
                            }
                            if let Some(client) = &voice_transport {
                                client.shutdown();
                            }
                            break;
                        }
                        _ => {
                            let _ = notice_tx.send("not connected".into());
                        }
                    }
                }
            });
        });
        Self {
            commands: command_tx,
            notices: notice_rx,
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([860.0, 640.0])
            .with_min_inner_size([620.0, 440.0]),
        wgpu_options: startup_wgpu_options(),
        ..Default::default()
    };
    eframe::run_native(
        "NektoMe — anonymous chat",
        options,
        Box::new(|_| Ok(Box::new(NektoMeApp::new()))),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn startup_wgpu_options() -> eframe::WgpuConfiguration {
    #[allow(unused_mut)]
    let mut options = eframe::WgpuConfiguration::default();
    #[cfg(target_os = "windows")]
    if std::env::var_os("WGPU_BACKEND").is_none()
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut options.wgpu_setup
    {
        // Enumerating the OpenGL backend can crash inside some AMD drivers
        // before wgpu selects a native Windows backend. DX12 covers standard
        // Windows 10 installations, while Vulkan retains a second hardware
        // path for older adapters and drivers.
        setup.instance_descriptor.backends =
            eframe::wgpu::Backends::DX12 | eframe::wgpu::Backends::VULKAN;
    }
    options
}

fn voice_reconnect_delay(attempt: u8) -> std::time::Duration {
    let exponent = u32::from(attempt.min(4));
    std::time::Duration::from_secs((1u64 << exponent).min(15))
}

fn human_file_size(bytes: u64) -> String {
    if bytes < 1_024 {
        format!("{bytes} B")
    } else if bytes < 1_048_576 {
        format_decimal_size(bytes, 1_024, "KiB")
    } else {
        format_decimal_size(bytes, 1_048_576, "MiB")
    }
}

fn format_decimal_size(bytes: u64, unit: u64, suffix: &str) -> String {
    let whole = bytes / unit;
    let tenth = bytes % unit * 10 / unit;
    format!("{whole}.{tenth} {suffix}")
}

#[cfg(feature = "cpal-audio")]
fn audio_device_label(preferred: &str, available: &[String], default: Option<&str>) -> String {
    if preferred.is_empty() {
        return default.map_or_else(
            || "System default (unavailable)".into(),
            |name| format!("System default · {name}"),
        );
    }
    if available.iter().any(|name| name == preferred) {
        preferred.to_owned()
    } else {
        format!("{preferred} · unavailable; using default")
    }
}

struct NektoMeApp {
    worker: Worker,
    voice_tab: bool,
    connected: bool,
    chat_reconnect_search: bool,
    chat_reconnect_requested: bool,
    chat_reconnect_pending: bool,
    chat_reconnect_cancel: Arc<AtomicBool>,
    searching: bool,
    dialog_id: Option<i64>,
    dialog: Option<Dialog>,
    last_message_id: Option<i64>,
    own_user_id: Option<i64>,
    chat_config: Option<AppConfig>,
    pending_random_ids: HashSet<i64>,
    message_ids: HashSet<i64>,
    input: String,
    captcha_solution: String,
    captcha_visible: bool,
    captcha_browser_running: bool,
    captcha_resume_search: bool,
    captcha_left: i64,
    captcha_need: i64,
    captcha_public_key: Option<String>,
    online_summary: String,
    voice_connected: bool,
    voice_reconnect_search: bool,
    voice_reconnect_requested: bool,
    voice_reconnect_pending: bool,
    voice_reconnect_preserve_media: bool,
    voice_reconnect_cancel: Arc<AtomicBool>,
    voice_reconnect_generation: Arc<AtomicU64>,
    voice_reconnect_attempt: u8,
    voice_transport_failure_streak: u8,
    voice_web_mode: bool,
    voice_server_host: String,
    voice_server_override: bool,
    voice_auto_next: bool,
    voice_android_user_id: String,
    voice_web_token: String,
    voice_proxy_seller_api_key: String,
    voice_proxy_seller_list_name: String,
    voice_proxy_seller_ports_count: i64,
    voice_proxy_seller_rotation_seconds: i64,
    voice_proxy_seller_whitelist_ip: String,
    voice_proxy_url: String,
    voice_proxy_index: usize,
    voice_android_user_agent: String,
    voice_android_locale: String,
    voice_android_time_zone: String,
    voice_remote_audio_wav_path: String,
    voice_record_sessions: bool,
    voice_recording_format: crate::session_recorder::RecordingFormat,
    voice_recording_parameters: crate::session_recorder::RecordingParameters,
    voice_saved_recordings: Vec<crate::session_recorder::SavedRecording>,
    voice_recording_delete_pending: Option<std::path::PathBuf>,
    voice_microphone_device: String,
    voice_speaker_device: String,
    #[cfg(feature = "cpal-audio")]
    voice_audio_devices: crate::audio::AudioDevices,
    voice_speaker_gain: f32,
    voice_speaker_dsp: bool,
    voice_microphone_dsp: bool,
    voice_microphone_noise_suppression: bool,
    voice_microphone_gain: f32,
    voice_echo_cancellation: bool,
    voice_noise_gate_threshold: f32,
    voice_compressor_threshold: f32,
    voice_compressor_ratio: f32,
    voice_makeup_gain: f32,
    voice_microphone_warmth: bool,
    voice_saturation_drive: f32,
    voice_saturation_asymmetry: f32,
    voice_saturation_bias: f32,
    voice_saturation_profile: crate::audio::SaturationProfile,
    voice_character_profile: crate::audio::VoiceCharacterProfile,
    voice_presence_boost_db: f32,
    voice_proximity_gain_db: f32,
    voice_air_boost_db: f32,
    voice_changer_enabled: bool,
    voice_changer_model: String,
    voice_changer_pitch: f32,
    voice_changer_wet: f32,
    voice_searching: bool,
    voice_in_call: bool,
    voice_muted: bool,
    voice_speaker_muted: bool,
    voice_push_to_talk: bool,
    voice_peer_muted: bool,
    voice_users: i64,
    voice_waiting: i64,
    voice_talking: i64,
    peer_typing: bool,
    voice_user_sex: String,
    voice_peer_sex: String,
    voice_user_age_from: String,
    voice_user_age_to: String,
    voice_peer_age_from: String,
    voice_peer_age_to: String,
    voice_captcha_solution: String,
    voice_captcha_visible: bool,
    voice_captcha_focus_requested: bool,
    voice_mic_test: bool,
    voice_microphone_active: bool,
    #[cfg(feature = "cpal-audio")]
    voice_microphone_level: f32,
    voice_captcha_submitted: bool,
    voice_wrong_captcha_streak: u8,
    voice_captcha_type: Option<String>,
    voice_captcha_url: Option<String>,
    voice_captcha_image: Option<egui::ColorImage>,
    voice_captcha_texture: Option<egui::TextureHandle>,
    voice_captcha_left: i64,
    voice_captcha_need: i64,
    voice_log: Vec<String>,
    voice_log_filter: String,
    voice_log_auto_scroll: bool,
    voice_session: VoiceSession,
    messages: Vec<ChatLine>,
    log: Vec<String>,
    log_filter: String,
    log_auto_scroll: bool,
}

const MAX_LOG_LINES: usize = 500;
#[cfg(feature = "webrtc-media")]
const MAX_VOICE_CANDIDATE_PEERS: usize = 128;
const VOICE_MEDIA_CONNECT_TIMEOUT_SECONDS: u64 = 45;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ChatLine {
    id: i64,
    is_own: bool,
    text: String,
    created_at: i64,
    read: bool,
}

impl ChatLine {
    fn from_message(message: &crate::domain::Message, is_own: bool) -> Self {
        Self {
            id: message.id.0,
            is_own,
            text: message.text.clone(),
            created_at: message.created_at,
            read: message.read,
        }
    }

    fn display_time(&self) -> String {
        if self.created_at == 0 {
            return String::new();
        }
        Local
            .timestamp_opt(self.created_at, 0)
            .single()
            .map(|time| time.format("%H:%M").to_string())
            .unwrap_or_default()
    }
}

fn trim_log(log: &mut Vec<String>) {
    let excess = log.len().saturating_sub(MAX_LOG_LINES);
    if excess > 0 {
        log.drain(..excess);
    }
}

fn log_level(line: &str) -> &'static str {
    let lower = line.to_lowercase();
    if [
        "error",
        "failed",
        "failure",
        "rejected",
        "unavailable",
        "wrong",
        "ошиб",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "error"
    } else if [
        "disconnect",
        "timeout",
        "captcha",
        "retry",
        "lost",
        "stopped",
        "каптч",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        "warning"
    } else if ["connected", "ready", "accepted", "active", "authenticated"]
        .iter()
        .any(|marker| lower.contains(marker))
    {
        "success"
    } else {
        "info"
    }
}

fn structured_log_json(source: &str, log: &[String]) -> Result<Vec<u8>, serde_json::Error> {
    let entries = log
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let event = message
                .strip_prefix("event:")
                .map(str::trim)
                .filter(|event| !event.is_empty());
            serde_json::json!({
                "sequence": index + 1,
                "level": log_level(message),
                "event": event,
                "message": message,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "source": source,
        "exportedAt": Local::now().to_rfc3339(),
        "entryCount": entries.len(),
        "entries": entries,
    }))
}

fn export_log_json(source: &str, log: &[String]) -> Result<std::path::PathBuf, String> {
    let directory = crate::session_recorder::downloads_directory()?;
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let stamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
    let name = match source {
        "voice-roulette" => "voice-roulette",
        _ => "text-chat",
    };
    let mut path = directory.join(format!("NektoMe-{name}-log-{stamp}.json"));
    if path.exists() {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        path = directory.join(format!("NektoMe-{name}-log-{stamp}-{}.json", &suffix[..8]));
    }
    let bytes = structured_log_json(source, log).map_err(|error| error.to_string())?;
    std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
    Ok(path)
}

fn show_log_viewer(
    ui: &mut egui::Ui,
    id: &str,
    source: &str,
    log: &mut Vec<String>,
    filter: &mut String,
    auto_scroll: &mut bool,
    max_height: f32,
) {
    let mut normalized_filter = String::new();
    let mut visible_count = 0;

    ui.horizontal_wrapped(|ui| {
        ui.label("Filter");
        ui.add(
            egui::TextEdit::singleline(filter)
                .hint_text("Search log…")
                .desired_width(180.0),
        );
        if !filter.is_empty() && ui.small_button("Clear filter").clicked() {
            filter.clear();
        }
        normalized_filter = filter.trim().to_lowercase();
        visible_count = log
            .iter()
            .filter(|line| {
                normalized_filter.is_empty() || line.to_lowercase().contains(&normalized_filter)
            })
            .count();
        ui.checkbox(auto_scroll, "Follow");
        ui.small(format!("{visible_count} / {} entries", log.len()));
        let copy_label = if normalized_filter.is_empty() {
            "Copy all"
        } else {
            "Copy visible"
        };
        if ui
            .add_enabled(visible_count > 0, egui::Button::new(copy_label))
            .clicked()
        {
            ui.ctx().copy_text(
                log.iter()
                    .filter(|line| {
                        normalized_filter.is_empty()
                            || line.to_lowercase().contains(&normalized_filter)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        if ui
            .add_enabled(!log.is_empty(), egui::Button::new("Export JSON"))
            .on_hover_text("Save all entries as structured JSON in Downloads")
            .clicked()
        {
            match export_log_json(source, log) {
                Ok(path) => log.push(format!("Exported JSON log to {}", path.display())),
                Err(error) => log.push(format!("Log export failed: {error}")),
            }
            trim_log(log);
        }
        if ui
            .add_enabled(!log.is_empty(), egui::Button::new("Clear log"))
            .clicked()
        {
            log.clear();
        }
    });

    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(max_height)
        .stick_to_bottom(*auto_scroll)
        .show(ui, |ui| {
            if visible_count == 0 {
                ui.weak(if log.is_empty() {
                    "No log entries"
                } else {
                    "No entries match this filter"
                });
            }
            for line in log.iter().filter(|line| {
                normalized_filter.is_empty() || line.to_lowercase().contains(&normalized_filter)
            }) {
                let color = match log_level(line) {
                    "error" => ui.visuals().error_fg_color,
                    "warning" => ui.visuals().warn_fg_color,
                    "success" => egui::Color32::from_rgb(70, 175, 105),
                    _ => ui.visuals().text_color(),
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(line).monospace().color(color))
                        .selectable(true),
                );
            }
        });
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VoiceAgePreset {
    Under18,    // "до 17 лет" (0..=17)
    From18To24, // "от 18 до 24 лет" (18..=24)
    From25To32, // "от 25 до 32 лет" (25..=32)
    Over33,     // "старше 33 лет" (33..=99)
}

impl VoiceAgePreset {
    pub const ALL: [Self; 4] = [
        Self::Under18,
        Self::From18To24,
        Self::From25To32,
        Self::Over33,
    ];

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Under18 => "до 17 лет",
            Self::From18To24 => "от 18 до 24 лет",
            Self::From25To32 => "от 25 до 32 лет",
            Self::Over33 => "старше 33 лет",
        }
    }

    #[must_use]
    pub const fn range(self) -> (i32, i32) {
        match self {
            Self::Under18 => (0, 17),
            Self::From18To24 => (18, 24),
            Self::From25To32 => (25, 32),
            Self::Over33 => (33, 99),
        }
    }

    #[must_use]
    pub const fn matches_range(self, from: i32, to: i32) -> bool {
        match self {
            Self::Under18 => (from == 0 || from == 1) && to == 17,
            Self::From18To24 => from == 18 && to == 24,
            Self::From25To32 => from == 25 && to == 32,
            Self::Over33 => from == 33 && (to == 99 || to == 100 || to == 120),
        }
    }
}

impl NektoMeApp {
    fn new() -> Self {
        Self {
            worker: Worker::new(),
            voice_tab: false,
            connected: false,
            chat_reconnect_search: false,
            chat_reconnect_requested: false,
            chat_reconnect_pending: false,
            chat_reconnect_cancel: Arc::new(AtomicBool::new(true)),
            searching: false,
            dialog_id: None,
            dialog: None,
            last_message_id: None,
            own_user_id: None,
            chat_config: None,
            pending_random_ids: HashSet::new(),
            message_ids: HashSet::new(),
            input: String::new(),
            captcha_solution: String::new(),
            captcha_visible: false,
            captcha_browser_running: false,
            captcha_resume_search: false,
            captcha_left: 0,
            captcha_need: 0,
            captcha_public_key: None,
            online_summary: String::new(),
            voice_connected: false,
            voice_reconnect_search: false,
            voice_reconnect_requested: false,
            voice_reconnect_pending: false,
            voice_reconnect_preserve_media: false,
            voice_reconnect_cancel: Arc::new(AtomicBool::new(true)),
            voice_reconnect_generation: Arc::new(AtomicU64::new(0)),
            voice_reconnect_attempt: 0,
            voice_transport_failure_streak: 0,
            voice_web_mode: false,
            voice_server_host: "audio.nekto.me".into(),
            voice_server_override: false,
            voice_auto_next: false,
            voice_android_user_id: String::new(),
            voice_web_token: String::new(),
            voice_proxy_seller_api_key: String::new(),
            voice_proxy_seller_list_name: "nektome_app".into(),
            voice_proxy_seller_ports_count: 20,
            voice_proxy_seller_rotation_seconds: 3600,
            voice_proxy_seller_whitelist_ip: String::new(),
            voice_proxy_url: String::new(),
            voice_proxy_index: 0,
            voice_android_user_agent: String::new(),
            voice_android_locale: String::new(),
            voice_android_time_zone: String::new(),
            voice_remote_audio_wav_path: String::new(),
            voice_record_sessions: false,
            voice_recording_format: crate::session_recorder::RecordingFormat::Flac,
            voice_recording_parameters: crate::session_recorder::RecordingParameters::default(),
            voice_saved_recordings: crate::session_recorder::saved_recordings().unwrap_or_default(),
            voice_recording_delete_pending: None,
            voice_microphone_device: String::new(),
            voice_speaker_device: String::new(),
            #[cfg(feature = "cpal-audio")]
            voice_audio_devices: crate::audio::audio_devices(),
            voice_speaker_gain: 100.0,
            voice_speaker_dsp: true,
            voice_microphone_dsp: true,
            voice_microphone_noise_suppression: true,
            voice_microphone_gain: 100.0,
            voice_echo_cancellation: true,
            voice_noise_gate_threshold: 0.002,
            voice_compressor_threshold: 0.12,
            voice_compressor_ratio: 2.5,
            voice_makeup_gain: 1.35,
            voice_microphone_warmth: true,
            voice_saturation_drive: 1.08,
            voice_saturation_asymmetry: 1.15,
            voice_saturation_bias: 0.0,
            voice_saturation_profile: crate::audio::SaturationProfile::NaturalVoice,
            voice_character_profile: crate::audio::VoiceCharacterProfile::Natural,
            voice_presence_boost_db: 0.0,
            voice_proximity_gain_db: 0.0,
            voice_air_boost_db: 0.0,
            voice_changer_enabled: false,
            voice_changer_model: "models/voices/woman_1.onnx".into(),
            voice_changer_pitch: 12.0,
            voice_changer_wet: 1.0,
            voice_searching: false,
            voice_in_call: false,
            voice_muted: false,
            voice_speaker_muted: false,
            voice_push_to_talk: false,
            voice_peer_muted: false,
            voice_users: 0,
            voice_waiting: 0,
            voice_talking: 0,
            peer_typing: false,
            voice_user_sex: "ANY".into(),
            voice_peer_sex: "ANY".into(),
            voice_user_age_from: String::new(),
            voice_user_age_to: String::new(),
            voice_peer_age_from: String::new(),
            voice_peer_age_to: String::new(),
            voice_captcha_solution: String::new(),
            voice_captcha_visible: false,
            voice_captcha_focus_requested: false,
            voice_mic_test: false,
            voice_microphone_active: false,
            #[cfg(feature = "cpal-audio")]
            voice_microphone_level: 0.0,
            voice_captcha_submitted: false,
            voice_wrong_captcha_streak: 0,
            voice_captcha_type: None,
            voice_captcha_url: None,
            voice_captcha_image: None,
            voice_captcha_texture: None,
            voice_captcha_left: 0,
            voice_captcha_need: 0,
            voice_log: vec!["Voice roulette ready.".into()],
            voice_log_filter: String::new(),
            voice_log_auto_scroll: true,
            voice_session: VoiceSession::default(),
            messages: Vec::new(),
            log: vec!["Ready. Press Connect.".into()],
            log_filter: String::new(),
            log_auto_scroll: true,
        }
    }

    fn poll(&mut self) {
        while let Ok(raw) = self.worker.notices.try_recv() {
            self.handle_notice(&raw);
        }
    }

    fn schedule_chat_reconnect(&mut self) {
        if !self.chat_reconnect_requested || self.chat_reconnect_pending {
            return;
        }
        self.chat_reconnect_pending = true;
        self.chat_reconnect_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.chat_reconnect_cancel);
        let commands = self.worker.commands.clone();
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_secs(2));
            if !cancel.load(Ordering::Relaxed) {
                let _ = commands.send(Command::Connect);
            }
        });
        self.log
            .push("Connection lost; retrying in 2 seconds…".into());
    }

    fn schedule_voice_reconnect(&mut self) {
        if !self.voice_reconnect_requested || self.voice_reconnect_pending {
            return;
        }
        self.voice_reconnect_pending = true;
        self.voice_reconnect_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.voice_reconnect_cancel);
        let generation = Arc::clone(&self.voice_reconnect_generation);
        let reconnect_generation = generation.fetch_add(1, Ordering::Relaxed) + 1;
        let delay = voice_reconnect_delay(self.voice_reconnect_attempt);
        self.voice_reconnect_attempt = self.voice_reconnect_attempt.saturating_add(1);
        let commands = self.worker.commands.clone();
        let command = self.voice_connect_command_with_media(
            self.voice_reconnect_preserve_media && self.voice_in_call,
        );
        thread::spawn(move || {
            thread::sleep(delay);
            if !cancel.load(Ordering::Relaxed)
                && generation.load(Ordering::Relaxed) == reconnect_generation
            {
                let _ = commands.send(command);
            }
        });
        self.voice_log.push(format!(
            "Signaling connection lost; retrying in {} second{}…",
            delay.as_secs(),
            if delay.as_secs() == 1 { "" } else { "s" }
        ));
    }

    fn cancel_voice_reconnect(&mut self) {
        self.voice_reconnect_cancel.store(true, Ordering::Relaxed);
        self.voice_reconnect_generation
            .fetch_add(1, Ordering::Relaxed);
        self.voice_reconnect_requested = false;
        self.voice_reconnect_pending = false;
        self.voice_reconnect_preserve_media = false;
        self.voice_reconnect_attempt = 0;
    }

    fn handle_notice(&mut self, raw: &str) {
        // Keep both diagnostic panes bounded even for notice branches that
        // return early (transport errors, CAPTCHA images, and proxy results).
        trim_log(&mut self.log);
        trim_log(&mut self.voice_log);
        if let Some(token) = raw.strip_prefix("chat-captcha-token:") {
            self.captcha_browser_running = false;
            self.captcha_visible = false;
            self.chat_reconnect_search = true;
            self.log
                .push("Captcha solved; adopting the verified browser session…".into());
            let _ = self
                .worker
                .commands
                .send(Command::SaveToken(token.to_owned()));
            let _ = self.worker.commands.send(Command::Connect);
            return;
        }
        if let Some(error) = raw.strip_prefix("chat-captcha-browser-error:") {
            self.captcha_browser_running = false;
            self.log.push(format!("Captcha browser failed: {error}"));
            return;
        }
        if let Some(error) = raw.strip_prefix("connect failed:") {
            self.connected = false;
            self.log.push(format!("Chat reconnect failed: {error}"));
            self.chat_reconnect_pending = false;
            self.schedule_chat_reconnect();
            return;
        }
        if let Some(error) = raw.strip_prefix("send-error:") {
            self.pending_random_ids.clear();
            self.log.push(format!("Send failed: {error}"));
            return;
        }
        if raw == "token-cleared" {
            self.log.push(
                "Saved chat token cleared; next connection will authenticate freshly.".into(),
            );
            return;
        }
        if let Some(error) = raw.strip_prefix("token-clear-error:") {
            self.log
                .push(format!("Could not clear saved chat token: {error}"));
            return;
        }
        if let Some(error) = raw
            .strip_prefix("transport-error:")
            .or_else(|| raw.strip_prefix("transport-closed:"))
        {
            self.pending_random_ids.clear();
            self.log.push(format!("Chat transport failed: {error}"));
            let unexpected = self.connected;
            self.connected = false;
            if unexpected {
                self.chat_reconnect_search = self.searching;
                self.chat_reconnect_requested = true;
                self.schedule_chat_reconnect();
            }
            return;
        }
        if let Some(raw_preferences) = raw.strip_prefix("voice-preferences:") {
            if let Ok(preferences) = serde_json::from_str::<VoicePreferences>(raw_preferences) {
                self.voice_user_sex = preferences.user_sex;
                self.voice_proxy_seller_api_key = preferences.proxy_seller_api_key;
                self.voice_proxy_seller_list_name = preferences.proxy_seller_list_name;
                self.voice_proxy_seller_ports_count = preferences.proxy_seller_ports_count;
                self.voice_proxy_seller_rotation_seconds =
                    preferences.proxy_seller_rotation_seconds;
                self.voice_proxy_seller_whitelist_ip = preferences.proxy_seller_whitelist_ip;
                self.voice_web_token = preferences.web_token;
                self.voice_web_mode = preferences.web_mode;
                self.voice_server_host = if preferences.voice_server_host.trim().is_empty() {
                    "audio.nekto.me".into()
                } else {
                    preferences.voice_server_host
                };
                self.voice_server_override =
                    preferences.voice_server_override || self.voice_server_host != "audio.nekto.me";
                self.voice_auto_next = preferences.auto_next;
                self.voice_proxy_url = preferences.proxy_url;
                self.voice_proxy_index = 0;
                self.voice_android_user_agent = preferences.android_user_agent;
                self.voice_android_locale = preferences.android_locale;
                self.voice_android_time_zone = preferences.android_time_zone;
                self.voice_remote_audio_wav_path = preferences.remote_audio_wav_path;
                self.voice_record_sessions = preferences.record_sessions;
                self.voice_recording_format = preferences.recording_format;
                self.voice_recording_parameters = preferences.recording_parameters;
                self.voice_microphone_device = preferences.microphone_device_name;
                self.voice_speaker_device = preferences.speaker_device_name;
                self.voice_push_to_talk = preferences.push_to_talk;
                self.voice_speaker_gain = preferences.speaker_gain_percent;
                self.voice_speaker_dsp = preferences.speaker_dsp_enabled;
                self.voice_microphone_dsp = preferences.microphone_dsp_enabled;
                self.voice_microphone_noise_suppression =
                    preferences.microphone_noise_suppression_enabled;
                self.voice_microphone_gain = preferences.microphone_gain_percent;
                self.voice_echo_cancellation = preferences.echo_cancellation_enabled;
                self.voice_noise_gate_threshold = preferences.noise_gate_threshold;
                self.voice_compressor_threshold = preferences.compressor_threshold;
                self.voice_compressor_ratio = preferences.compressor_ratio;
                self.voice_makeup_gain = preferences.makeup_gain;
                self.voice_microphone_warmth = preferences.microphone_warmth_enabled;
                self.voice_saturation_drive = preferences.saturation_drive;
                self.voice_saturation_asymmetry = preferences.saturation_asymmetry;
                self.voice_saturation_bias = preferences.saturation_bias;
                self.voice_saturation_profile = preferences.saturation_profile;
                self.voice_character_profile = preferences.character_profile;
                self.voice_presence_boost_db = preferences.presence_boost_db;
                self.voice_proximity_gain_db = preferences.proximity_gain_db;
                self.voice_air_boost_db = preferences.air_boost_db;
                self.voice_changer_enabled = preferences.voice_changer_enabled;
                self.voice_changer_model = preferences.voice_changer_model;
                self.voice_changer_pitch = preferences.voice_changer_pitch;
                self.voice_changer_wet = preferences.voice_changer_wet;
                self.voice_peer_sex = preferences.peer_sex;
                self.voice_user_age_from = preferences.user_age_from;
                self.voice_user_age_to = preferences.user_age_to;
                self.voice_peer_age_from = preferences.peer_age_from;
                self.voice_peer_age_to = preferences.peer_age_to;
            }
            return;
        }
        if let Some(voice_raw) = raw.strip_prefix("voice:") {
            self.handle_voice_notice(voice_raw);
            return;
        }
        if let Some(result) = raw.strip_prefix("proxy-seller:ok:") {
            match serde_json::from_str::<crate::proxy_seller::ProxySellerResult>(result) {
                Ok(result) => {
                    self.voice_proxy_url = result.proxies.join(", ");
                    self.voice_proxy_index = 0;
                    self.voice_log.push(format!(
                        "{} Loaded {} resident proxies (list #{})",
                        result.package_summary,
                        result.proxies.len(),
                        result.list_id
                    ));
                    let _ = self
                        .worker
                        .commands
                        .send(Command::SaveVoicePreferences(self.voice_preferences()));
                }
                Err(error) => self
                    .voice_log
                    .push(format!("Proxy-Seller response invalid: {error}")),
            }
            return;
        }
        if let Some(error) = raw.strip_prefix("proxy-seller:error:") {
            self.voice_log
                .push(format!("Proxy-Seller request failed: {error}"));
            return;
        }
        if matches!(raw, "connected" | "ready" | "disconnected" | "connecting") {
            let unexpected = raw == "disconnected" && self.connected;
            self.log.push(raw.into());
            self.connected = raw != "disconnected";
            if raw == "connected" {
                self.chat_reconnect_pending = false;
                self.chat_reconnect_cancel.store(true, Ordering::Relaxed);
                self.chat_reconnect_requested = false;
            }
            if unexpected {
                self.chat_reconnect_search = self.searching;
                self.chat_reconnect_requested = true;
                self.schedule_chat_reconnect();
            }
            return;
        }
        if let Ok(envelope) = serde_json::from_str::<protocol::NoticeEnvelope>(raw) {
            let name = envelope.notice.unwrap_or_else(|| "unknown".into());
            let data = envelope.data.unwrap_or(Value::Null);
            match name.as_str() {
                "auth.successToken" | "auth.successUser" => {
                    let persist_token = name == "auth.successToken";
                    self.own_user_id = data.get("id").and_then(Value::as_i64);
                    if let Some(config_value) = data.get("config").filter(|value| value.is_object())
                    {
                        let config = protocol::app_config(config_value);
                        if let Some(alert) = config
                            .alert_info
                            .as_deref()
                            .filter(|alert| !alert.trim().is_empty())
                        {
                            self.log.push(format!("Server notice: {alert}"));
                        }
                        self.chat_config = Some(config);
                    }
                    if persist_token
                        && let Some(token) =
                            data.pointer("/tokenInfo/authToken").and_then(Value::as_str)
                    {
                        let _ = self
                            .worker
                            .commands
                            .send(Command::SaveToken(token.to_owned()));
                    }
                    let _ = self
                        .worker
                        .commands
                        .send(Command::Send(protocol::online_track(true)));
                    if protocol::communication_banned(&data) {
                        self.log
                            .push("Chat communication is currently banned".into());
                    }
                    if self.chat_reconnect_search {
                        self.chat_reconnect_search = false;
                        let _ = self.worker.commands.send(Command::Search);
                    }
                    self.log.push(format!(
                        "Authenticated as #{}",
                        self.own_user_id.unwrap_or_default()
                    ));
                }
                "search.success" => {
                    self.searching = true;
                    self.log.push("Searching for a partner…".into());
                }
                "search.out" => {
                    self.searching = false;
                    self.log.push("Search stopped".into());
                }
                "dialog.opened" | "dialog.info" => {
                    let closed = data.get("close").and_then(Value::as_bool) == Some(true);
                    self.dialog_id = data.get("id").and_then(Value::as_i64);
                    self.dialog = protocol::parse_dialog(&data);
                    self.last_message_id = None;
                    self.pending_random_ids.clear();
                    self.message_ids.clear();
                    self.messages.clear();
                    if let Some(dialog) = &self.dialog {
                        for message in &dialog.messages {
                            let own = message.sender_id.0 == self.own_user_id.unwrap_or_default();
                            self.message_ids.insert(message.id.0);
                            if self.last_message_id.is_none_or(|last| message.id.0 > last) {
                                self.last_message_id = Some(message.id.0);
                            }
                            self.messages.push(ChatLine::from_message(message, own));
                        }
                    }
                    if closed {
                        if let Some(dialog) = &mut self.dialog {
                            dialog.close();
                        }
                        self.dialog_id = None;
                        self.last_message_id = None;
                        self.pending_random_ids.clear();
                        self.message_ids.clear();
                        self.log.push("Dialog is already closed".into());
                    }
                }
                "dialog.closed" => {
                    if let Some(dialog) = &mut self.dialog {
                        dialog.close();
                    }
                    self.dialog_id = None;
                    self.last_message_id = None;
                    self.pending_random_ids.clear();
                    self.message_ids.clear();
                    self.peer_typing = false;
                    self.log.push("Dialog closed".into());
                }
                "dialog.typing" => {
                    self.peer_typing = data.get("typing").and_then(Value::as_bool).unwrap_or(false);
                }
                "messages.new" => {
                    let added = self.add_message(&data);
                    if added
                        && let (Some(dialog_id), Some(message_id)) =
                            (self.dialog_id, self.last_message_id)
                        && data.get("senderId").and_then(Value::as_i64) != self.own_user_id
                    {
                        let _ = self
                            .worker
                            .commands
                            .send(Command::Send(protocol::read_messages(
                                dialog_id, message_id,
                            )));
                    }
                }
                "messages.reads" => {
                    let watermark = data
                        .get("reads")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_i64)
                        .max()
                        .or_else(|| {
                            data.get("lastMessageId")
                                .or_else(|| data.get("messageId"))
                                .and_then(Value::as_i64)
                        });
                    if let Some(watermark) = watermark {
                        if let Some(dialog) = &mut self.dialog {
                            dialog.mark_read(crate::domain::MessageId(watermark));
                        }
                        for message in &mut self.messages {
                            if message.id <= watermark {
                                message.read = true;
                            }
                        }
                    }
                    self.log.push(format!(
                        "Messages read through #{}",
                        watermark.unwrap_or_default()
                    ));
                }
                "dialog.paid" | "purchase.changed" => {
                    self.log.push(format!(
                        "Account status changed{}",
                        data.get("paidType")
                            .and_then(Value::as_i64)
                            .map_or(String::new(), |paid_type| format!(
                                " (paid type {paid_type})"
                            ))
                    ));
                }
                "socket.close" => {
                    self.chat_reconnect_search = self.searching;
                    self.connected = false;
                    self.chat_reconnect_requested = true;
                    self.log.push("Server closed the chat connection".into());
                    self.schedule_chat_reconnect();
                }
                "online.count" => {
                    self.online_summary = format!(
                        "Online: {} in chats · {} in search",
                        data.get("inChats").and_then(Value::as_i64).unwrap_or(0),
                        data.get("inSearch").and_then(Value::as_i64).unwrap_or(0)
                    );
                }
                "captcha.verify" => {
                    if let Some(solution) = data.get("solution").and_then(Value::as_str) {
                        let _ = self.worker.commands.send(Command::Captcha(solution.into()));
                        self.log
                            .push("Captcha supplied by server; verifying automatically".into());
                    } else {
                        self.captcha_visible = false;
                        self.log.push("Captcha verified".into());
                        if self.captcha_resume_search {
                            self.captcha_resume_search = false;
                            let _ = self.worker.commands.send(Command::Search);
                        }
                    }
                }
                "error.code" => {
                    let code = data
                        .get("code")
                        .or_else(|| data.get("id"))
                        .and_then(Value::as_i64)
                        .map_or_else(|| "?".into(), |code| code.to_string());
                    let description = data
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let additional = data
                        .get("additional")
                        .filter(|value| !value.is_null())
                        .map_or_else(String::new, |value| format!(" ({value})"));
                    if let Some((left, need, public_key)) = chat_captcha_details(&data) {
                        self.searching = false;
                        self.captcha_visible = true;
                        self.captcha_resume_search = true;
                        self.captcha_left = left;
                        self.captcha_need = need;
                        self.captcha_public_key = public_key;
                        self.log.push(format!(
                            "Captcha required ({left}/{need} chats completed). Solve it in Chrome to continue."
                        ));
                    } else {
                        self.log
                            .push(format!("Error {code}: {description}{additional}"));
                    }
                }
                _ => self.log.push(format!("notice: {name}")),
            }
        } else {
            self.log.push(raw.into());
        }
        if self.log.len() > 500 {
            self.log.remove(0);
        }
    }

    fn handle_voice_notice(&mut self, raw: &str) {
        if let Some(error) = raw
            .strip_prefix("connect failed: ")
            .or_else(|| raw.strip_prefix("register failed: "))
        {
            self.voice_connected = false;
            self.voice_reconnect_pending = false;
            self.voice_log
                .push(format!("Voice signaling reconnect failed: {error}"));
            let proxy_info = if self.voice_proxy_url.trim().is_empty() {
                "direct (no proxy)".to_owned()
            } else {
                format!("proxy active (entry #{})", self.voice_proxy_index + 1)
            };
            self.voice_log.push(format!(
                "Channel: server={}, mode={}, {}",
                self.voice_server_host,
                if self.voice_web_mode {
                    "Web"
                } else {
                    "Android"
                },
                proxy_info,
            ));
            self.voice_log.push(
                "Tip: If server is unreachable, test 'Switch to audio.nekto-me.kz' or configure a proxy in Settings.".into(),
            );
            self.schedule_voice_reconnect();
            return;
        }
        if let Some(state) = raw.strip_prefix("ice-state:") {
            self.voice_log.push(format!("WebRTC ICE state: {state}"));
            if state == "failed" || state == "disconnected" {
                self.voice_log.push(
                    "Tip: WebRTC media path lost/failed. Configure a SOCKS5/HTTP proxy in Settings -> Voice proxy or test 'Switch to audio.nekto-me.kz'.".into(),
                );
            }
            return;
        }
        if let Some(path) = raw.strip_prefix("recording started:") {
            self.voice_log
                .push(format!("Recording this call to {path}"));
            return;
        }
        if let Some(error) = raw.strip_prefix("recording failed:") {
            self.voice_log
                .push(format!("Session recording unavailable: {error}"));
            return;
        }
        if let Some(error) = raw.strip_prefix("send-error:") {
            if self.voice_captcha_visible && self.voice_captcha_submitted {
                self.voice_captcha_submitted = false;
                self.voice_captcha_focus_requested = true;
            }
            self.voice_searching = false;
            self.voice_log.push(format!("Voice send failed: {error}"));
            self.voice_log.push(
                "Channel: transport channel closed or busy during send; reconnecting.".into(),
            );
            return;
        }
        if let Some(codec) = raw.strip_prefix("codec:") {
            self.voice_log.push(format!("Media codec: {codec}"));
            return;
        }
        if let Some(active_proxy) = raw.strip_prefix("active-proxy:") {
            let active_proxy = crate::transport::normalize_proxy_url(active_proxy);
            if let Some(index) = crate::transport::proxy_entries(&self.voice_proxy_url)
                .iter()
                .position(|entry| crate::transport::normalize_proxy_url(entry) == active_proxy)
            {
                self.voice_proxy_index = index;
            }
            return;
        }
        if let Some(user_id) = raw.strip_prefix("android-id:") {
            self.voice_android_user_id = user_id.to_owned();
            return;
        }
        if let Some(user_agent) = raw.strip_prefix("android-ua:") {
            self.voice_android_user_agent = user_agent.to_owned();
            return;
        }
        if let Some(connection_id) = raw.strip_prefix("emulator-imported:") {
            self.voice_log.push(format!(
                "Imported emulator voice profile (last connection: {connection_id})."
            ));
            return;
        }
        if let Some(error) = raw.strip_prefix("emulator-import-error:") {
            self.voice_log
                .push(format!("Emulator import failed: {error}"));
            return;
        }
        if let Some(token) = raw.strip_prefix("web-token:") {
            self.voice_web_token = token.to_owned();
            self.voice_log
                .push("Generated a new Web voice token".into());
            return;
        }
        if let Some(encoded) = raw.strip_prefix("captcha-image:") {
            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) => match image::load_from_memory(&bytes) {
                    Ok(image) => {
                        let image = image.to_rgba8();
                        let size = [image.width() as usize, image.height() as usize];
                        self.voice_captcha_image = Some(egui::ColorImage::from_rgba_unmultiplied(
                            size,
                            image.as_raw(),
                        ));
                        self.voice_captcha_texture = None;
                    }
                    Err(error) => self
                        .voice_log
                        .push(format!("Captcha image decode failed: {error}")),
                },
                Err(error) => self
                    .voice_log
                    .push(format!("Captcha image transfer invalid: {error}")),
            }
            return;
        }
        if let Some(error) = raw.strip_prefix("captcha-image-error:") {
            self.voice_log
                .push(format!("Captcha image download failed: {error}"));
            return;
        }
        if raw == "mic-test:started" {
            self.voice_mic_test = true;
            self.voice_log
                .push("Microphone loopback test started.".into());
            return;
        }
        if raw == "mic-test:stopped" {
            self.voice_mic_test = false;
            #[cfg(feature = "cpal-audio")]
            {
                self.voice_microphone_level = 0.0;
            }
            self.voice_log
                .push("Microphone loopback test stopped.".into());
            return;
        }
        if let Some(error) = raw.strip_prefix("mic-test:failed:") {
            self.voice_mic_test = false;
            #[cfg(feature = "cpal-audio")]
            {
                self.voice_microphone_level = 0.0;
            }
            self.voice_log
                .push(format!("Microphone loopback unavailable: {error}"));
            return;
        }
        if let Some(description) = raw.strip_prefix("microphone active") {
            self.voice_microphone_active = true;
            self.voice_log
                .push(format!("Microphone active{}", description));
            return;
        }
        if let Some(error) = raw.strip_prefix("microphone failed: ") {
            self.voice_microphone_active = false;
            self.voice_log
                .push(format!("Microphone unavailable: {error}"));
            return;
        }
        if raw == "identity-reset" {
            self.voice_session = VoiceSession::default();
            self.voice_connected = false;
            self.voice_searching = false;
            self.voice_in_call = false;
            self.voice_muted = false;
            self.voice_peer_muted = false;
            self.voice_mic_test = false;
            self.voice_microphone_active = false;
            self.voice_captcha_visible = false;
            self.voice_captcha_focus_requested = false;
            self.voice_captcha_solution.clear();
            self.voice_captcha_submitted = false;
            self.voice_wrong_captcha_streak = 0;
            self.voice_captcha_type = None;
            self.voice_captcha_url = None;
            self.voice_captcha_left = 0;
            self.voice_captcha_need = 0;
            self.voice_transport_failure_streak = 0;
            self.voice_log
                .push("Voice identity reset; reconnect to apply it".into());
            return;
        }
        if raw == "peer-timeout" {
            self.voice_in_call = false;
            self.voice_microphone_active = false;
            self.voice_muted = false;
            self.voice_peer_muted = false;
            #[cfg(feature = "cpal-audio")]
            {
                self.voice_microphone_level = 0.0;
            }
            self.voice_searching = true;
            self.voice_log.push(format!(
                "Peer media did not connect within {VOICE_MEDIA_CONNECT_TIMEOUT_SECONDS} seconds; searching again"
            ));
            self.voice_log.push(
                "Tip: Peer media timed out (ICE traversal failed). P2P UDP may be blocked by your network/NAT. Setting a proxy or VPN routes WebRTC through TURN relay.".into(),
            );
            let _ = self.worker.commands.send(self.voice_search_command(None));
            return;
        }
        if let Some(reason) = raw
            .strip_prefix("transport-error:")
            .or_else(|| raw.strip_prefix("transport-closed:"))
        {
            self.voice_transport_failure_streak =
                self.voice_transport_failure_streak.saturating_add(1);
            self.voice_log.push(format!(
                "Voice transport failure ({}/3): {}",
                self.voice_transport_failure_streak, reason
            ));
            let proxy_info = if self.voice_proxy_url.trim().is_empty() {
                "direct (no proxy)".to_owned()
            } else {
                format!("proxy active (entry #{})", self.voice_proxy_index + 1)
            };
            self.voice_log.push(format!(
                "Channel: server={}, mode={}, {}",
                self.voice_server_host,
                if self.voice_web_mode {
                    "Web"
                } else {
                    "Android"
                },
                proxy_info,
            ));
            if self.voice_transport_failure_streak >= 2 {
                self.voice_log.push(
                    "Tip: If connection resets persist, switch server to audio.nekto-me.kz in More menu, or set a proxy in Settings.".into(),
                );
            }
            if self.voice_transport_failure_streak >= 3
                && crate::transport::proxy_entries(&self.voice_proxy_url).len() >= 2
            {
                self.voice_transport_failure_streak = 0;
                self.recover_voice_route(self.voice_searching, false);
            }
            return;
        }
        if matches!(raw, "connected" | "ready" | "connecting") {
            if raw == "ready" {
                self.voice_transport_failure_streak = 0;
            }
            self.voice_connected = raw != "disconnected";
            self.voice_log.push(raw.into());
        } else if raw == "disconnected" {
            let unexpected = self.voice_connected;
            let preserve_media = unexpected && self.voice_in_call;
            self.voice_reconnect_search = self.voice_searching;
            self.voice_connected = false;
            self.voice_searching = false;
            if !preserve_media {
                self.voice_session = VoiceSession::default();
                self.voice_in_call = false;
                self.voice_microphone_active = false;
                self.voice_muted = false;
                self.voice_peer_muted = false;
                self.voice_mic_test = false;
                #[cfg(feature = "cpal-audio")]
                {
                    self.voice_microphone_level = 0.0;
                }
            }
            self.voice_captcha_type = None;
            self.voice_captcha_url = None;
            self.voice_log.push(raw.into());
            if unexpected {
                self.voice_reconnect_requested = true;
                self.voice_reconnect_pending = false;
                self.voice_reconnect_preserve_media = preserve_media;
                if preserve_media {
                    self.voice_log.push(
                        "Signaling dropped during the call; keeping audio active while reconnecting."
                            .into(),
                    );
                }
                self.schedule_voice_reconnect();
            }
        } else if let Ok(event) = serde_json::from_str::<Value>(raw) {
            let event_type = event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let peer_event_matches = self.voice_session.matches_current_peer(&event);
            let resumed_registration = event_type == "registered"
                && event.get("success").and_then(Value::as_bool) != Some(false)
                && self.voice_reconnect_preserve_media
                && self.voice_in_call;
            if !resumed_registration {
                let _ = self.voice_session.on_event(&event);
            }
            let _ = self
                .worker
                .commands
                .send(Command::VoiceEvent(event.clone()));
            match event_type {
                "registered" => {
                    let success = event
                        .get("success")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if success {
                        self.voice_reconnect_cancel.store(true, Ordering::Relaxed);
                        self.voice_reconnect_generation
                            .fetch_add(1, Ordering::Relaxed);
                        self.voice_reconnect_requested = false;
                        self.voice_reconnect_pending = false;
                        self.voice_reconnect_attempt = 0;
                        if resumed_registration {
                            self.voice_log
                                .push("Voice signaling restored; call media stayed active".into());
                        }
                        self.voice_reconnect_preserve_media = false;
                    }
                    if !success {
                        self.voice_connected = false;
                        self.voice_searching = false;
                        self.voice_in_call = false;
                        self.voice_microphone_active = false;
                        self.voice_reconnect_preserve_media = false;
                    }
                    self.voice_log.push(if success {
                        "Voice registration accepted".into()
                    } else {
                        format!(
                            "Voice registration rejected{}",
                            event
                                .get("errorCode")
                                .and_then(Value::as_i64)
                                .map_or(String::new(), |code| format!(" (code {code})"))
                        )
                    });
                    if success && self.voice_reconnect_search {
                        self.voice_reconnect_search = false;
                        let _ = self.worker.commands.send(self.voice_search_command(None));
                    }
                }
                "search.success" => {
                    self.voice_searching = true;
                    self.voice_captcha_visible = false;
                    self.voice_captcha_focus_requested = false;
                    self.voice_captcha_submitted = false;
                    self.voice_wrong_captcha_streak = 0;
                    self.voice_captcha_type = None;
                    self.voice_captcha_url = None;
                }
                "search.out" | "search.stop" => {
                    self.voice_searching = false;
                }
                "peer-connect" => {
                    if event
                        .get("connectionId")
                        .and_then(Value::as_str)
                        .is_none_or(|value| value.trim().is_empty())
                    {
                        return;
                    }
                    self.voice_searching = false;
                    self.voice_in_call = true;
                    self.voice_mic_test = false;
                    self.voice_peer_muted = false;
                    #[cfg(feature = "webrtc-media")]
                    crate::webrtc_backend::reset_audio_signal_quality();
                }
                "peer-connection" => {
                    if event.get("connectionId").is_some() && !peer_event_matches {
                        return;
                    }
                    if event.get("connection") == Some(&Value::Bool(false)) {
                        self.voice_in_call = false;
                        self.voice_microphone_active = false;
                    } else {
                        self.voice_in_call = true;
                    }
                }
                "stream-received" => {
                    if peer_event_matches {
                        self.voice_in_call = true;
                    }
                }
                "peer-disconnect" | "peer-soft-disconnect" => {
                    if !peer_event_matches {
                        return;
                    }
                    if !self.voice_auto_next {
                        // The server may keep the roulette scan alive after a
                        // peer leaves and does not always emit search.stop.
                        // Keep the local state machine aligned with the
                        // explicit stop request below.
                        let _ = self.voice_session.stop_search();
                    }
                    if event_type == "peer-soft-disconnect" && !self.voice_auto_next {
                        // The server keeps the roulette scan alive after a
                        // soft disconnect; stop it explicitly when the user
                        // did not request auto-next.
                        let _ = self.worker.commands.send(Command::VoiceStop);
                    }
                    self.voice_searching = self.voice_auto_next;
                    self.voice_in_call = false;
                    self.voice_microphone_active = false;
                    self.voice_muted = false;
                    self.voice_peer_muted = false;
                    if self.voice_auto_next {
                        let _ = self.worker.commands.send(self.voice_search_command(None));
                    }
                }
                "peer-mute" => {
                    if peer_event_matches {
                        self.voice_peer_muted =
                            event.get("muted").and_then(Value::as_bool).unwrap_or(false)
                    }
                }
                "users-count" => {
                    self.voice_users = event.get("usersCount").and_then(Value::as_i64).unwrap_or(0);
                    self.voice_waiting = event
                        .get("waitingUsersCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.voice_talking = event
                        .get("talkingUsersCount")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.voice_log.push(format!(
                        "Online: {} · waiting: {} · talking: {}",
                        self.voice_users, self.voice_waiting, self.voice_talking
                    ));
                }
                "captcha-request" => {
                    let was_submitted = self.voice_captcha_submitted;
                    self.voice_captcha_submitted = false;
                    if was_submitted {
                        self.voice_wrong_captcha_streak =
                            self.voice_wrong_captcha_streak.saturating_add(1);
                        if self.voice_wrong_captcha_streak >= 2 {
                            self.voice_log.push(
                                "Captcha was rejected twice; rotating the voice route.".into(),
                            );
                            self.recover_voice_route(
                                self.voice_searching || self.voice_auto_next,
                                true,
                            );
                            return;
                        }
                    }
                    self.voice_searching = false;
                    self.voice_captcha_visible = true;
                    self.voice_captcha_focus_requested = true;
                    self.voice_captcha_type = event
                        .get("captchaType")
                        .or_else(|| event.get("tokenType"))
                        .and_then(Value::as_str)
                        .map(normalize_captcha_type)
                        .or_else(|| {
                            event
                                .get("url")
                                .and_then(Value::as_str)
                                .filter(|url| !url.trim().is_empty())
                                .map_or_else(|| Some("RECAPTCHA".into()), |_| Some("IMAGE".into()))
                        });
                    self.voice_captcha_url = event
                        .get("url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.trim().is_empty())
                        .map(str::to_owned);
                    if let Some(url) = self.voice_captcha_url.clone()
                        && let Ok(absolute_url) = captcha_url(
                            &url,
                            self.voice_server_override
                                .then_some(self.voice_server_host.as_str()),
                            self.voice_web_mode,
                        )
                    {
                        self.voice_captcha_url = Some(absolute_url);
                    }
                    self.voice_captcha_image = None;
                    self.voice_captcha_texture = None;
                    if let Some(url) = self.voice_captcha_url.clone() {
                        let _ = self.worker.commands.send(Command::VoiceFetchCaptcha {
                            url,
                            proxy_url: self.active_voice_proxy_entry(),
                            server_host: if self.voice_server_override {
                                self.voice_server_host.clone()
                            } else {
                                String::new()
                            },
                            web_mode: self.voice_web_mode,
                            user_agent: if self.voice_web_mode {
                                VoiceTransportClient::DEFAULT_WEB_USER_AGENT.into()
                            } else if self.voice_android_user_agent.trim().is_empty() {
                                crate::transport::VoiceTransportClient::DEFAULT_ANDROID_USER_AGENT
                                    .into()
                            } else {
                                self.voice_android_user_agent.clone()
                            },
                            locale: (!self.voice_web_mode
                                && !self.voice_android_locale.trim().is_empty())
                            .then_some(self.voice_android_locale.clone()),
                        });
                    }
                    self.voice_captcha_left = event
                        .get("leftChats")
                        .or_else(|| event.get("left"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.voice_captcha_need = event
                        .get("needChats")
                        .or_else(|| event.get("need"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.voice_captcha_solution.clear();
                    self.voice_log
                        .push("Voice search paused: captcha required.".into());
                }
                "ban" => {
                    self.voice_captcha_submitted = false;
                    self.voice_wrong_captcha_streak = 0;
                    let resume_search = self.voice_searching || self.voice_auto_next;
                    self.voice_searching = false;
                    self.voice_in_call = false;
                    self.voice_log.push(format!(
                        "Voice access banned{}",
                        event
                            .pointer("/banInfo/text")
                            .or_else(|| event.get("text"))
                            .and_then(Value::as_str)
                            .map_or(String::new(), |text| format!(": {text}"))
                    ));
                    self.recover_voice_route(resume_search, false);
                }
                "error" => {
                    let (code, description) = voice_error_details(&event);
                    let resume_search = self.voice_searching || self.voice_auto_next;
                    self.voice_searching = false;
                    self.voice_in_call = false;
                    let description_lower =
                        description.as_deref().unwrap_or_default().to_lowercase();
                    let is_ban = code.as_deref() == Some("201")
                        || description_lower.contains("banned")
                        || description_lower.contains("blocked")
                        || description_lower.contains("заблокирован");
                    if is_ban {
                        let _ = self
                            .worker
                            .commands
                            .send(Command::VoiceResetIdentity(self.voice_web_mode));
                        self.voice_log.push(format!(
                            "Voice route rejected{}; resetting identity and rotating route.",
                            code.as_deref()
                                .map_or(String::new(), |code| format!(" (code {code})"))
                        ));
                        self.recover_voice_route(resume_search, false);
                        return;
                    }
                    self.voice_log.push(format!(
                        "Voice error{}",
                        description.map_or(String::new(), |text| format!(": {text}"))
                    ));
                }
                "purchase-changed" => self.voice_log.push(format!(
                    "Voice account status changed{}",
                    event
                        .get("paidType")
                        .and_then(Value::as_i64)
                        .map_or(String::new(), |paid| format!(" (paid type {paid})"))
                )),
                _ => {}
            }
            if matches!(
                self.voice_session.state(),
                VoiceState::InCall | VoiceState::Muted
            ) {
                self.voice_searching = false;
            }
            self.voice_log.push(format!("event: {event_type}"));
        } else {
            self.voice_log.push(raw.into());
        }
        if self.voice_log.len() > 500 {
            self.voice_log.remove(0);
        }
    }

    fn add_message(&mut self, data: &Value) -> bool {
        let Some(mut message) = protocol::parse_message(data) else {
            return false;
        };
        let message_id = message.id.0;
        let message_dialog_id = data
            .get("dialogId")
            .and_then(Value::as_i64)
            .filter(|value| *value != 0);
        if let (Some(current), Some(message_dialog)) = (self.dialog_id, message_dialog_id)
            && current != message_dialog
        {
            return false;
        }
        if self.dialog_id.is_none() {
            self.dialog_id = message_dialog_id;
        }
        let dialog_id = self.dialog_id.or(message_dialog_id).unwrap_or(0);
        if self.dialog.is_none() {
            self.dialog = Some(Dialog::new(dialog_id, Vec::new(), false, None, None));
        }
        if message.dialog_id.0 == 0 && dialog_id != 0 {
            message.dialog_id = crate::domain::DialogId(dialog_id);
        }
        if !self.message_ids.insert(message_id) {
            return false;
        }
        if !self
            .dialog
            .as_mut()
            .and_then(|dialog| dialog.try_add_message(message.clone()).ok())
            .unwrap_or(false)
        {
            self.message_ids.remove(&message_id);
            return false;
        }
        if self
            .last_message_id
            .is_none_or(|last_message_id| message_id > last_message_id)
        {
            self.last_message_id = Some(message_id);
        }
        let own_by_random_id = data
            .get("randomId")
            .and_then(Value::as_i64)
            .is_some_and(|random_id| self.pending_random_ids.remove(&random_id));
        let own =
            own_by_random_id || data.get("senderId").and_then(Value::as_i64) == self.own_user_id;
        let line = ChatLine::from_message(&message, own);
        self.messages.push(line);
        true
    }

    fn send_message(&mut self) {
        if let Some(dialog_id) = self.dialog_id {
            let text = self.input.trim().to_owned();
            if !text.is_empty() {
                let random_id = rand::random();
                self.pending_random_ids.insert(random_id);
                let _ = self.worker.commands.send(Command::Send(protocol::message(
                    dialog_id, &text, random_id,
                )));
                self.input.clear();
            }
        }
    }

    fn verify_captcha(&mut self) {
        let solution = self.captcha_solution.trim().to_owned();
        if !solution.is_empty() {
            let _ = self.worker.commands.send(Command::Captcha(solution));
            self.captcha_solution.clear();
            self.captcha_visible = false;
        }
    }

    fn parse_voice_age(from: &str, to: &str) -> Option<(i32, i32)> {
        let from = from.trim().parse().ok()?;
        let to = to.trim().parse().ok()?;
        (from <= to).then_some((from, to))
    }

    fn parse_voice_age_ranges(from: &str, to: &str) -> Vec<(i32, i32)> {
        if !to.trim().is_empty() {
            return Self::parse_voice_age(from, to).into_iter().collect();
        }
        from.split([',', ';'])
            .filter_map(|range| {
                let (start, end) = range
                    .trim()
                    .split_once('-')
                    .or_else(|| range.trim().split_once('–'))
                    .or_else(|| range.trim().split_once('—'))?;
                Self::parse_voice_age(start, end)
            })
            .collect()
    }

    fn selected_user_age_preset(&self) -> Option<VoiceAgePreset> {
        let (from, to) = Self::parse_voice_age(&self.voice_user_age_from, &self.voice_user_age_to)?;
        VoiceAgePreset::ALL
            .into_iter()
            .find(|preset| preset.matches_range(from, to))
    }

    fn is_peer_age_preset_selected(&self, preset: VoiceAgePreset) -> bool {
        let ranges =
            Self::parse_voice_age_ranges(&self.voice_peer_age_from, &self.voice_peer_age_to);
        ranges
            .iter()
            .any(|&(from, to)| preset.matches_range(from, to))
    }

    fn set_user_age_preset(&mut self, preset: Option<VoiceAgePreset>) {
        if let Some(p) = preset {
            let (from, to) = p.range();
            self.voice_user_age_from = from.to_string();
            self.voice_user_age_to = to.to_string();
            if p != VoiceAgePreset::Under18
                && self.is_peer_age_preset_selected(VoiceAgePreset::Under18)
            {
                self.remove_peer_age_preset(VoiceAgePreset::Under18);
            }
        } else {
            self.voice_user_age_from.clear();
            self.voice_user_age_to.clear();
        }
    }

    fn toggle_peer_age_preset(&mut self, preset: VoiceAgePreset) {
        let mut selected: Vec<VoiceAgePreset> = VoiceAgePreset::ALL
            .into_iter()
            .filter(|&p| self.is_peer_age_preset_selected(p))
            .collect();

        if let Some(pos) = selected.iter().position(|&p| p == preset) {
            selected.remove(pos);
        } else {
            selected.push(preset);
            if preset == VoiceAgePreset::Under18
                && self.selected_user_age_preset() != Some(VoiceAgePreset::Under18)
            {
                let (from, to) = VoiceAgePreset::Under18.range();
                self.voice_user_age_from = from.to_string();
                self.voice_user_age_to = to.to_string();
            }
        }
        selected.sort();
        self.apply_peer_age_presets(&selected);
    }

    fn remove_peer_age_preset(&mut self, preset: VoiceAgePreset) {
        let mut selected: Vec<VoiceAgePreset> = VoiceAgePreset::ALL
            .into_iter()
            .filter(|&p| self.is_peer_age_preset_selected(p) && p != preset)
            .collect();
        selected.sort();
        self.apply_peer_age_presets(&selected);
    }

    fn apply_peer_age_presets(&mut self, selected: &[VoiceAgePreset]) {
        self.voice_peer_age_to.clear();
        if selected.is_empty() {
            self.voice_peer_age_from.clear();
        } else {
            self.voice_peer_age_from = selected
                .iter()
                .map(|p| {
                    let (from, to) = p.range();
                    format!("{from}-{to}")
                })
                .collect::<Vec<_>>()
                .join(", ");
        }
    }

    fn start_voice_search(&mut self) {
        if !self.voice_connected || self.voice_in_call {
            return;
        }
        self.voice_searching = true;
        self.voice_captcha_visible = false;
        self.voice_captcha_focus_requested = false;
        self.voice_captcha_submitted = false;
        self.voice_wrong_captcha_streak = 0;
        self.voice_captcha_type = None;
        self.voice_captcha_url = None;

        let user_sex_norm = crate::persistence::normalize_voice_sex_filter(&self.voice_user_sex);
        let desc = if user_sex_norm == "ANY" {
            "Any / Anyone (fast match)".to_string()
        } else {
            let mut parts = vec![format!("I am: {}", self.voice_user_sex)];
            if let Some((from, to)) =
                Self::parse_voice_age(&self.voice_user_age_from, &self.voice_user_age_to)
            {
                parts.push(format!("({from}-{to})"));
            }
            parts.push(format!("Looking for: {}", self.voice_peer_sex));
            let peer_ranges =
                Self::parse_voice_age_ranges(&self.voice_peer_age_from, &self.voice_peer_age_to);
            if !peer_ranges.is_empty() && peer_ranges.len() < 4 {
                let ranges_str = peer_ranges
                    .iter()
                    .map(|(f, t)| format!("{f}-{t}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                parts.push(format!("Ages: {ranges_str}"));
            }
            parts.join(" ")
        };
        self.voice_log
            .push(format!("Searching for a peer ({desc})…"));
        let _ = self.worker.commands.send(self.voice_search_command(None));
    }

    fn stop_voice_search(&mut self) {
        self.voice_searching = false;
        self.voice_captcha_visible = false;
        self.voice_captcha_submitted = false;
        self.voice_log.push("Search stopped".into());
        let _ = self.worker.commands.send(Command::VoiceStop);
    }

    fn voice_search_command(&self, token: Option<String>) -> Command {
        let mut peer_ages =
            Self::parse_voice_age_ranges(&self.voice_peer_age_from, &self.voice_peer_age_to);
        if peer_ages.len() >= 4 {
            peer_ages.clear();
        }
        let mut user_age =
            Self::parse_voice_age(&self.voice_user_age_from, &self.voice_user_age_to);
        let user_sex = crate::persistence::normalize_voice_sex_filter(&self.voice_user_sex);
        let mut peer_sex = crate::persistence::normalize_voice_sex_filter(&self.voice_peer_sex);
        if user_sex == "ANY" {
            peer_sex = "ANY".into();
            peer_ages.clear();
            user_age = None;
        } else if user_age.is_none() {
            user_age = if peer_ages.first().is_some_and(|&(_, to)| to <= 17) {
                Some((0, 17))
            } else {
                Some((18, 24))
            };
        }
        Command::VoiceSearch {
            user_sex,
            peer_sex,
            user_age,
            peer_ages,
            token,
            token_type: self.voice_captcha_type.clone(),
            preferences: self.voice_preferences(),
        }
    }

    fn voice_connect_command(&self) -> Command {
        self.voice_connect_command_with_media(false)
    }

    fn voice_connect_command_with_media(&self, preserve_media: bool) -> Command {
        Command::VoiceConnect {
            web_mode: self.voice_web_mode,
            proxy_url: self.selected_voice_proxy(),
            server_host: if self.voice_server_override {
                self.voice_server_host.trim().to_owned()
            } else {
                String::new()
            },
            preserve_media,
        }
    }

    fn apply_web_token_and_search(&mut self) {
        let token = self
            .voice_web_token
            .trim_matches(['"', '\'', ' ', '\t', '\r', '\n'])
            .to_owned();
        if token.is_empty() {
            self.voice_log.push("Web token cannot be empty.".into());
            return;
        }
        self.voice_web_token = token;
        self.voice_captcha_visible = false;
        self.voice_captcha_solution.clear();
        self.voice_reconnect_search = true;
        let _ = self
            .worker
            .commands
            .send(Command::SaveWebToken(self.voice_web_token.clone()));
        if self.voice_connected {
            let _ = self.worker.commands.send(Command::VoiceDisconnect);
        }
        let _ = self.worker.commands.send(self.voice_connect_command());
        self.voice_connected = false;
        self.voice_searching = false;
        self.voice_in_call = false;
        self.voice_log
            .push("Web token applied; connecting and starting voice search…".into());
    }

    fn selected_voice_proxy(&self) -> Option<String> {
        select_proxy_entry(&self.voice_proxy_url, self.voice_proxy_index)
    }

    fn active_voice_proxy_entry(&self) -> Option<String> {
        let entries = crate::transport::proxy_entries(&self.voice_proxy_url);
        entries
            .get(self.voice_proxy_index.min(entries.len().saturating_sub(1)))
            .cloned()
    }

    fn rotate_voice_proxy(&mut self, direction: isize) {
        let entries = crate::transport::proxy_entries(&self.voice_proxy_url);
        if entries.len() < 2 {
            self.voice_log
                .push("Proxy pool needs at least two entries to rotate.".into());
            return;
        }
        self.voice_proxy_index = (self.voice_proxy_index as isize + direction)
            .rem_euclid(entries.len() as isize) as usize;
        self.voice_log.push(format!(
            "Proxy selected: {}/{}.",
            self.voice_proxy_index + 1,
            entries.len()
        ));
        if self.voice_connected {
            let _ = self.worker.commands.send(Command::VoiceDisconnect);
            let _ = self.worker.commands.send(self.voice_connect_command());
            self.voice_connected = false;
            self.voice_searching = false;
            self.voice_in_call = false;
            self.voice_log
                .push("Reconnecting through selected proxy…".into());
        }
    }

    fn recover_voice_route(&mut self, resume_search: bool, reset_identity: bool) {
        let entries = crate::transport::proxy_entries(&self.voice_proxy_url);
        if entries.len() < 2 {
            self.voice_log.push(
                "Voice route is unhealthy; reconnect manually or configure a proxy pool.".into(),
            );
            return;
        }
        self.voice_proxy_index = (self.voice_proxy_index + 1) % entries.len();
        self.voice_reconnect_search = resume_search;
        if reset_identity {
            let _ = self
                .worker
                .commands
                .send(Command::VoiceResetIdentity(self.voice_web_mode));
        }
        let _ = self.worker.commands.send(Command::VoiceDisconnect);
        let _ = self.worker.commands.send(self.voice_connect_command());
        self.voice_connected = false;
        self.voice_searching = false;
        self.voice_in_call = false;
        self.voice_log.push(format!(
            "Voice route banned; rotating to proxy {}/{} and reconnecting…",
            self.voice_proxy_index + 1,
            entries.len()
        ));
    }

    fn refresh_voice_captcha(&mut self) {
        let search = self.voice_search_command(None);
        let commands = self.worker.commands.clone();
        let _ = commands.send(Command::VoiceStop);
        self.voice_captcha_visible = false;
        self.voice_captcha_focus_requested = false;
        self.voice_captcha_submitted = false;
        self.voice_captcha_solution.clear();
        self.voice_captcha_type = None;
        self.voice_captcha_url = None;
        self.voice_captcha_left = 0;
        self.voice_captcha_need = 0;
        self.voice_log
            .push("Requesting a fresh captcha challenge…".into());
        thread::spawn(move || {
            thread::sleep(std::time::Duration::from_millis(400));
            let _ = commands.send(search);
        });
    }

    fn submit_voice_captcha(&mut self) {
        let token = self.voice_captcha_solution.trim().to_owned();
        if token.is_empty() || self.voice_captcha_submitted {
            return;
        }
        self.voice_captcha_submitted = true;
        let _ = self
            .worker
            .commands
            .send(self.voice_search_command(Some(token)));
        self.voice_log
            .push("Captcha answer submitted; waiting for verification…".into());
    }

    fn refresh_saved_recordings(&mut self) {
        match crate::session_recorder::saved_recordings() {
            Ok(recordings) => self.voice_saved_recordings = recordings,
            Err(error) => self
                .voice_log
                .push(format!("Recording library refresh failed: {error}")),
        }
    }

    fn show_recording_library(&mut self, ui: &mut egui::Ui) {
        ui.collapsing(
            format!("Recording library ({})", self.voice_saved_recordings.len()),
            |ui| {
                if ui.small_button("Refresh recordings").clicked() {
                    self.refresh_saved_recordings();
                }
                if self.voice_saved_recordings.is_empty() {
                    ui.small("No finalized NektoMe recordings found in Downloads.");
                    return;
                }
                let mut refresh = false;
                for recording in self.voice_saved_recordings.clone() {
                    let name = recording.path.file_name().map_or_else(
                        || recording.path.display().to_string(),
                        |name| name.to_string_lossy().into_owned(),
                    );
                    let modified: chrono::DateTime<Local> = recording.modified.into();
                    ui.group(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(name);
                            ui.small(format!(
                                "{} · {} · {}",
                                recording.format.label(),
                                human_file_size(recording.bytes),
                                modified.format("%Y-%m-%d %H:%M")
                            ));
                        });
                        ui.horizontal_wrapped(|ui| {
                            if ui.small_button("Play").clicked()
                                && let Err(error) = open_external_path(&recording.path, false)
                            {
                                self.voice_log
                                    .push(format!("Could not open recording: {error}"));
                            }
                            if ui.small_button("Reveal").clicked()
                                && let Err(error) = open_external_path(&recording.path, true)
                            {
                                self.voice_log
                                    .push(format!("Could not reveal recording: {error}"));
                            }
                            if self.voice_recording_delete_pending.as_ref() == Some(&recording.path)
                            {
                                if ui.small_button("Confirm delete").clicked() {
                                    match crate::session_recorder::delete_saved_recording(
                                        &recording.path,
                                    ) {
                                        Ok(()) => {
                                            self.voice_log.push(format!(
                                                "Deleted recording {}",
                                                recording.path.display()
                                            ));
                                            refresh = true;
                                        }
                                        Err(error) => self
                                            .voice_log
                                            .push(format!("Could not delete recording: {error}")),
                                    }
                                    self.voice_recording_delete_pending = None;
                                }
                                if ui.small_button("Cancel").clicked() {
                                    self.voice_recording_delete_pending = None;
                                }
                            } else if ui.small_button("Delete…").clicked() {
                                self.voice_recording_delete_pending = Some(recording.path.clone());
                            }
                        });
                    });
                }
                if refresh {
                    self.refresh_saved_recordings();
                }
            },
        );
    }

    fn diagnostics_bundle(&self) -> Value {
        #[cfg(feature = "webrtc-media")]
        let audio = {
            let quality = crate::webrtc_backend::audio_signal_quality();
            serde_json::json!({
                "packets": quality.packets,
                "lossPercent": quality.loss_percent,
                "jitterMs": quality.jitter_ms,
                "decodeErrorPercent": quality.decode_error_percent,
                "lastPacketAgeMs": quality.last_packet_age_ms,
                "playoutDelayMs": quality.playout_delay_ms,
                "rtpUnderruns": quality.playout_underruns,
                "concealedFrames": quality.concealed_frames,
                "speakerUnderruns": quality.speaker_underruns,
            })
        };
        #[cfg(not(feature = "webrtc-media"))]
        let audio = Value::Null;
        let devices = crate::audio::audio_devices();
        serde_json::json!({
            "schemaVersion": 1,
            "exportedAt": Local::now().to_rfc3339(),
            "application": {
                "name": env!("CARGO_PKG_NAME"),
                "version": env!("CARGO_PKG_VERSION"),
                "targetOs": std::env::consts::OS,
                "targetArch": std::env::consts::ARCH,
                "features": {
                    "webrtcMedia": cfg!(feature = "webrtc-media"),
                    "opusMedia": cfg!(feature = "opus-media"),
                    "nativeAudio": cfg!(feature = "cpal-audio"),
                }
            },
            "connection": {
                "chatConnected": self.connected,
                "chatSearching": self.searching,
                "voiceConnected": self.voice_connected,
                "voiceSearching": self.voice_searching,
                "voiceInCall": self.voice_in_call,
                "voiceWebMode": self.voice_web_mode,
                "voiceServer": self.voice_server_host,
                "voiceReconnectPending": self.voice_reconnect_pending,
                "voiceReconnectAttempt": self.voice_reconnect_attempt,
                "voiceTransportFailureStreak": self.voice_transport_failure_streak,
            },
            "audio": {
                "signal": audio,
                "microphoneActive": self.voice_microphone_active,
                "microphoneMuted": self.voice_muted,
                "speakerMuted": self.voice_speaker_muted,
                "pushToTalk": self.voice_push_to_talk,
                "microphoneGainPercent": self.voice_microphone_gain,
                "speakerGainPercent": self.voice_speaker_gain,
                "microphoneDsp": self.voice_microphone_dsp,
                "microphoneNoiseSuppression": self.voice_microphone_noise_suppression,
                "speakerDsp": self.voice_speaker_dsp,
                "echoSuppression": self.voice_echo_cancellation,
                "selectedMicrophone": self.voice_microphone_device,
                "selectedSpeaker": self.voice_speaker_device,
                "availableMicrophones": devices.inputs,
                "availableSpeakers": devices.outputs,
                "defaultMicrophone": devices.default_input,
                "defaultSpeaker": devices.default_output,
            },
            "recording": {
                "enabled": self.voice_record_sessions,
                "format": self.voice_recording_format.label(),
                "savedCount": self.voice_saved_recordings.len(),
                "mp3RateMode": self.voice_recording_parameters.mp3_rate_mode.label(),
                "mp3BitrateKbps": self.voice_recording_parameters.mp3_bitrate_kbps,
                "mp3VbrQuality": self.voice_recording_parameters.mp3_vbr_quality,
                "opusRateMode": self.voice_recording_parameters.opus_rate_mode.label(),
                "opusBitrateKbps": self.voice_recording_parameters.opus_bitrate_kbps,
                "opusComplexity": self.voice_recording_parameters.opus_complexity,
            },
            "logs": {
                "textChat": self.log,
                "voiceRoulette": self.voice_log,
            }
        })
    }

    fn export_diagnostics_bundle(&self) -> Result<std::path::PathBuf, String> {
        let directory = crate::session_recorder::downloads_directory()?;
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let stamp = Local::now().format("%Y-%m-%d_%H-%M-%S");
        let mut path = directory.join(format!("NektoMe-diagnostics-{stamp}.json"));
        if path.exists() {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            path = directory.join(format!("NektoMe-diagnostics-{stamp}-{}.json", &suffix[..8]));
        }
        let bytes = serde_json::to_vec_pretty(&self.diagnostics_bundle())
            .map_err(|error| error.to_string())?;
        std::fs::write(&path, bytes).map_err(|error| error.to_string())?;
        Ok(path)
    }

    pub fn channel_diagnostics_text(&self) -> String {
        let mut out = String::new();
        out.push_str("=== NektoMe Voice Channel Diagnostics ===\n");
        out.push_str(&format!(
            "Time: {}\n",
            Local::now().format("%Y-%m-%d %H:%M:%S")
        ));
        out.push_str(&format!(
            "App: {} v{} ({}-{})\n",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        ));
        out.push_str(&format!(
            "Channel Mode: {} (server: {})\n",
            if self.voice_web_mode {
                "Web"
            } else {
                "Android"
            },
            self.voice_server_host,
        ));
        let proxy_status = if self.voice_proxy_url.trim().is_empty() {
            "direct (no proxy)".to_owned()
        } else {
            let proxies = crate::transport::proxy_entries(&self.voice_proxy_url);
            format!(
                "configured ({} entries, active: #{})",
                proxies.len(),
                self.voice_proxy_index + 1
            )
        };
        out.push_str(&format!("Proxy: {proxy_status}\n"));
        out.push_str(&format!(
            "Connection: signaling={}, searching={}, in_call={}, failure_streak={}\n",
            if self.voice_connected {
                "connected"
            } else {
                "disconnected"
            },
            self.voice_searching,
            self.voice_in_call,
            self.voice_transport_failure_streak,
        ));
        #[cfg(feature = "webrtc-media")]
        {
            let quality = crate::webrtc_backend::audio_signal_quality();
            let state_str = match quality.state() {
                crate::webrtc_backend::AudioSignalState::Waiting => "Waiting",
                crate::webrtc_backend::AudioSignalState::Good => "Good",
                crate::webrtc_backend::AudioSignalState::Fair => "Fair",
                crate::webrtc_backend::AudioSignalState::Poor => "Poor",
                crate::webrtc_backend::AudioSignalState::Interrupted => "Interrupted",
            };
            out.push_str(&format!(
                "Audio Signal: {} (loss: {:.1}%, jitter: {:.0} ms, delay: {} ms, RTP underruns: {}, dev underruns: {})\n",
                state_str,
                quality.loss_percent,
                quality.jitter_ms,
                quality.playout_delay_ms,
                quality.playout_underruns,
                quality.speaker_underruns,
            ));
        }
        let devices = crate::audio::audio_devices();
        let mic_name = if self.voice_microphone_device.trim().is_empty() {
            devices.default_input.as_deref().unwrap_or("default")
        } else {
            self.voice_microphone_device.trim()
        };
        let speaker_name = if self.voice_speaker_device.trim().is_empty() {
            devices.default_output.as_deref().unwrap_or("default")
        } else {
            self.voice_speaker_device.trim()
        };
        out.push_str(&format!(
            "Microphone: {} (gain: {:.0}%, dsp: {}, ns: {}, echo: {})\n",
            mic_name,
            self.voice_microphone_gain,
            self.voice_microphone_dsp,
            self.voice_microphone_noise_suppression,
            self.voice_echo_cancellation,
        ));
        out.push_str(&format!(
            "Speaker: {} (gain: {:.0}%, dsp: {})\n",
            speaker_name, self.voice_speaker_gain, self.voice_speaker_dsp,
        ));
        out.push_str("\n--- Useful Channel Inputs & Settings ---\n");
        out.push_str("• Alternate Server: switch to 'audio.nekto-me.kz' (More ▾ menu) if audio.nekto.me resets\n");
        out.push_str("• Voice Proxy: set 'http://host:port' or 'socks5://host:port' in Settings -> Voice proxy\n");
        out.push_str(
            "• Web Token: click 'Import from Chrome' in Settings if web session expired\n",
        );
        out.push_str("• Echo Suppression: uncheck in Settings if using headphones/AirPods to avoid ducking\n");
        out.push_str("• Audio Codec: can force via env var NEKTOME_AUDIO_CODEC=opus or pcmu\n");
        out.push_str("=========================================\n");
        out
    }

    fn voice_preferences(&self) -> VoicePreferences {
        VoicePreferences {
            proxy_seller_api_key: self.voice_proxy_seller_api_key.trim().into(),
            proxy_seller_list_name: self.voice_proxy_seller_list_name.trim().into(),
            proxy_seller_ports_count: self.voice_proxy_seller_ports_count,
            proxy_seller_rotation_seconds: self.voice_proxy_seller_rotation_seconds,
            proxy_seller_whitelist_ip: self.voice_proxy_seller_whitelist_ip.trim().into(),
            web_token: self.voice_web_token.trim().into(),
            web_mode: self.voice_web_mode,
            voice_server_host: self.voice_server_host.trim().into(),
            voice_server_override: self.voice_server_override,
            auto_next: self.voice_auto_next,
            proxy_url: self.voice_proxy_url.trim().into(),
            android_user_agent: self.voice_android_user_agent.trim().into(),
            android_locale: self.voice_android_locale.trim().into(),
            android_time_zone: self.voice_android_time_zone.trim().into(),
            remote_audio_wav_path: self.voice_remote_audio_wav_path.trim().into(),
            record_sessions: self.voice_record_sessions,
            recording_format: self.voice_recording_format,
            recording_parameters: self.voice_recording_parameters,
            microphone_device_name: self.voice_microphone_device.trim().into(),
            speaker_device_name: self.voice_speaker_device.trim().into(),
            push_to_talk: self.voice_push_to_talk,
            speaker_gain_percent: self.voice_speaker_gain,
            speaker_dsp_enabled: self.voice_speaker_dsp,
            microphone_dsp_enabled: self.voice_microphone_dsp,
            microphone_noise_suppression_enabled: self.voice_microphone_noise_suppression,
            microphone_gain_percent: self.voice_microphone_gain,
            echo_cancellation_enabled: self.voice_echo_cancellation,
            noise_gate_threshold: self.voice_noise_gate_threshold,
            compressor_threshold: self.voice_compressor_threshold,
            compressor_ratio: self.voice_compressor_ratio,
            makeup_gain: self.voice_makeup_gain,
            microphone_warmth_enabled: self.voice_microphone_warmth,
            saturation_drive: self.voice_saturation_drive,
            saturation_asymmetry: self.voice_saturation_asymmetry,
            saturation_bias: self.voice_saturation_bias,
            saturation_profile: self.voice_saturation_profile,
            character_profile: self.voice_character_profile,
            presence_boost_db: self.voice_presence_boost_db,
            proximity_gain_db: self.voice_proximity_gain_db,
            air_boost_db: self.voice_air_boost_db,
            voice_changer_enabled: self.voice_changer_enabled,
            voice_changer_model: self.voice_changer_model.clone(),
            voice_changer_pitch: self.voice_changer_pitch,
            voice_changer_wet: self.voice_changer_wet,
            user_sex: crate::persistence::normalize_voice_sex_filter(&self.voice_user_sex),
            peer_sex: crate::persistence::normalize_voice_sex_filter(&self.voice_peer_sex),
            user_age_from: self.voice_user_age_from.trim().into(),
            user_age_to: self.voice_user_age_to.trim().into(),
            peer_age_from: self.voice_peer_age_from.trim().into(),
            peer_age_to: self.voice_peer_age_to.trim().into(),
        }
    }

    fn show_text_toolbar(&mut self, ui: &mut egui::Ui) {
        if self.connected {
            if ui.button("Disconnect").clicked() {
                self.connected = false;
                self.chat_reconnect_requested = false;
                self.chat_reconnect_pending = false;
                self.chat_reconnect_cancel.store(true, Ordering::Relaxed);
                let _ = self.worker.commands.send(Command::Disconnect);
            }
        } else if ui.button("Connect").clicked() {
            self.chat_reconnect_requested = false;
            self.chat_reconnect_pending = false;
            self.chat_reconnect_cancel.store(true, Ordering::Relaxed);
            let _ = self.worker.commands.send(Command::Connect);
        }

        if self.searching {
            if ui.button("Stop search").clicked() {
                let _ = self.worker.commands.send(Command::Stop);
            }
        } else if ui
            .add_enabled(
                self.connected && self.dialog_id.is_none(),
                egui::Button::new("Find partner"),
            )
            .clicked()
        {
            let _ = self.worker.commands.send(Command::Search);
        }

        if self.dialog_id.is_some() && ui.button("Leave dialog").clicked() {
            if let Some(dialog_id) = self.dialog_id {
                let _ = self.worker.commands.send(Command::Leave(dialog_id));
            }
            self.dialog_id = None;
            if let Some(dialog) = &mut self.dialog {
                dialog.close();
            }
            self.pending_random_ids.clear();
        }

        ui.menu_button("More ▾", |ui| {
            if ui
                .add_enabled(
                    self.dialog_id.is_some(),
                    egui::Button::new("Refresh dialog"),
                )
                .clicked()
                && let Some(dialog_id) = self.dialog_id
            {
                let _ = self
                    .worker
                    .commands
                    .send(Command::Send(protocol::dialog_info(dialog_id)));
                ui.close();
            }
            if ui
                .add_enabled(
                    self.dialog_id.is_some() && self.last_message_id.is_some(),
                    egui::Button::new("Report last message"),
                )
                .clicked()
                && let (Some(dialog_id), Some(message_id)) = (self.dialog_id, self.last_message_id)
            {
                let _ = self
                    .worker
                    .commands
                    .send(Command::Send(protocol::report_dialog(
                        dialog_id, message_id, 1,
                    )));
                self.log.push("Reported the latest message.".into());
                ui.close();
            }
            ui.separator();
            if ui.button("Forget saved token").clicked() {
                let _ = self.worker.commands.send(Command::ForgetToken);
                ui.close();
            }
        });
    }

    fn show_text_captcha(&mut self, ui: &mut egui::Ui) {
        if !self.captcha_visible {
            return;
        }

        ui.group(|ui| {
            ui.strong("Captcha required to continue");
            ui.label("Solve the challenge in Chrome, then return here. Search resumes automatically.");
            if self.captcha_need > 0 {
                ui.small(format!(
                    "Completed chats: {}/{}",
                    self.captcha_left, self.captcha_need
                ));
            }
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(
                        !self.captcha_browser_running,
                        egui::Button::new("Solve in Chrome"),
                    )
                    .clicked()
                {
                    self.captcha_browser_running = true;
                    self.log.push(
                        "Chrome opened. Press Find partner there and solve the captcha; this app will reconnect automatically."
                            .into(),
                    );
                    let _ = self.worker.commands.send(Command::SolveChatCaptcha);
                }
                if self.captcha_browser_running {
                    ui.spinner();
                    ui.label("Waiting for Chrome…");
                }
            });
            ui.collapsing("Enter a solution token manually", |ui| {
                ui.horizontal_wrapped(|ui| {
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut self.captcha_solution)
                            .hint_text("Solution token")
                            .desired_width(260.0),
                    );
                    let can_verify = !self.captcha_solution.trim().is_empty();
                    let verify_with_enter = response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    let verify_clicked = ui
                        .add_enabled(can_verify, egui::Button::new("Verify"))
                        .clicked();
                    if verify_clicked || (verify_with_enter && can_verify) {
                        self.verify_captcha();
                    }
                });
            });
        });
    }

    fn show_text_conversation(&mut self, ui: &mut egui::Ui) {
        let conversation_height = (ui.available_height() - 62.0).max(60.0);
        egui::ScrollArea::vertical()
            .id_salt("text-conversation")
            .max_height(conversation_height)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.messages.is_empty() {
                    ui.centered_and_justified(|ui| {
                        ui.weak(if self.dialog_id.is_some() {
                            "Connected. Say hello!"
                        } else if self.searching {
                            "Looking for someone to chat with…"
                        } else if self.connected {
                            "Select Find partner to start a chat."
                        } else {
                            "Connect to start chatting."
                        });
                    });
                } else {
                    for message in &self.messages {
                        let time = message.display_time();
                        let stamp = if time.is_empty() {
                            String::new()
                        } else {
                            format!(" [{time}]")
                        };
                        let read = if message.is_own && message.read {
                            " ✓"
                        } else {
                            ""
                        };
                        if message.is_own {
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                                ui.label(format!("You{stamp}{read}: {}", message.text));
                            });
                        } else {
                            ui.label(format!("Anon{stamp}: {}", message.text));
                        }
                    }
                }
            });

        if self.peer_typing {
            ui.small("Partner is typing…");
        } else {
            ui.add_space(ui.spacing().interact_size.y.min(18.0));
        }
        ui.horizontal(|ui| {
            let editor_width = (ui.available_width() - 72.0).max(100.0);
            let response = ui.add_enabled(
                self.dialog_id.is_some(),
                egui::TextEdit::singleline(&mut self.input)
                    .hint_text("Type a message…")
                    .desired_width(editor_width),
            );
            if response.changed()
                && let Some(dialog_id) = self.dialog_id
            {
                let _ = self.worker.commands.send(Command::Typing {
                    dialog_id,
                    typing: !self.input.trim().is_empty(),
                });
            }
            if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                self.send_message();
            }
            if ui
                .add_enabled(self.dialog_id.is_some(), egui::Button::new("Send"))
                .clicked()
            {
                self.send_message();
            }
        });
    }

    fn show_text_chat(&mut self, ui: &mut egui::Ui) {
        ui.heading("Text chat");
        ui.group(|ui| {
            ui.horizontal_wrapped(|ui| {
                let (label, color) = if self.dialog_id.is_some() {
                    ("Chatting with a partner", egui::Color32::LIGHT_GREEN)
                } else if self.searching {
                    ("Searching for a partner…", egui::Color32::LIGHT_BLUE)
                } else if self.connected {
                    ("Ready to find a partner", egui::Color32::LIGHT_GREEN)
                } else {
                    ("Offline", ui.visuals().weak_text_color())
                };
                ui.colored_label(color, egui::RichText::new(label).strong());
                if self.searching {
                    ui.spinner();
                }
                if !self.online_summary.is_empty() {
                    ui.separator();
                    ui.small(&self.online_summary);
                }
            });
        });

        if self.captcha_visible {
            ui.add_space(4.0);
            self.show_text_captcha(ui);
        }

        ui.add_space(4.0);
        egui::CollapsingHeader::new("Connection log")
            .default_open(false)
            .show(ui, |ui| {
                ui.small(format!("{} entries", self.log.len()));
                show_log_viewer(
                    ui,
                    "text-log-viewer",
                    "text-chat",
                    &mut self.log,
                    &mut self.log_filter,
                    &mut self.log_auto_scroll,
                    180.0,
                );
            });
        ui.separator();
        self.show_text_conversation(ui);
    }

    fn show_voice_captcha(&mut self, ui: &mut egui::Ui) {
        if !self.voice_captcha_visible {
            return;
        }

        ui.group(|ui| {
            ui.heading("Captcha required to continue");
            if self.voice_captcha_need > 0 {
                ui.small(format!(
                    "Completed chats: {}/{}",
                    self.voice_captcha_left, self.voice_captcha_need
                ));
            }

            if let Some(texture) = &self.voice_captcha_texture {
                let available_width = ui.available_width().max(1.0);
                let original_size = texture.size_vec2();
                let scale = (available_width / original_size.x).min(1.0);
                ui.image((texture.id(), original_size * scale));
                ui.label(
                    egui::RichText::new("Type the characters shown above, then submit.").strong(),
                );
            } else if self.voice_captcha_url.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading captcha image…");
                });
            } else {
                ui.label("Complete the web challenge, then paste its result below.");
            }

            ui.horizontal_wrapped(|ui| {
                let answer = ui.add_enabled(
                    !self.voice_captcha_submitted,
                    egui::TextEdit::singleline(&mut self.voice_captcha_solution)
                        .hint_text("Captcha answer")
                        .desired_width(220.0),
                );
                if self.voice_captcha_focus_requested {
                    answer.request_focus();
                    self.voice_captcha_focus_requested = false;
                }
                let submit_with_enter =
                    answer.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                let can_submit =
                    !self.voice_captcha_submitted && !self.voice_captcha_solution.trim().is_empty();
                let submit_clicked = ui
                    .add_enabled(can_submit, egui::Button::new("Submit and continue"))
                    .clicked();
                if submit_clicked || (submit_with_enter && can_submit) {
                    self.submit_voice_captcha();
                }
            });
            if self.voice_captcha_submitted {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Answer sent. Waiting for verification…");
                });
            }

            ui.horizontal_wrapped(|ui| {
                if ui.button("New image").clicked() {
                    self.refresh_voice_captcha();
                }
                if let Some(url) = self.voice_captcha_url.clone()
                    && ui.button("Open in browser").clicked()
                {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                } else if self.voice_captcha_url.is_none()
                    && ui.button("Open web captcha").clicked()
                {
                    ui.ctx()
                        .open_url(egui::OpenUrl::new_tab("https://nekto.me/audiochat"));
                }
                ui.small("Press Enter to submit.");
            });
        });
    }

    fn set_voice_muted(&mut self, muted: bool) {
        if self.voice_muted == muted {
            return;
        }
        self.voice_muted = muted;
        let _ = self.voice_session.set_muted(muted);
        let _ = self.worker.commands.send(Command::VoiceMute(muted));
    }

    fn set_voice_speaker_muted(&mut self, muted: bool) {
        self.voice_speaker_muted = muted;
        crate::audio::set_speaker_muted(muted);
    }

    fn show_voice_toolbar(&mut self, ui: &mut egui::Ui) {
        if self.voice_connected || self.voice_in_call {
            if ui.button("Disconnect").clicked() {
                self.cancel_voice_reconnect();
                self.voice_connected = false;
                self.voice_searching = false;
                let _ = self.worker.commands.send(Command::VoiceDisconnect);
            }
        } else if ui.button("Connect").clicked() {
            self.cancel_voice_reconnect();
            let _ = self
                .worker
                .commands
                .send(Command::SaveWebToken(self.voice_web_token.clone()));
            let _ = self.worker.commands.send(self.voice_connect_command());
        }

        if self.voice_searching {
            if ui.button("Stop search").clicked() {
                self.stop_voice_search();
            }
        } else if ui
            .add_enabled(
                self.voice_connected && !self.voice_in_call,
                egui::Button::new("Find peer"),
            )
            .clicked()
        {
            self.start_voice_search();
        }

        if self.voice_in_call {
            if ui
                .button(if self.voice_muted {
                    "Unmute mic"
                } else {
                    "Mute mic"
                })
                .on_hover_text("Toggle microphone · M")
                .clicked()
            {
                self.set_voice_muted(!self.voice_muted);
            }
            if ui
                .button(if self.voice_speaker_muted {
                    "Unmute speaker"
                } else {
                    "Mute speaker"
                })
                .on_hover_text("Toggle local playback · S")
                .clicked()
            {
                self.set_voice_speaker_muted(!self.voice_speaker_muted);
            }
            if ui.button("Hang up").clicked() {
                let _ = self.worker.commands.send(Command::VoiceHangup);
                self.voice_in_call = false;
                self.voice_microphone_active = false;
                #[cfg(feature = "webrtc-media")]
                crate::webrtc_backend::reset_audio_signal_quality();
            }
        }

        ui.menu_button("More ▾", |ui| {
            if ui
                .add_enabled(
                    self.voice_connected,
                    egui::Button::new("Refresh online count"),
                )
                .clicked()
            {
                let _ = self.worker.commands.send(Command::VoiceCount);
                ui.close();
            }
            if ui
                .add_enabled(self.voice_in_call, egui::Button::new("Report call trouble"))
                .clicked()
            {
                let _ = self.worker.commands.send(Command::VoiceTrouble);
                ui.close();
            }
            if ui
                .button("Copy channel diagnostics")
                .on_hover_text(
                    "Copy formatted channel status, audio stats, and setup options to clipboard",
                )
                .clicked()
            {
                ui.ctx().copy_text(self.channel_diagnostics_text());
                self.voice_log
                    .push("Copied channel diagnostic summary to clipboard.".into());
                ui.close();
            }
            if ui.button("Export diagnostics JSON").clicked() {
                match self.export_diagnostics_bundle() {
                    Ok(path) => self
                        .voice_log
                        .push(format!("Exported diagnostics to {}", path.display())),
                    Err(error) => self
                        .voice_log
                        .push(format!("Diagnostics export failed: {error}")),
                }
                ui.close();
            }
            ui.separator();
            let alternate_server = if self.voice_server_host.ends_with(".kz") {
                "audio.nekto.me"
            } else {
                "audio.nekto-me.kz"
            };
            if ui.button(format!("Switch to {alternate_server}")).clicked() {
                self.voice_server_host = if self.voice_server_host.ends_with(".kz") {
                    "audio.nekto.me".into()
                } else {
                    "audio.nekto-me.kz".into()
                };
                self.voice_server_override = true;
                let _ = self
                    .worker
                    .commands
                    .send(Command::SaveVoicePreferences(self.voice_preferences()));
                if self.voice_connected {
                    let _ = self.worker.commands.send(Command::VoiceDisconnect);
                    let _ = self.worker.commands.send(self.voice_connect_command());
                    self.voice_connected = false;
                    self.voice_searching = false;
                    self.voice_in_call = false;
                    self.voice_microphone_active = false;
                }
                self.voice_log
                    .push(format!("Voice server selected: {}", self.voice_server_host));
                ui.close();
            }
            if self.voice_server_override && ui.button("Use configured endpoint").clicked() {
                self.voice_server_override = false;
                let _ = self
                    .worker
                    .commands
                    .send(Command::SaveVoicePreferences(self.voice_preferences()));
                if self.voice_connected {
                    let _ = self.worker.commands.send(Command::VoiceDisconnect);
                    let _ = self.worker.commands.send(self.voice_connect_command());
                    self.voice_connected = false;
                    self.voice_searching = false;
                    self.voice_in_call = false;
                    self.voice_microphone_active = false;
                }
                self.voice_log
                    .push("Using configured voice endpoint again.".into());
                ui.close();
            }
            ui.separator();
            if ui.button("New voice identity").clicked() {
                if !self.voice_web_mode {
                    self.voice_android_user_agent =
                        crate::transport::VoiceTransportClient::random_android_user_agent().into();
                    let _ = self
                        .worker
                        .commands
                        .send(Command::SaveVoicePreferences(self.voice_preferences()));
                }
                let _ = self
                    .worker
                    .commands
                    .send(Command::VoiceResetIdentity(self.voice_web_mode));
                if self.voice_web_mode {
                    self.voice_web_token.clear();
                }
                ui.close();
            }
            if ui.button("Randomize identity and profile").clicked() {
                if !self.voice_web_mode {
                    self.voice_android_user_agent =
                        crate::transport::VoiceTransportClient::random_android_user_agent().into();
                    self.voice_android_locale =
                        crate::transport::VoiceTransportClient::random_android_locale().into();
                    self.voice_android_time_zone =
                        crate::transport::VoiceTransportClient::random_android_time_zone().into();
                    let _ = self
                        .worker
                        .commands
                        .send(Command::SaveVoicePreferences(self.voice_preferences()));
                }
                let _ = self
                    .worker
                    .commands
                    .send(Command::VoiceResetIdentity(self.voice_web_mode));
                if self.voice_web_mode {
                    self.voice_web_token.clear();
                }
                self.voice_log
                    .push("Voice identity and profile randomized.".into());
                ui.close();
            }
        });
    }
}

impl eframe::App for NektoMeApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        trim_log(&mut self.log);
        trim_log(&mut self.voice_log);
        self.poll();
        if !self.voice_in_call && self.voice_speaker_muted {
            self.set_voice_speaker_muted(false);
        }
        if self.voice_in_call && self.voice_push_to_talk {
            let talking = !context.egui_wants_keyboard_input()
                && context.input(|input| input.key_down(egui::Key::Space));
            self.set_voice_muted(!talking);
        } else if self.voice_in_call
            && !context.egui_wants_keyboard_input()
            && context.input(|input| input.key_pressed(egui::Key::M))
        {
            self.set_voice_muted(!self.voice_muted);
        }
        if self.voice_in_call
            && !context.egui_wants_keyboard_input()
            && context.input(|input| input.key_pressed(egui::Key::S))
        {
            self.set_voice_speaker_muted(!self.voice_speaker_muted);
        }
        if self.voice_tab
            && !context.egui_wants_keyboard_input()
            && !self.voice_in_call
            && self.voice_connected
            && context.input(|input| {
                input.key_pressed(egui::Key::F)
                    || (!self.voice_searching && input.key_pressed(egui::Key::Enter))
            })
        {
            if self.voice_searching {
                self.stop_voice_search();
            } else {
                self.start_voice_search();
            }
        }
        if self.voice_tab
            && !context.egui_wants_keyboard_input()
            && context.input(|input| input.key_pressed(egui::Key::Escape))
        {
            if self.voice_searching {
                self.stop_voice_search();
            } else if self.voice_in_call {
                let _ = self.worker.commands.send(Command::VoiceHangup);
                self.voice_in_call = false;
                self.voice_microphone_active = false;
                #[cfg(feature = "webrtc-media")]
                crate::webrtc_backend::reset_audio_signal_quality();
            }
        }
        #[cfg(feature = "cpal-audio")]
        {
            self.voice_microphone_level = crate::audio::microphone_level();
        }
        if let Some(image) = self.voice_captcha_image.take() {
            self.voice_captcha_texture =
                Some(context.load_texture("voice-captcha", image, egui::TextureOptions::LINEAR));
        }
        egui::Panel::top("navigation").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.voice_tab, false, "Text chat");
                ui.selectable_value(&mut self.voice_tab, true, "Voice roulette");
                ui.separator();
                let status = if self.voice_tab {
                    if self.voice_in_call {
                        "In call"
                    } else if self.voice_searching {
                        "Searching…"
                    } else if self.voice_connected {
                        "Connected"
                    } else {
                        "Offline"
                    }
                } else if self.dialog_id.is_some() {
                    "Dialog open"
                } else if self.searching {
                    "Searching…"
                } else if self.connected {
                    "Connected"
                } else {
                    "Offline"
                };
                ui.small(status);
            });
        });
        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                if self.voice_tab {
                    self.show_voice_toolbar(ui);
                } else {
                    self.show_text_toolbar(ui);
                }
            });
        });
        egui::CentralPanel::default().show(ui, |ui| {
            if self.voice_tab {
                ui.heading("Voice roulette");
                ui.group(|ui| {
                    ui.horizontal_wrapped(|ui| {
                        let (label, color) = if self.voice_in_call {
                            ("In call", egui::Color32::LIGHT_GREEN)
                        } else if self.voice_searching {
                            ("Searching for a peer…", egui::Color32::LIGHT_BLUE)
                        } else if self.voice_connected {
                            ("Ready to find a peer", egui::Color32::LIGHT_GREEN)
                        } else {
                            ("Offline", ui.visuals().weak_text_color())
                        };
                        ui.colored_label(color, egui::RichText::new(label).strong());
                        if self.voice_searching {
                            ui.spinner();
                        }
                        if self.voice_connected {
                            ui.separator();
                            ui.small(format!(
                                "Online {}  ·  Waiting {}  ·  Talking {}",
                                self.voice_users, self.voice_waiting, self.voice_talking
                            ));
                        }
                        if self.voice_peer_muted {
                            ui.separator();
                            ui.small("Peer muted");
                        }
                        if self.voice_in_call {
                            ui.separator();
                            let microphone_status = if self.voice_muted {
                                "Microphone muted"
                            } else if self.voice_microphone_active {
                                "Microphone active"
                            } else {
                                "Microphone unavailable"
                            };
                            ui.small(microphone_status);
                            if self.voice_speaker_muted {
                                ui.separator();
                                ui.small("Speaker muted");
                            }
                            if self.voice_push_to_talk {
                                ui.separator();
                                ui.small(if self.voice_muted {
                                    "Hold Space to talk"
                                } else {
                                    "Push to talk active"
                                });
                            }
                        }
                    });
                    #[cfg(feature = "webrtc-media")]
                    if self.voice_in_call {
                        let quality = crate::webrtc_backend::audio_signal_quality();
                        let (label, color) = match quality.state() {
                            crate::webrtc_backend::AudioSignalState::Waiting => {
                                ("Waiting for incoming audio", ui.visuals().weak_text_color())
                            }
                            crate::webrtc_backend::AudioSignalState::Good => {
                                ("Good", egui::Color32::from_rgb(70, 175, 105))
                            }
                            crate::webrtc_backend::AudioSignalState::Fair => {
                                ("Fair", ui.visuals().warn_fg_color)
                            }
                            crate::webrtc_backend::AudioSignalState::Poor => {
                                ("Poor", ui.visuals().error_fg_color)
                            }
                            crate::webrtc_backend::AudioSignalState::Interrupted => {
                                ("Interrupted", ui.visuals().error_fg_color)
                            }
                        };
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Incoming audio signal:");
                            ui.colored_label(color, egui::RichText::new(label).strong());
                            if quality.packets > 0 {
                                ui.small(format!(
                                    "loss {:.1}%  ·  jitter {:.0} ms  ·  buffer {} ms  ·  RTP underruns {}  ·  device underruns {}",
                                    quality.loss_percent,
                                    quality.jitter_ms,
                                    quality.playout_delay_ms,
                                    quality.playout_underruns,
                                    quality.speaker_underruns
                                ))
                                .on_hover_text(
                                    format!(
                                        "Decode errors {:.1}% · {} frames recovered with FEC, PLC, or adaptive delay",
                                        quality.decode_error_percent,
                                        quality.concealed_frames
                                    ),
                                );
                            }
                        });
                    }
                    #[cfg(feature = "cpal-audio")]
                    if self.voice_in_call || self.voice_mic_test {
                        ui.add(
                            egui::ProgressBar::new(self.voice_microphone_level).text(format!(
                                "Microphone {:.0}%",
                                self.voice_microphone_level * 100.0
                            )),
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        if self.voice_in_call {
                            if ui
                                .add(
                                    egui::Button::new("Hang up")
                                        .fill(egui::Color32::from_rgb(180, 50, 50)),
                                )
                                .clicked()
                            {
                                let _ = self.worker.commands.send(Command::VoiceHangup);
                                self.voice_in_call = false;
                                self.voice_microphone_active = false;
                                #[cfg(feature = "webrtc-media")]
                                crate::webrtc_backend::reset_audio_signal_quality();
                            }
                            if ui
                                .button(if self.voice_muted {
                                    "Unmute mic"
                                } else {
                                    "Mute mic"
                                })
                                .on_hover_text("Toggle microphone · M")
                                .clicked()
                            {
                                self.set_voice_muted(!self.voice_muted);
                            }
                            if ui
                                .button(if self.voice_speaker_muted {
                                    "Unmute speaker"
                                } else {
                                    "Mute speaker"
                                })
                                .on_hover_text("Toggle local playback · S")
                                .clicked()
                            {
                                self.set_voice_speaker_muted(!self.voice_speaker_muted);
                            }
                        } else if self.voice_searching {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("⏹ Stop search")
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(180, 60, 60)),
                                )
                                .on_hover_text("Stop searching for a peer · F or Escape")
                                .clicked()
                            {
                                self.stop_voice_search();
                            }
                        } else if self.voice_connected {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("🔍 Find peer")
                                            .strong()
                                            .color(egui::Color32::WHITE),
                                    )
                                    .fill(egui::Color32::from_rgb(46, 139, 87)),
                                )
                                .on_hover_text("Start searching for a voice peer · F or Enter")
                                .clicked()
                            {
                                self.start_voice_search();
                            }
                        } else if ui.button("Connect").clicked() {
                            self.cancel_voice_reconnect();
                            let _ = self
                                .worker
                                .commands
                                .send(Command::SaveWebToken(self.voice_web_token.clone()));
                            let _ = self.worker.commands.send(self.voice_connect_command());
                        }
                    });
                });
                ui.add_space(4.0);
                self.show_voice_captcha(ui);
                if self.voice_captcha_visible {
                    ui.add_space(4.0);
                }
                egui::ScrollArea::vertical()
                    .id_salt("voice-tab-content")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut preferences_changed = false;
                        ui.group(|ui| {
                            ui.strong("Match preferences");
                            let prev_user_sex = self.voice_user_sex.clone();
                            let prev_peer_sex = self.voice_peer_sex.clone();
                            ui.horizontal_wrapped(|ui| {
                                ui.label("I am");
                                egui::ComboBox::from_id_salt("voice-user-sex")
                                    .selected_text(match self.voice_user_sex.as_str() {
                                        "F" => "Woman",
                                        "M" => "Man",
                                        _ => "Any",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.voice_user_sex,
                                            "ANY".into(),
                                            "Any",
                                        );
                                        ui.selectable_value(
                                            &mut self.voice_user_sex,
                                            "F".into(),
                                            "Woman",
                                        );
                                        ui.selectable_value(
                                            &mut self.voice_user_sex,
                                            "M".into(),
                                            "Man",
                                        );
                                    });
                                ui.label("looking for");
                                egui::ComboBox::from_id_salt("voice-peer-sex")
                                    .selected_text(match self.voice_peer_sex.as_str() {
                                        "F" => "Woman",
                                        "M" => "Man",
                                        _ => "Anyone",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut self.voice_peer_sex,
                                            "ANY".into(),
                                            "Anyone",
                                        );
                                        ui.selectable_value(
                                            &mut self.voice_peer_sex,
                                            "F".into(),
                                            "Woman",
                                        );
                                        ui.selectable_value(
                                            &mut self.voice_peer_sex,
                                            "M".into(),
                                            "Man",
                                        );
                                    });
                            });
                            if prev_user_sex != self.voice_user_sex {
                                preferences_changed = true;
                                if self.voice_user_sex == "ANY" {
                                    self.voice_peer_sex = "ANY".into();
                                    self.set_user_age_preset(None);
                                    self.voice_peer_age_from.clear();
                                    self.voice_peer_age_to.clear();
                                } else if self.selected_user_age_preset().is_none() {
                                    self.set_user_age_preset(Some(VoiceAgePreset::From18To24));
                                }
                            }
                            if prev_peer_sex != self.voice_peer_sex {
                                preferences_changed = true;
                                if self.voice_peer_sex != "ANY" && self.voice_user_sex == "ANY" {
                                    self.voice_user_sex = "M".into();
                                    if self.selected_user_age_preset().is_none() {
                                        self.set_user_age_preset(Some(VoiceAgePreset::From18To24));
                                    }
                                }
                            }

                            ui.add_space(6.0);

                            let mut clicked_user_preset = None;
                            let mut clicked_peer_preset = None;
                            let current_user_preset = self.selected_user_age_preset();

                            ui.columns(2, |columns| {
                                // Left column: Ваш возраст
                                columns[0].vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new("Ваш возраст:").strong().size(13.0),
                                    );
                                    ui.add_space(4.0);
                                    let col_width = ui.available_width();
                                    for &preset in &VoiceAgePreset::ALL {
                                        let is_selected = current_user_preset == Some(preset);
                                        let text = if is_selected {
                                            egui::RichText::new(preset.label())
                                                .color(egui::Color32::WHITE)
                                                .size(13.0)
                                        } else {
                                            egui::RichText::new(preset.label())
                                                .color(egui::Color32::from_rgb(26, 32, 44))
                                                .size(13.0)
                                        };
                                        let btn = egui::Button::new(text)
                                            .fill(if is_selected {
                                                egui::Color32::from_rgb(56, 102, 141)
                                            } else {
                                                egui::Color32::from_rgb(185, 194, 204)
                                            })
                                            .corner_radius(6)
                                            .min_size(egui::vec2(col_width, 34.0));
                                        if ui.add(btn).clicked() {
                                            clicked_user_preset = Some((preset, is_selected));
                                        }
                                        ui.add_space(3.0);
                                    }
                                });

                                // Right column: Возраст собеседника
                                columns[1].vertical(|ui| {
                                    ui.label(
                                        egui::RichText::new("Возраст собеседника:")
                                            .strong()
                                            .size(13.0),
                                    );
                                    ui.add_space(4.0);
                                    let col_width = ui.available_width();
                                    for &preset in &VoiceAgePreset::ALL {
                                        let is_selected = self.is_peer_age_preset_selected(preset);
                                        let text = if is_selected {
                                            egui::RichText::new(preset.label())
                                                .color(egui::Color32::WHITE)
                                                .size(13.0)
                                        } else {
                                            egui::RichText::new(preset.label())
                                                .color(egui::Color32::from_rgb(26, 32, 44))
                                                .size(13.0)
                                        };
                                        let btn = egui::Button::new(text)
                                            .fill(if is_selected {
                                                egui::Color32::from_rgb(56, 102, 141)
                                            } else {
                                                egui::Color32::from_rgb(185, 194, 204)
                                            })
                                            .corner_radius(6)
                                            .min_size(egui::vec2(col_width, 34.0));
                                        if ui.add(btn).clicked() {
                                            clicked_peer_preset = Some(preset);
                                        }
                                        ui.add_space(3.0);
                                    }
                                    ui.add_space(2.0);
                                    ui.vertical_centered(|ui| {
                                        ui.label(
                                            egui::RichText::new("Можно несколько")
                                                .italics()
                                                .weak()
                                                .size(12.0),
                                        );
                                    });
                                });
                            });

                            if let Some((preset, was_selected)) = clicked_user_preset {
                                preferences_changed = true;
                                if was_selected {
                                    self.set_user_age_preset(None);
                                } else {
                                    self.set_user_age_preset(Some(preset));
                                    if self.voice_user_sex == "ANY" {
                                        self.voice_user_sex = "M".into();
                                    }
                                }
                            }

                            if let Some(preset) = clicked_peer_preset {
                                preferences_changed = true;
                                self.toggle_peer_age_preset(preset);
                                if self.voice_user_sex == "ANY" {
                                    self.voice_user_sex = "M".into();
                                    if self.selected_user_age_preset().is_none() {
                                        self.set_user_age_preset(Some(VoiceAgePreset::From18To24));
                                    }
                                }
                            }

                            ui.add_space(2.0);
                            ui.collapsing("Указать вручную (Custom range)", |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.label("My age:");
                                    preferences_changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(
                                                &mut self.voice_user_age_from,
                                            )
                                            .hint_text("from")
                                            .desired_width(42.0),
                                        )
                                        .changed();
                                    ui.label("to");
                                    preferences_changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(&mut self.voice_user_age_to)
                                                .hint_text("to")
                                                .desired_width(42.0),
                                        )
                                        .changed();
                                    ui.separator();
                                    ui.label("Peer age:");
                                    preferences_changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(
                                                &mut self.voice_peer_age_from,
                                            )
                                            .hint_text("e.g. 18-24, 25-32")
                                            .desired_width(120.0),
                                        )
                                        .changed();
                                    ui.label("to");
                                    preferences_changed |= ui
                                        .add(
                                            egui::TextEdit::singleline(&mut self.voice_peer_age_to)
                                                .hint_text("to")
                                                .desired_width(42.0),
                                        )
                                        .changed();
                                });
                            });
                        });
                        ui.add_space(4.0);
                        egui::CollapsingHeader::new("Audio and call settings")
                            .default_open(true)
                            .show(ui, |ui| {
                                preferences_changed |= ui
                                    .checkbox(
                                        &mut self.voice_auto_next,
                                        "Automatically find the next peer",
                                    )
                                    .changed();
                                let push_to_talk_changed = ui
                                    .checkbox(
                                        &mut self.voice_push_to_talk,
                                        "Push to talk (hold Space during a call)",
                                    )
                                    .changed();
                                preferences_changed |= push_to_talk_changed;
                                if push_to_talk_changed && self.voice_in_call {
                                    self.set_voice_muted(self.voice_push_to_talk);
                                }
                                ui.horizontal(|ui| {
                                    preferences_changed |= ui
                                        .checkbox(
                                            &mut self.voice_record_sessions,
                                            "Save calls to Downloads as stereo",
                                        )
                                        .changed();
                                    ui.add_enabled_ui(self.voice_record_sessions, |ui| {
                                        let previous_format = self.voice_recording_format;
                                        egui::ComboBox::from_id_salt("voice-recording-format")
                                            .selected_text(self.voice_recording_format.label())
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(
                                                    &mut self.voice_recording_format,
                                                    crate::session_recorder::RecordingFormat::Flac,
                                                    "FLAC",
                                                );
                                                ui.selectable_value(
                                                    &mut self.voice_recording_format,
                                                    crate::session_recorder::RecordingFormat::Mp3,
                                                    "MP3",
                                                );
                                                ui.selectable_value(
                                                    &mut self.voice_recording_format,
                                                    crate::session_recorder::RecordingFormat::Opus,
                                                    "Opus",
                                                );
                                            });
                                        preferences_changed |=
                                            previous_format != self.voice_recording_format;
                                    });
                                });
                                ui.add_enabled_ui(self.voice_record_sessions, |ui| {
                                    ui.collapsing("Recording quality", |ui| {
                                        match self.voice_recording_format {
                                            crate::session_recorder::RecordingFormat::Flac => {
                                                ui.label(
                                                    "Lossless 48 kHz, 16-bit stereo audio.",
                                                );
                                            }
                                            crate::session_recorder::RecordingFormat::Mp3 => {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label("Rate mode");
                                                    let previous = self
                                                        .voice_recording_parameters
                                                        .mp3_rate_mode;
                                                    egui::ComboBox::from_id_salt(
                                                        "voice-mp3-rate-mode",
                                                    )
                                                    .selected_text(previous.label())
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .mp3_rate_mode,
                                                            crate::session_recorder::Mp3RateMode::Constant,
                                                            "CBR",
                                                        );
                                                        ui.selectable_value(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .mp3_rate_mode,
                                                            crate::session_recorder::Mp3RateMode::Variable,
                                                            "VBR",
                                                        );
                                                    });
                                                    preferences_changed |= previous
                                                        != self
                                                            .voice_recording_parameters
                                                            .mp3_rate_mode;
                                                });
                                                match self
                                                    .voice_recording_parameters
                                                    .mp3_rate_mode
                                                {
                                                    crate::session_recorder::Mp3RateMode::Constant => {
                                                        let previous = self
                                                            .voice_recording_parameters
                                                            .mp3_bitrate_kbps;
                                                        egui::ComboBox::from_id_salt(
                                                            "voice-mp3-bitrate",
                                                        )
                                                        .selected_text(format!("{previous} kbps"))
                                                        .show_ui(ui, |ui| {
                                                            for bitrate in [
                                                                64, 96, 128, 160, 192, 224, 256,
                                                                320,
                                                            ] {
                                                                ui.selectable_value(
                                                                    &mut self
                                                                        .voice_recording_parameters
                                                                        .mp3_bitrate_kbps,
                                                                    bitrate,
                                                                    format!("{bitrate} kbps"),
                                                                );
                                                            }
                                                        });
                                                        preferences_changed |= previous
                                                            != self
                                                                .voice_recording_parameters
                                                                .mp3_bitrate_kbps;
                                                    }
                                                    crate::session_recorder::Mp3RateMode::Variable => {
                                                        preferences_changed |= ui
                                                            .add(
                                                                egui::Slider::new(
                                                                    &mut self
                                                                        .voice_recording_parameters
                                                                        .mp3_vbr_quality,
                                                                    0.0..=9.0,
                                                                )
                                                                .step_by(1.0)
                                                                .text("VBR quality (0 best)"),
                                                            )
                                                            .changed();
                                                    }
                                                }
                                            }
                                            crate::session_recorder::RecordingFormat::Opus => {
                                                preferences_changed |= ui
                                                    .add(
                                                        egui::Slider::new(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .opus_bitrate_kbps,
                                                            32..=256,
                                                        )
                                                        .step_by(8.0)
                                                        .suffix(" kbps")
                                                        .text("Bitrate"),
                                                    )
                                                    .changed();
                                                preferences_changed |= ui
                                                    .add(
                                                        egui::Slider::new(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .opus_complexity,
                                                            0..=10,
                                                        )
                                                        .text("Encoder complexity"),
                                                    )
                                                    .changed();
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label("Rate mode");
                                                    let previous = self
                                                        .voice_recording_parameters
                                                        .opus_rate_mode;
                                                    egui::ComboBox::from_id_salt(
                                                        "voice-opus-rate-mode",
                                                    )
                                                    .selected_text(previous.label())
                                                    .show_ui(ui, |ui| {
                                                        ui.selectable_value(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .opus_rate_mode,
                                                            crate::session_recorder::OpusRateMode::Variable,
                                                            "VBR",
                                                        );
                                                        ui.selectable_value(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .opus_rate_mode,
                                                            crate::session_recorder::OpusRateMode::ConstrainedVariable,
                                                            "Constrained VBR",
                                                        );
                                                        ui.selectable_value(
                                                            &mut self
                                                                .voice_recording_parameters
                                                                .opus_rate_mode,
                                                            crate::session_recorder::OpusRateMode::Constant,
                                                            "CBR",
                                                        );
                                                    });
                                                    preferences_changed |= previous
                                                        != self
                                                            .voice_recording_parameters
                                                            .opus_rate_mode;
                                                });
                                            }
                                        }
                                        ui.small("Changes apply to the next recorded call.");
                                    });
                                });
                                self.show_recording_library(ui);
                                #[cfg(feature = "cpal-audio")]
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.strong("Audio devices");
                                        if ui.small_button("Refresh").clicked() {
                                            self.voice_audio_devices = crate::audio::audio_devices();
                                        }
                                    });
                                    let input_label = audio_device_label(
                                        &self.voice_microphone_device,
                                        &self.voice_audio_devices.inputs,
                                        self.voice_audio_devices.default_input.as_deref(),
                                    );
                                    let previous_input = self.voice_microphone_device.clone();
                                    egui::ComboBox::from_id_salt("voice-microphone-device")
                                        .selected_text(input_label)
                                        .width(280.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.voice_microphone_device,
                                                String::new(),
                                                "System default microphone",
                                            );
                                            for name in &self.voice_audio_devices.inputs {
                                                ui.selectable_value(
                                                    &mut self.voice_microphone_device,
                                                    name.clone(),
                                                    name,
                                                );
                                            }
                                        });
                                    preferences_changed |=
                                        previous_input != self.voice_microphone_device;

                                    let output_label = audio_device_label(
                                        &self.voice_speaker_device,
                                        &self.voice_audio_devices.outputs,
                                        self.voice_audio_devices.default_output.as_deref(),
                                    );
                                    let previous_output = self.voice_speaker_device.clone();
                                    egui::ComboBox::from_id_salt("voice-speaker-device")
                                        .selected_text(output_label)
                                        .width(280.0)
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.voice_speaker_device,
                                                String::new(),
                                                "System default speaker",
                                            );
                                            for name in &self.voice_audio_devices.outputs {
                                                ui.selectable_value(
                                                    &mut self.voice_speaker_device,
                                                    name.clone(),
                                                    name,
                                                );
                                            }
                                        });
                                    preferences_changed |=
                                        previous_output != self.voice_speaker_device;
                                    ui.small("Device changes apply to the next call or microphone test. Missing saved devices use the system default.");
                                });
                                preferences_changed |= ui
                                    .add(
                                        egui::Slider::new(
                                            &mut self.voice_speaker_gain,
                                            0.0..=200.0,
                                        )
                                        .text("Speaker gain %"),
                                    )
                                    .changed();
                                preferences_changed |= ui
                                    .checkbox(&mut self.voice_speaker_dsp, "Speaker DSP")
                                    .changed();

                                preferences_changed |= ui
                                    .add(
                                        egui::Slider::new(
                                            &mut self.voice_microphone_gain,
                                            25.0..=400.0,
                                        )
                                        .text("Microphone gain %"),
                                    )
                                    .changed();
                                preferences_changed |= ui
                                    .checkbox(&mut self.voice_microphone_dsp, "Microphone DSP")
                                    .changed();
                                preferences_changed |= ui
                                    .checkbox(
                                        &mut self.voice_microphone_noise_suppression,
                                        "Noise suppression (RNNoise)",
                                    )
                                    .changed();
                                #[cfg(feature = "cpal-audio")]
                                ui.add(egui::ProgressBar::new(self.voice_microphone_level).text(
                                    format!(
                                        "Microphone {:.0}%",
                                        self.voice_microphone_level * 100.0
                                    ),
                                ));
                                #[cfg(feature = "cpal-audio")]
                                ui.small(if self.voice_microphone_active {
                                    "Microphone active"
                                } else if self.voice_in_call {
                                    "Microphone inactive — see voice log"
                                } else {
                                    "Microphone activates when a peer connects"
                                });
                                #[cfg(not(feature = "cpal-audio"))]
                ui.colored_label(
                    egui::Color32::YELLOW,
                    "Microphone unavailable in this build. Restart with ./run.sh --native-audio.",
                );
                                #[cfg(feature = "cpal-audio")]
                                if ui
                                    .checkbox(&mut self.voice_mic_test, "Microphone loopback test")
                                    .changed()
                                {
                                    if self.voice_in_call {
                                        self.voice_mic_test = false;
                                        self.voice_log
                            .push("Stop the active call before starting a microphone test.".into());
                                    } else {
                                        let _ = self
                                            .worker
                                            .commands
                                            .send(Command::VoiceMicTest(self.voice_mic_test));
                                    }
                                }
                                preferences_changed |= ui
                                    .checkbox(&mut self.voice_echo_cancellation, "Echo suppression")
                                    .changed();
                                ui.collapsing("Advanced microphone DSP", |ui| {
                                    // 1. Holistic Voice Character Profile selection
                                    ui.horizontal(|ui| {
                                        ui.label(egui::RichText::new("Character Profile:").strong());
                                        let prev_character_profile = self.voice_character_profile;
                                        egui::ComboBox::from_id_salt("voice-character-profile")
                                            .selected_text(self.voice_character_profile.label())
                                            .show_ui(ui, |ui| {
                                                ui.label(egui::RichText::new("🗣️ Dialogue & Voice").strong());
                                                for profile in crate::audio::VoiceCharacterProfile::DIALOGUE {
                                                    ui.selectable_value(
                                                        &mut self.voice_character_profile,
                                                        profile,
                                                        profile.label(),
                                                    );
                                                }
                                                ui.separator();
                                                ui.label(egui::RichText::new("👑 Dominant & Authority").strong());
                                                for profile in crate::audio::VoiceCharacterProfile::DOMINANT {
                                                    ui.selectable_value(
                                                        &mut self.voice_character_profile,
                                                        profile,
                                                        profile.label(),
                                                    );
                                                }
                                                ui.separator();
                                                ui.label(egui::RichText::new("🎭 Character & Tone").strong());
                                                for profile in crate::audio::VoiceCharacterProfile::CHARACTER {
                                                    ui.selectable_value(
                                                        &mut self.voice_character_profile,
                                                        profile,
                                                        profile.label(),
                                                    );
                                                }
                                                ui.separator();
                                                ui.label(egui::RichText::new("🎛️ Analog Coloration").strong());
                                                for profile in crate::audio::VoiceCharacterProfile::ANALOG {
                                                    ui.selectable_value(
                                                        &mut self.voice_character_profile,
                                                        profile,
                                                        profile.label(),
                                                    );
                                                }
                                                ui.separator();
                                                ui.selectable_value(
                                                    &mut self.voice_character_profile,
                                                    crate::audio::VoiceCharacterProfile::Custom,
                                                    crate::audio::VoiceCharacterProfile::Custom.label(),
                                                );
                                            });
                                        if self.voice_character_profile != prev_character_profile {
                                            if let Some(p) = self.voice_character_profile.params() {
                                                self.voice_presence_boost_db = p.presence_db;
                                                self.voice_saturation_drive = p.drive;
                                                self.voice_saturation_asymmetry = p.asymmetry;
                                                self.voice_saturation_bias = p.bias;
                                                self.voice_proximity_gain_db = p.proximity_db;
                                                self.voice_air_boost_db = p.air_db;
                                                self.voice_compressor_ratio = p.compressor_ratio;
                                                self.voice_makeup_gain = p.makeup_gain;
                                                self.voice_microphone_warmth = true;
                                                self.voice_saturation_profile =
                                                    self.voice_character_profile.to_saturation_profile();
                                            }
                                            preferences_changed = true;
                                        }
                                    });
                                    ui.small(self.voice_character_profile.description());

                                    // Sonic dimensions readout: Warmth ➔ Proximity ➔ Presence ➔ Air + Compression
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "🔥 Warmth: {:.2}×  ·  🎙️ Proximity: {:+.1} dB  ·  🗣️ Presence: {:+.1} dB  ·  🍃 Air: {:+.1} dB  ·  🎛️ Comp: {:.1}:1",
                                                self.voice_saturation_drive,
                                                self.voice_proximity_gain_db,
                                                self.voice_presence_boost_db,
                                                self.voice_air_boost_db,
                                                self.voice_compressor_ratio
                                            ))
                                            .weak(),
                                        );
                                    });
                                    ui.separator();

                                    // 2. Vocal Dynamics (Noise Gate & Compressor)
                                    ui.label(egui::RichText::new("Dynamics & Level").strong());
                                    let gate_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_noise_gate_threshold,
                                            0.0..=0.1,
                                        )
                                        .step_by(0.001)
                                        .max_decimals(4)
                                        .text("Noise gate"),
                                    );
                                    let thresh_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_compressor_threshold,
                                            0.05..=1.0,
                                        )
                                        .step_by(0.01)
                                        .max_decimals(2)
                                        .text("Compressor threshold"),
                                    );
                                    let ratio_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_compressor_ratio,
                                            1.0..=10.0,
                                        )
                                        .step_by(0.1)
                                        .max_decimals(1)
                                        .text("Compressor ratio"),
                                    );
                                    let makeup_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_makeup_gain,
                                            0.5..=3.0,
                                        )
                                        .step_by(0.05)
                                        .max_decimals(2)
                                        .text("Makeup gain"),
                                    );
                                    if gate_resp.dragged()
                                        || gate_resp.lost_focus()
                                        || thresh_resp.dragged()
                                        || thresh_resp.lost_focus()
                                        || ratio_resp.dragged()
                                        || ratio_resp.lost_focus()
                                        || makeup_resp.dragged()
                                        || makeup_resp.lost_focus()
                                    {
                                        self.voice_character_profile =
                                            crate::audio::VoiceCharacterProfile::from_params(
                                                self.voice_presence_boost_db,
                                                self.voice_saturation_drive,
                                                self.voice_saturation_asymmetry,
                                                self.voice_proximity_gain_db,
                                                self.voice_air_boost_db,
                                                self.voice_compressor_ratio,
                                            );
                                        self.voice_saturation_profile =
                                            self.voice_character_profile.to_saturation_profile();
                                        preferences_changed = true;
                                    }

                                    ui.separator();

                                    // 3. Vocal Tone Equalization: Proximity ➔ Presence ➔ Air
                                    ui.label(egui::RichText::new("Tone & Articulation (EQ)").strong());
                                    let prox_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_proximity_gain_db,
                                            -3.0..=3.0,
                                        )
                                        .step_by(0.1)
                                        .max_decimals(1)
                                        .suffix(" dB")
                                        .text("Proximity / Intimacy"),
                                    ).on_hover_text(
                                        "200 Hz chest resonance & near-mic intimacy. \
                                         Boost (+dB) adds close-mic proximity warmth; \
                                         cut (-dB) eliminates boomy room resonance.",
                                    );

                                    let pres_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_presence_boost_db,
                                            0.0..=6.0,
                                        )
                                        .step_by(0.1)
                                        .max_decimals(1)
                                        .suffix(" dB")
                                        .text("Presence"),
                                    ).on_hover_text(
                                        "3.2 kHz speech formant peak EQ — lifts vocal \
                                         authority and consonant bite without adding \
                                         saturation or distortion",
                                    );

                                    let air_resp = ui.add(
                                        egui::Slider::new(
                                            &mut self.voice_air_boost_db,
                                            0.0..=3.0,
                                        )
                                        .step_by(0.1)
                                        .max_decimals(1)
                                        .suffix(" dB")
                                        .text("Air"),
                                    ).on_hover_text(
                                        "10 kHz silky top-end sheen & breathy openness \
                                         for transparent near-mic presence.",
                                    );

                                    if prox_resp.dragged()
                                        || prox_resp.lost_focus()
                                        || pres_resp.dragged()
                                        || pres_resp.lost_focus()
                                        || air_resp.dragged()
                                        || air_resp.lost_focus()
                                    {
                                        self.voice_character_profile =
                                            crate::audio::VoiceCharacterProfile::from_params(
                                                self.voice_presence_boost_db,
                                                self.voice_saturation_drive,
                                                self.voice_saturation_asymmetry,
                                                self.voice_proximity_gain_db,
                                                self.voice_air_boost_db,
                                                self.voice_compressor_ratio,
                                            );
                                        self.voice_saturation_profile =
                                            self.voice_character_profile.to_saturation_profile();
                                        preferences_changed = true;
                                    }

                                    ui.separator();

                                    // 4. Analog Tube/Tape Warmth Saturation
                                    preferences_changed |= ui
                                        .checkbox(
                                            &mut self.voice_microphone_warmth,
                                            "Tube warmth",
                                        )
                                        .on_hover_text(
                                            "Analogue tube/tape saturation — adds vocal \
                                             presence and harmonic richness",
                                        )
                                        .changed();

                                    if self.voice_microphone_warmth {
                                        ui.horizontal(|ui| {
                                            ui.label("Saturation Profile:");
                                            let prev_sat = self.voice_saturation_profile;
                                            egui::ComboBox::from_id_salt("voice-saturation-profile")
                                                .selected_text(self.voice_saturation_profile.label())
                                                .show_ui(ui, |ui| {
                                                    ui.label(egui::RichText::new("Dialogue & Voice").strong());
                                                    for profile in crate::audio::SaturationProfile::DIALOGUE {
                                                        ui.selectable_value(
                                                            &mut self.voice_saturation_profile,
                                                            profile,
                                                            profile.label(),
                                                        );
                                                    }
                                                    ui.separator();
                                                    ui.label(egui::RichText::new("👑 Dominant & Authority").strong());
                                                    for profile in crate::audio::SaturationProfile::DOMINANT {
                                                        ui.selectable_value(
                                                            &mut self.voice_saturation_profile,
                                                            profile,
                                                            profile.label(),
                                                        );
                                                    }
                                                    ui.separator();
                                                    ui.label(egui::RichText::new("🎭 Character & Tone").strong());
                                                    for profile in crate::audio::SaturationProfile::CHARACTER {
                                                        ui.selectable_value(
                                                            &mut self.voice_saturation_profile,
                                                            profile,
                                                            profile.label(),
                                                        );
                                                    }
                                                    ui.separator();
                                                    ui.label(egui::RichText::new("🎛️ Analog Coloration").strong());
                                                    for profile in crate::audio::SaturationProfile::ANALOG {
                                                        ui.selectable_value(
                                                            &mut self.voice_saturation_profile,
                                                            profile,
                                                            profile.label(),
                                                        );
                                                    }
                                                    ui.separator();
                                                    ui.selectable_value(
                                                        &mut self.voice_saturation_profile,
                                                        crate::audio::SaturationProfile::Custom,
                                                        crate::audio::SaturationProfile::Custom.label(),
                                                    );
                                                });
                                            if self.voice_saturation_profile != prev_sat {
                                                if let Some(p) = self.voice_saturation_profile.full_params() {
                                                    self.voice_presence_boost_db = p.presence_db;
                                                    self.voice_saturation_drive = p.drive;
                                                    self.voice_saturation_asymmetry = p.asymmetry;
                                                    self.voice_saturation_bias = p.bias;
                                                    self.voice_proximity_gain_db = p.proximity_db;
                                                    self.voice_air_boost_db = p.air_db;
                                                    self.voice_compressor_ratio = p.compressor_ratio;
                                                    self.voice_makeup_gain = p.makeup_gain;
                                                    self.voice_character_profile =
                                                        self.voice_saturation_profile.to_character_profile();
                                                }
                                                preferences_changed = true;
                                            }
                                        });
                                        ui.small(self.voice_saturation_profile.description());

                                        let drive_resp = ui.add(
                                            egui::Slider::new(
                                                &mut self.voice_saturation_drive,
                                                1.0..=3.0,
                                            )
                                            .step_by(0.01)
                                            .max_decimals(2)
                                            .clamping(egui::SliderClamping::Edits)
                                            .text("Drive"),
                                        ).on_hover_text("Overall saturation intensity — how hard the signal hits the tube curve");

                                        let asym_resp = ui.add(
                                            egui::Slider::new(
                                                &mut self.voice_saturation_asymmetry,
                                                1.0..=3.0,
                                            )
                                            .step_by(0.01)
                                            .max_decimals(2)
                                            .clamping(egui::SliderClamping::Edits)
                                            .text("Asymmetry"),
                                        ).on_hover_text("α+/α− ratio: 1.0 = symmetric (3rd harmonic), 1.7 = golden vintage (2nd harmonic), 2.5 = heavy tube");

                                        let bias_resp = ui.add(
                                            egui::Slider::new(
                                                &mut self.voice_saturation_bias,
                                                -0.02..=0.05,
                                            )
                                            .step_by(0.001)
                                            .max_decimals(3)
                                            .clamping(egui::SliderClamping::Edits)
                                            .text("Bias (δ)"),
                                        ).on_hover_text("Class-A operating point shift — shifts crossover point for asymmetric clipping (0.0 = centered / pure vocal warmth)");

                                        if drive_resp.dragged()
                                            || drive_resp.lost_focus()
                                            || asym_resp.dragged()
                                            || asym_resp.lost_focus()
                                            || bias_resp.dragged()
                                            || bias_resp.lost_focus()
                                        {
                                            self.voice_saturation_profile =
                                                crate::audio::SaturationProfile::from_params(
                                                    self.voice_saturation_drive,
                                                    self.voice_saturation_asymmetry,
                                                    self.voice_saturation_bias,
                                                );
                                            self.voice_character_profile =
                                                crate::audio::VoiceCharacterProfile::from_params(
                                                    self.voice_presence_boost_db,
                                                    self.voice_saturation_drive,
                                                    self.voice_saturation_asymmetry,
                                                    self.voice_proximity_gain_db,
                                                    self.voice_air_boost_db,
                                                    self.voice_compressor_ratio,
                                                );
                                            preferences_changed = true;
                                        }
                                    }

                                    ui.separator();
                                    egui::CollapsingHeader::new("✨ AI Voice Changer (RVC / ONNX)")
                                        .default_open(self.voice_changer_enabled)
                                        .show(ui, |ui| {
                                            preferences_changed |= ui
                                                .checkbox(
                                                    &mut self.voice_changer_enabled,
                                                    "Enable Neural Voice Morpher",
                                                )
                                                .on_hover_text(
                                                    "Real-time neural voice conversion using ONNX Runtime (RVC v2 + RMVPE + ContentVec).",
                                                )
                                                .changed();

                                            if self.voice_changer_enabled {
                                                ui.horizontal(|ui| {
                                                    ui.label("Model:");
                                                    let current_label = match self.voice_changer_model.as_str() {
                                                        "models/voices/sabrina.onnx" => "✨ Sabrina (Clean Pop)",
                                                        "models/voices/lana.onnx" => "🌹 Lana (Smooth Vocal)",
                                                        "models/voices/woman_1.onnx" => "👩 Soft Female (woman_1)",
                                                        "models/voices/woman_2.onnx" => "👱 Bright Female (woman_2)",
                                                        "models/voices/woman_3.onnx" => "👧 Warm Female (woman_3)",
                                                        other => other,
                                                    };
                                                    egui::ComboBox::from_id_salt("voice-changer-model-select")
                                                        .selected_text(current_label)
                                                        .show_ui(ui, |ui| {
                                                            if ui.selectable_label(self.voice_changer_model == "models/voices/sabrina.onnx", "✨ Sabrina (Clean Pop)").clicked() {
                                                                self.voice_changer_model = "models/voices/sabrina.onnx".into();
                                                                preferences_changed = true;
                                                            }
                                                            if ui.selectable_label(self.voice_changer_model == "models/voices/lana.onnx", "🌹 Lana (Smooth Vocal)").clicked() {
                                                                self.voice_changer_model = "models/voices/lana.onnx".into();
                                                                preferences_changed = true;
                                                            }
                                                            if ui.selectable_label(self.voice_changer_model == "models/voices/woman_1.onnx", "👩 Soft Female (woman_1)").clicked() {
                                                                self.voice_changer_model = "models/voices/woman_1.onnx".into();
                                                                preferences_changed = true;
                                                            }
                                                            if ui.selectable_label(self.voice_changer_model == "models/voices/woman_2.onnx", "👱 Bright Female (woman_2)").clicked() {
                                                                self.voice_changer_model = "models/voices/woman_2.onnx".into();
                                                                preferences_changed = true;
                                                            }
                                                            if ui.selectable_label(self.voice_changer_model == "models/voices/woman_3.onnx", "👧 Warm Female (woman_3)").clicked() {
                                                                self.voice_changer_model = "models/voices/woman_3.onnx".into();
                                                                preferences_changed = true;
                                                            }
                                                        });
                                                });

                                                ui.horizontal(|ui| {
                                                    ui.label("Pitch Shift:");
                                                    let resp = ui.add(
                                                        egui::Slider::new(&mut self.voice_changer_pitch, -12.0..=24.0)
                                                            .step_by(1.0)
                                                            .suffix(" st")
                                                            .text("Semitones"),
                                                    ).on_hover_text("0 st = естественный тон модели (без питча, самый натуральный); +6..+12 st = повышение тона");
                                                    preferences_changed |= resp.changed();
                                                });

                                                ui.horizontal(|ui| {
                                                    ui.label("Quick Pitch:");
                                                    if ui.selectable_label(self.voice_changer_pitch.abs() < 0.2, "0 st (Натуральный)").clicked() {
                                                        self.voice_changer_pitch = 0.0;
                                                        preferences_changed = true;
                                                    }
                                                    if ui.selectable_label((self.voice_changer_pitch - 6.0).abs() < 0.2, "+6 st (Мягкий)").clicked() {
                                                        self.voice_changer_pitch = 6.0;
                                                        preferences_changed = true;
                                                    }
                                                    if ui.selectable_label((self.voice_changer_pitch - 12.0).abs() < 0.2, "+12 st (+1 октава)").clicked() {
                                                        self.voice_changer_pitch = 12.0;
                                                        preferences_changed = true;
                                                    }
                                                });

                                                ui.horizontal(|ui| {
                                                    ui.label("Замена голоса (Wet):");
                                                    let resp = ui.add(
                                                        egui::Slider::new(&mut self.voice_changer_wet, 0.0..=1.0)
                                                            .step_by(0.05)
                                                            .custom_formatter(|v, _| {
                                                                if v >= 0.99 {
                                                                    "100% (Только AI)".into()
                                                                } else {
                                                                    format!("{:.0}% AI", v * 100.0)
                                                                }
                                                            }),
                                                    ).on_hover_text("100% = только изменённый голос. Меньше 100% подмешивает ваш настоящий голос.");
                                                    preferences_changed |= resp.changed();

                                                    if (self.voice_changer_wet - 1.0).abs() > 0.01
                                                        && ui.button("Сбросить на 100%").clicked()
                                                    {
                                                        self.voice_changer_wet = 1.0;
                                                        preferences_changed = true;
                                                    }
                                                });
                                            }
                                        });
                                });
                            });
                        egui::CollapsingHeader::new("Connection and identity")
                            .default_open(false)
                            .show(ui, |ui| {
                                let mode_changed = ui
                                    .checkbox(&mut self.voice_web_mode, "Web signaling mode")
                                    .changed();
                                preferences_changed |= mode_changed;
                                if mode_changed && self.voice_connected {
                                    let _ = self.worker.commands.send(Command::VoiceDisconnect);
                                    let _ = self.worker.commands.send(self.voice_connect_command());
                                    self.voice_connected = false;
                                    self.voice_searching = false;
                                    self.voice_in_call = false;
                                    self.voice_log
                                        .push("Voice mode changed; reconnecting…".into());
                                }
                                if self.voice_web_mode {
                                    ui.horizontal(|ui| {
                                        ui.label("Web token");
                                        preferences_changed |= ui
                                            .text_edit_singleline(&mut self.voice_web_token)
                                            .changed();
                                        if ui.button("Import from Chrome").clicked() {
                                            if let Some(token) =
                                                crate::persistence::try_extract_web_token()
                                            {
                                                self.voice_web_token = token;
                                                let _ = self.worker.commands.send(
                                                    Command::SaveWebToken(
                                                        self.voice_web_token.clone(),
                                                    ),
                                                );
                                                self.voice_log
                                                    .push("Imported Web token from Chrome.".into());
                                            } else {
                                                self.voice_log
                                                    .push("Chrome Web token was not found.".into());
                                            }
                                        }
                                        if ui.button("Apply & find").clicked() {
                                            self.apply_web_token_and_search();
                                        }
                                        if ui.button("Open audiochat in browser").clicked() {
                                            match open_external_url("https://nekto.me/audiochat") {
                                                Ok(()) => self.voice_log.push(
                                                    "Opened NektoMe audiochat in the browser."
                                                        .into(),
                                                ),
                                                Err(error) => self.voice_log.push(format!(
                                                    "Could not open audiochat: {error}"
                                                )),
                                            }
                                        }
                                    });
                                } else {
                                    ui.horizontal(|ui| {
                                        ui.label("Android voice ID");
                                        ui.text_edit_singleline(&mut self.voice_android_user_id);
                                        if ui.button("Apply").clicked() {
                                            let user_id =
                                                self.voice_android_user_id.trim().to_owned();
                                            if user_id.is_empty() {
                                                self.voice_log.push(
                                                    "Android voice ID cannot be empty.".into(),
                                                );
                                            } else {
                                                let reconnect = self.voice_connected;
                                                let _ = self
                                                    .worker
                                                    .commands
                                                    .send(Command::VoiceSetIdentity(user_id));
                                                if reconnect {
                                                    let _ = self
                                                        .worker
                                                        .commands
                                                        .send(self.voice_connect_command());
                                                }
                                                self.voice_log.push(
                                    if reconnect {
                                        "Android voice ID updated; reconnecting voice session…"
                                    } else {
                                        "Android voice ID updated; reconnect to apply it."
                                    }
                                    .into(),
                                );
                                                self.voice_connected = false;
                                                self.voice_searching = false;
                                                self.voice_in_call = false;
                                            }
                                        }
                                        if ui.button("Import from emulator").clicked() {
                                            let _ = self
                                                .worker
                                                .commands
                                                .send(Command::VoiceImportEmulator);
                                            self.voice_log.push(
                                                "Reading voice profile from adb emulator…".into(),
                                            );
                                        }
                                    });
                                }
                                ui.horizontal(|ui| {
                                    ui.label("Proxy (optional)");
                                    if ui.text_edit_singleline(&mut self.voice_proxy_url).changed()
                                    {
                                        self.voice_proxy_index = 0;
                                        preferences_changed = true;
                                    }
                                    let proxy_count =
                                        crate::transport::proxy_entries(&self.voice_proxy_url)
                                            .len();
                                    if ui
                                        .add_enabled(proxy_count > 1, egui::Button::new("◀"))
                                        .clicked()
                                    {
                                        self.rotate_voice_proxy(-1);
                                    }
                                    if ui
                                        .add_enabled(proxy_count > 1, egui::Button::new("▶"))
                                        .clicked()
                                    {
                                        self.rotate_voice_proxy(1);
                                    }
                                    if proxy_count > 0 {
                                        ui.small(format!(
                                            "{}/{}",
                                            self.voice_proxy_index.min(proxy_count - 1) + 1,
                                            proxy_count
                                        ));
                                    }
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Proxy-Seller API key");
                                    let changed = ui
                                        .add(
                                            egui::TextEdit::singleline(
                                                &mut self.voice_proxy_seller_api_key,
                                            )
                                            .password(true),
                                        )
                                        .changed();
                                    if changed {
                                        preferences_changed = true;
                                    }
                                    if ui.button("Fetch resident pool").clicked() {
                                        let _ = self.worker.commands.send(
                                            Command::SaveVoicePreferences(self.voice_preferences()),
                                        );
                                        let _ =
                                            self.worker.commands.send(Command::FetchProxySeller(
                                                self.voice_proxy_seller_api_key.trim().to_owned(),
                                            ));
                                        self.voice_log.push(
                                            "Requesting resident proxies from Proxy-Seller…".into(),
                                        );
                                    }
                                });
                                ui.collapsing("Proxy-Seller list settings", |ui| {
                                    preferences_changed |= ui
                                        .text_edit_singleline(
                                            &mut self.voice_proxy_seller_list_name,
                                        )
                                        .changed();
                                    ui.label("List name");
                                    preferences_changed |= ui
                                        .add(
                                            egui::DragValue::new(
                                                &mut self.voice_proxy_seller_ports_count,
                                            )
                                            .range(1..=1000)
                                            .prefix("Ports: "),
                                        )
                                        .changed();
                                    preferences_changed |= ui
                                        .add(
                                            egui::DragValue::new(
                                                &mut self.voice_proxy_seller_rotation_seconds,
                                            )
                                            .range(60..=31_536_000)
                                            .prefix("Rotation seconds: "),
                                        )
                                        .changed();
                                    preferences_changed |= ui
                                        .text_edit_singleline(
                                            &mut self.voice_proxy_seller_whitelist_ip,
                                        )
                                        .changed();
                                    ui.label("Whitelist IP (optional)");
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Remote WAV (optional)");
                                    preferences_changed |= ui
                                        .text_edit_singleline(&mut self.voice_remote_audio_wav_path)
                                        .changed();
                                });
                                ui.horizontal(|ui| {
                                    ui.label("Android device profile");
                                    if self.voice_android_user_agent.is_empty() {
                                        ui.label("default");
                                    } else {
                                        ui.label(&self.voice_android_user_agent);
                                    }
                                    if ui.button("Randomize").clicked() {
                                        self.voice_android_user_agent =
                            crate::transport::VoiceTransportClient::random_android_user_agent()
                                .into();
                                        preferences_changed = true;
                                    }
                                });
                                if !self.voice_web_mode {
                                    ui.small(format!(
                                        "Android profile locale: {} · timezone: {}",
                                        if self.voice_android_locale.is_empty() {
                                            protocol::local_locale()
                                        } else {
                                            self.voice_android_locale.clone()
                                        },
                                        if self.voice_android_time_zone.is_empty() {
                                            protocol::local_time_zone()
                                        } else {
                                            self.voice_android_time_zone.clone()
                                        }
                                    ));
                                }
                            });
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui
                                .button("↺ Reset DSP to defaults")
                                .on_hover_text(
                                    "Restore all audio and DSP settings to their factory \
                                     defaults. Identity, server, and proxy settings are \
                                     not affected.",
                                )
                                .clicked()
                            {
                                self.voice_speaker_gain = 100.0;
                                self.voice_speaker_dsp = true;
                                self.voice_microphone_dsp = true;
                                self.voice_microphone_noise_suppression = true;
                                self.voice_microphone_gain = 100.0;
                                self.voice_echo_cancellation = true;
                                self.voice_noise_gate_threshold = 0.002;
                                self.voice_compressor_threshold = 0.12;
                                self.voice_compressor_ratio = 2.5;
                                self.voice_makeup_gain = 1.35;
                                self.voice_microphone_warmth = true;
                                self.voice_saturation_drive = 1.08;
                                self.voice_saturation_asymmetry = 1.15;
                                self.voice_saturation_bias = 0.0;
                                self.voice_saturation_profile =
                                    crate::audio::SaturationProfile::NaturalVoice;
                                self.voice_character_profile =
                                    crate::audio::VoiceCharacterProfile::Natural;
                                self.voice_presence_boost_db = 0.0;
                                self.voice_proximity_gain_db = 0.0;
                                self.voice_air_boost_db = 0.0;
                                self.voice_push_to_talk = false;
                                self.voice_auto_next = false;
                                preferences_changed = true;
                            }
                        });
                        if preferences_changed {
                            let _ = self
                                .worker
                                .commands
                                .send(Command::SaveVoicePreferences(self.voice_preferences()));
                        }
                        ui.separator();
                        egui::CollapsingHeader::new("Connection log")
                            .default_open(false)
                            .show(ui, |ui| {
                                ui.horizontal_wrapped(|ui| {
                                    ui.small(format!("{} entries", self.voice_log.len()));
                                    if ui
                                        .button("Copy channel diagnostics")
                                        .on_hover_text("Copy full channel metrics, audio status, and configuration help to clipboard")
                                        .clicked()
                                    {
                                        ui.ctx().copy_text(self.channel_diagnostics_text());
                                        self.voice_log
                                            .push("Copied channel diagnostic summary to clipboard.".into());
                                    }
                                    if ui.button("Open web audiochat").clicked() {
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(
                                            "https://nekto.me/audiochat",
                                        ));
                                        self.voice_log.push(
                                            "Opened https://nekto.me/audiochat in the browser.".into(),
                                        );
                                    }
                                });
                                show_log_viewer(
                                    ui,
                                    "voice-log-viewer",
                                    "voice-roulette",
                                    &mut self.voice_log,
                                    &mut self.voice_log_filter,
                                    &mut self.voice_log_auto_scroll,
                                    240.0,
                                );
                            });
                    });
                return;
            }
            self.show_text_chat(ui);
        });
        let needs_realtime_updates = if self.voice_tab {
            self.voice_in_call
                || self.voice_searching
                || self.voice_mic_test
                || self.voice_captcha_submitted
        } else {
            self.dialog_id.is_some() || self.searching || self.peer_typing
        };
        let repaint_millis = if needs_realtime_updates { 100 } else { 300 };
        context.request_repaint_after(std::time::Duration::from_millis(repaint_millis));
    }
}

impl Drop for NektoMeApp {
    fn drop(&mut self) {
        let _ = self.worker.commands.send(Command::Shutdown);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "cpal-audio")]
    use super::audio_device_label;
    use super::{
        Command, NektoMeApp, VoiceAgePreset, captcha_url, chat_captcha_details,
        chat_token_from_cdp, human_file_size, parse_emulator_profile, resolve_captcha_url,
        select_proxy_entry, structured_log_json, trim_log, voice_error_details,
        voice_reconnect_delay,
    };

    #[test]
    fn text_captcha_error_accepts_server_id_and_metadata() {
        let details = chat_captcha_details(&serde_json::json!({
            "id": 600,
            "description": "Требуется решить каптчу",
            "additional": {
                "leftChats": 0,
                "needChats": 5,
                "publicKey": "site-key"
            }
        }));
        assert_eq!(details, Some((0, 5, Some("site-key".into()))));
        assert!(
            chat_captcha_details(&serde_json::json!({
                "id": 401,
                "description": "First you need to login"
            }))
            .is_none()
        );
    }

    #[test]
    fn cdp_cookie_response_extracts_verified_text_token() {
        let token = "a".repeat(64);
        let response = serde_json::json!({
            "id": 2,
            "result": {"cookies": [
                {"name": "unrelated", "value": "x"},
                {"name": "chat_token_user", "value": token}
            ]}
        });
        assert_eq!(chat_token_from_cdp(&response), Some("a".repeat(64)));
    }

    #[test]
    fn proxy_selection_resolves_one_entry_at_every_index() {
        let pool = " socks5://first:1\n, socks5://second:2 ; socks5://third:3 ";
        assert_eq!(
            select_proxy_entry(pool, 0).as_deref(),
            Some("socks5://first:1")
        );
        assert_eq!(
            select_proxy_entry(pool, 1).as_deref(),
            Some("socks5://second:2")
        );
        assert_eq!(
            select_proxy_entry(pool, 2).as_deref(),
            Some("socks5://third:3")
        );
        assert_eq!(
            select_proxy_entry(pool, 99).as_deref(),
            Some("socks5://third:3")
        );
        assert_eq!(select_proxy_entry(" ", 0), None);
    }

    #[test]
    fn diagnostic_logs_keep_the_newest_entries() {
        let mut log = (0..(super::MAX_LOG_LINES + 3))
            .map(|index| index.to_string())
            .collect::<Vec<_>>();
        trim_log(&mut log);
        assert_eq!(log.len(), super::MAX_LOG_LINES);
        assert_eq!(log.first().map(String::as_str), Some("3"));
        assert_eq!(log.last().map(String::as_str), Some("502"));
    }

    #[test]
    fn structured_log_export_preserves_order_and_event_metadata() {
        let bytes = structured_log_json(
            "voice-roulette",
            &[
                "connected".into(),
                "event: peer-connect".into(),
                "connection failed".into(),
            ],
        )
        .expect("log should serialize");
        let document: serde_json::Value =
            serde_json::from_slice(&bytes).expect("export should be valid JSON");

        assert_eq!(document["schemaVersion"], 1);
        assert_eq!(document["source"], "voice-roulette");
        assert_eq!(document["entryCount"], 3);
        assert_eq!(document["entries"][0]["level"], "success");
        assert_eq!(document["entries"][1]["sequence"], 2);
        assert_eq!(document["entries"][1]["event"], "peer-connect");
        assert_eq!(document["entries"][2]["level"], "error");
    }

    #[cfg(feature = "webrtc-media")]
    #[test]
    fn voice_candidate_queue_deduplicates_and_is_bounded() {
        let mut pending = Vec::new();
        super::queue_voice_candidate(
            &mut pending,
            "peer-1".into(),
            "audio".into(),
            0,
            "candidate:1".into(),
        );
        super::queue_voice_candidate(
            &mut pending,
            "peer-1".into(),
            "audio".into(),
            0,
            "candidate:1".into(),
        );
        assert_eq!(pending.len(), 1);

        for index in 0..40 {
            super::queue_voice_candidate(
                &mut pending,
                "peer-1".into(),
                "audio".into(),
                index,
                format!("candidate:{index}"),
            );
        }
        assert_eq!(pending.len(), 32);
        assert_eq!(pending[0].2, 8);
    }

    #[cfg(feature = "webrtc-media")]
    #[test]
    fn stale_peer_events_do_not_match_active_peer() {
        assert!(super::event_matches_active_peer(
            &serde_json::json!({"connectionId": " peer-1 "}),
            Some("peer-1")
        ));
        assert!(!super::event_matches_active_peer(
            &serde_json::json!({"connectionId": "peer-2"}),
            Some("peer-1")
        ));
        assert!(super::event_matches_active_peer(
            &serde_json::json!({}),
            Some("peer-1")
        ));
        assert!(!super::event_matches_active_peer(
            &serde_json::json!({"connectionId": ""}),
            None
        ));
    }

    #[test]
    fn captcha_url_resolves_relative_paths_against_voice_endpoint() {
        assert_eq!(
            resolve_captcha_url("/captcha/image.png", "https://audio.example/androiduk/").unwrap(),
            "https://audio.example/captcha/image.png"
        );
        assert!(resolve_captcha_url("file:///tmp/captcha.png", "https://audio.example/").is_err());
    }

    #[test]
    fn captcha_url_uses_the_selected_endpoint_for_each_voice_mode() {
        assert_eq!(
            captcha_url("/captcha.png", Some("audio.example"), false).unwrap(),
            "https://audio.example/captcha.png"
        );
        assert_eq!(
            captcha_url("captcha.png", Some("audio.example"), true).unwrap(),
            "https://audio.example/captcha.png"
        );
    }

    #[test]
    fn emulator_profile_parser_accepts_legacy_preference_keys() {
        assert_eq!(
            parse_emulator_profile(r#"{"user_id":"google-device","connection_id_last":"conn-7"}"#)
                .unwrap(),
            ("google-device".to_owned(), Some("conn-7".to_owned()))
        );
        assert_eq!(
            parse_emulator_profile(r#"{"user_id":" device "}"#).unwrap(),
            ("device".to_owned(), None)
        );
        assert!(parse_emulator_profile(r#"{"connection_id_last":"conn"}"#).is_err());
    }

    #[test]
    fn peer_age_parser_accepts_multiple_ranges() {
        assert_eq!(
            NektoMeApp::parse_voice_age_ranges("18-24, 25–32; 33—40", ""),
            vec![(18, 24), (25, 32), (33, 40)]
        );
    }

    #[test]
    fn peer_age_parser_keeps_legacy_two_field_form() {
        assert_eq!(
            NektoMeApp::parse_voice_age_ranges("18", "24"),
            vec![(18, 24)]
        );
    }

    #[test]
    fn voice_age_presets_match_ranges_and_labels() {
        assert_eq!(VoiceAgePreset::Under18.label(), "до 17 лет");
        assert_eq!(VoiceAgePreset::Under18.range(), (0, 17));
        assert!(VoiceAgePreset::Under18.matches_range(0, 17));
        assert!(VoiceAgePreset::Under18.matches_range(1, 17));

        assert_eq!(VoiceAgePreset::From18To24.label(), "от 18 до 24 лет");
        assert_eq!(VoiceAgePreset::From18To24.range(), (18, 24));
        assert!(VoiceAgePreset::From18To24.matches_range(18, 24));

        assert_eq!(VoiceAgePreset::From25To32.label(), "от 25 до 32 лет");
        assert_eq!(VoiceAgePreset::From25To32.range(), (25, 32));
        assert!(VoiceAgePreset::From25To32.matches_range(25, 32));

        assert_eq!(VoiceAgePreset::Over33.label(), "старше 33 лет");
        assert_eq!(VoiceAgePreset::Over33.range(), (33, 99));
        assert!(VoiceAgePreset::Over33.matches_range(33, 99));
        assert!(VoiceAgePreset::Over33.matches_range(33, 120));
    }

    #[test]
    fn voice_age_preset_selection_and_minor_isolation() {
        let mut app = NektoMeApp::new();
        app.set_user_age_preset(Some(VoiceAgePreset::From18To24));
        assert_eq!(
            app.selected_user_age_preset(),
            Some(VoiceAgePreset::From18To24)
        );
        assert_eq!(app.voice_user_age_from, "18");
        assert_eq!(app.voice_user_age_to, "24");

        app.toggle_peer_age_preset(VoiceAgePreset::From18To24);
        app.toggle_peer_age_preset(VoiceAgePreset::From25To32);
        assert!(app.is_peer_age_preset_selected(VoiceAgePreset::From18To24));
        assert!(app.is_peer_age_preset_selected(VoiceAgePreset::From25To32));
        assert!(!app.is_peer_age_preset_selected(VoiceAgePreset::Under18));
        assert_eq!(app.voice_peer_age_from, "18-24, 25-32");

        app.toggle_peer_age_preset(VoiceAgePreset::Under18);
        assert_eq!(
            app.selected_user_age_preset(),
            Some(VoiceAgePreset::Under18)
        );
        assert!(app.is_peer_age_preset_selected(VoiceAgePreset::Under18));

        app.set_user_age_preset(Some(VoiceAgePreset::From25To32));
        assert_eq!(
            app.selected_user_age_preset(),
            Some(VoiceAgePreset::From25To32)
        );
        assert!(!app.is_peer_age_preset_selected(VoiceAgePreset::Under18));
    }

    #[test]
    fn voice_search_command_universal_matching_when_all_peer_ages_selected() {
        let mut app = NektoMeApp::new();
        app.voice_user_sex = "M".into();
        for &preset in &VoiceAgePreset::ALL {
            app.toggle_peer_age_preset(preset);
        }
        for &preset in &VoiceAgePreset::ALL {
            assert!(app.is_peer_age_preset_selected(preset));
        }

        let cmd = app.voice_search_command(None);
        if let Command::VoiceSearch { peer_ages, .. } = cmd {
            assert!(peer_ages.is_empty());
        } else {
            panic!("Expected VoiceSearch command");
        }
    }

    #[test]
    fn voice_search_command_universal_when_user_sex_is_any() {
        let mut app = NektoMeApp::new();
        app.voice_user_sex = "ANY".into();
        app.voice_peer_sex = "F".into();
        app.toggle_peer_age_preset(VoiceAgePreset::From18To24);

        let cmd = app.voice_search_command(None);
        if let Command::VoiceSearch {
            user_sex,
            peer_sex,
            user_age,
            peer_ages,
            ..
        } = cmd
        {
            assert_eq!(user_sex, "ANY");
            assert_eq!(peer_sex, "ANY");
            assert!(user_age.is_none());
            assert!(peer_ages.is_empty());
        } else {
            panic!("Expected VoiceSearch command");
        }
    }

    #[test]
    fn start_and_stop_voice_search_updates_state_and_logs() {
        let mut app = NektoMeApp::new();
        app.voice_connected = true;
        assert!(!app.voice_searching);

        app.start_voice_search();
        assert!(app.voice_searching);
        assert!(
            app.voice_log
                .iter()
                .any(|log| log.contains("Searching for a peer"))
        );

        app.stop_voice_search();
        assert!(!app.voice_searching);
        assert!(
            app.voice_log
                .iter()
                .any(|log| log.contains("Search stopped"))
        );
    }

    #[test]
    fn duplicate_chat_message_ids_are_not_added_twice() {
        let mut app = NektoMeApp::new();
        let message = serde_json::json!({
            "id": 7,
            "dialogId": 12,
            "senderId": 3,
            "message": "hello"
        });
        app.add_message(&message);
        app.add_message(&message);
        assert_eq!(app.messages.len(), 1);
    }

    #[test]
    fn chat_lines_preserve_timestamp_and_read_state() {
        let mut app = NektoMeApp::new();
        app.own_user_id = Some(3);
        app.add_message(&serde_json::json!({
            "id": 7,
            "dialogId": 12,
            "senderId": 3,
            "createTime": 1_700_000_000_000_i64,
            "read": true,
            "message": "hello"
        }));
        assert_eq!(app.messages[0].created_at, 1_700_000_000);
        assert!(app.messages[0].read);
        assert_eq!(app.messages[0].display_time().len(), 5);

        let mut missing_time = app.messages[0].clone();
        missing_time.created_at = 0;
        assert!(missing_time.display_time().is_empty());
    }

    #[test]
    fn chat_read_watermark_never_moves_backwards() {
        let mut app = NektoMeApp::new();
        app.add_message(&serde_json::json!({"id": 9, "message": "newer"}));
        app.add_message(&serde_json::json!({"id": 4, "message": "older"}));
        assert_eq!(app.last_message_id, Some(9));
    }

    #[test]
    fn voice_profile_user_agent_is_included_in_saved_preferences() {
        let mut app = NektoMeApp::new();
        app.voice_android_user_agent = "NektoMeAudio96/test-profile".into();
        app.voice_push_to_talk = true;
        assert_eq!(
            app.voice_preferences().android_user_agent,
            "NektoMeAudio96/test-profile"
        );
        assert!(app.voice_preferences().push_to_talk);
    }

    #[test]
    fn voice_reconnect_delay_backs_off_and_stays_bounded() {
        let delays = (0..8)
            .map(|attempt| voice_reconnect_delay(attempt).as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, vec![1, 2, 4, 8, 15, 15, 15, 15]);
    }

    #[test]
    fn recording_library_formats_file_sizes_without_rounding_up() {
        assert_eq!(human_file_size(512), "512 B");
        assert_eq!(human_file_size(1_536), "1.5 KiB");
        assert_eq!(human_file_size(1_572_864), "1.5 MiB");
    }

    #[test]
    fn diagnostics_bundle_omits_credentials_and_proxy_configuration() {
        let mut app = NektoMeApp::new();
        app.voice_web_token = "secret-web-token".into();
        app.voice_proxy_seller_api_key = "secret-api-key".into();
        app.voice_proxy_url = "http://user:secret@proxy.example:8080".into();
        let bundle = app.diagnostics_bundle();
        let encoded = serde_json::to_string(&bundle).unwrap();
        assert!(!encoded.contains("secret-web-token"));
        assert!(!encoded.contains("secret-api-key"));
        assert!(!encoded.contains("user:secret"));
        assert_eq!(bundle["schemaVersion"], 1);
        assert!(bundle["application"]["features"].is_object());
        assert!(bundle["logs"]["voiceRoulette"].is_array());
    }

    #[cfg(feature = "cpal-audio")]
    #[test]
    fn audio_device_label_explains_default_and_missing_choices() {
        let available = vec!["Studio Mic".to_owned()];
        assert_eq!(
            audio_device_label("", &available, Some("Built-in Mic")),
            "System default · Built-in Mic"
        );
        assert_eq!(
            audio_device_label("Studio Mic", &available, Some("Built-in Mic")),
            "Studio Mic"
        );
        assert_eq!(
            audio_device_label("Missing Mic", &available, Some("Built-in Mic")),
            "Missing Mic · unavailable; using default"
        );
    }

    #[test]
    fn malformed_chat_messages_are_ignored() {
        let mut app = NektoMeApp::new();
        app.add_message(&serde_json::json!({"id": 1, "message": ""}));
        app.add_message(&serde_json::json!({"id": 2, "message": "   "}));
        app.add_message(&serde_json::json!({"message": "missing id"}));
        assert!(app.messages.is_empty());
    }

    #[test]
    fn messages_from_other_dialogs_are_ignored() {
        let mut app = NektoMeApp::new();
        app.dialog_id = Some(12);
        app.add_message(&serde_json::json!({
            "id": 1,
            "dialogId": 99,
            "message": "stale"
        }));
        assert!(app.messages.is_empty());
    }

    #[test]
    fn voice_error_details_accept_nested_and_flat_shapes() {
        assert_eq!(
            voice_error_details(&serde_json::json!({
                "error": {"code": 201, "text": "IP blocked"}
            })),
            (Some("201".into()), Some("IP blocked".into()))
        );
        assert_eq!(
            voice_error_details(&serde_json::json!({
                "code": "E_CAPTCHA", "message": "try again"
            })),
            (Some("E_CAPTCHA".into()), Some("try again".into()))
        );
        assert_eq!(
            voice_error_details(&serde_json::json!({"error": "IP blocked"})),
            (None, Some("IP blocked".into()))
        );
    }

    #[test]
    fn voice_ban_text_matching_handles_cyrillic_case() {
        let (_, description) = voice_error_details(&serde_json::json!({
            "error": {"text": "ПОЛЬЗОВАТЕЛЬ ЗАБЛОКИРОВАН"}
        }));
        assert!(
            description
                .expect("error description")
                .to_lowercase()
                .contains("заблокирован")
        );
    }
}
