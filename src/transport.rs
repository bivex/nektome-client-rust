use crate::protocol::{SERVER, parse_socket_event};
use base64::Engine as _;
use futures_util::{Sink, SinkExt, StreamExt};
use percent_encoding::percent_decode_str;
use std::{
    io,
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::time::{Duration, timeout};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, oneshot, watch},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, client_async_tls_with_config, connect_async,
    tungstenite::{
        Message as WsMessage,
        client::IntoClientRequest,
        handshake::client::Response,
        http::{HeaderValue, Request},
    },
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const VOICE_CANDIDATE_TIMEOUT: Duration = Duration::from_secs(20);
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const DEFAULT_VOICE_KEEPALIVE_INTERVAL: Duration = Duration::from_millis(4000);

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn engine_io_pong(frame: &str) -> Option<String> {
    frame.strip_prefix('2').map(|payload| format!("3{payload}"))
}

fn parse_engine_io_ping_interval(frame: &str) -> Option<Duration> {
    let payload = frame.strip_prefix('0')?;
    let value = serde_json::from_str::<serde_json::Value>(payload).ok()?;
    let interval_ms = value
        .get("pingInterval")
        .and_then(serde_json::Value::as_u64)?;
    let target_ms = (interval_ms.saturating_mul(4) / 5).clamp(1000, 20000);
    Some(Duration::from_millis(target_ms))
}

async fn send_websocket_frame<S>(sink: &mut S, frame: WsMessage) -> Result<(), String>
where
    S: Sink<WsMessage> + Unpin,
    S::Error: std::fmt::Display,
{
    match timeout(SEND_TIMEOUT, sink.send(frame)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("WebSocket write timed out".into()),
    }
}

fn configured_endpoint(default: &str, variable: &str) -> String {
    select_endpoint(
        default,
        std::env::var(variable)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .as_deref(),
    )
}

fn select_endpoint(default: &str, override_value: Option<&str>) -> String {
    override_value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(default)
        .to_owned()
}

fn normalize_socket_endpoint(value: &str, default_path: &str, engine_io: bool) -> String {
    let input = value.trim();
    let parse_input = if input.contains("://") {
        input.to_owned()
    } else {
        format!("wss://{}", input.trim_end_matches('/'))
    };
    let Ok(mut endpoint) = url::Url::parse(&parse_input) else {
        return input.to_owned();
    };
    match endpoint.scheme() {
        "http" => {
            let _ = endpoint.set_scheme("ws");
        }
        "https" => {
            let _ = endpoint.set_scheme("wss");
        }
        "ws" | "wss" => {}
        _ => return input.to_owned(),
    }
    if endpoint.path().trim_matches('/').is_empty()
        || endpoint.path().trim_end_matches('/') == default_path.trim_end_matches('/')
    {
        endpoint.set_path(default_path);
    }
    if engine_io {
        let has_eio = endpoint.query_pairs().any(|(key, _)| key == "EIO");
        let has_transport = endpoint.query_pairs().any(|(key, _)| key == "transport");
        if !has_eio {
            endpoint.query_pairs_mut().append_pair("EIO", "3");
        }
        if !has_transport {
            endpoint
                .query_pairs_mut()
                .append_pair("transport", "websocket");
        }
    }
    endpoint.to_string()
}

fn normalize_web_endpoint(value: &str) -> String {
    normalize_socket_endpoint(value, "/websocket/", true)
}

fn normalize_chat_endpoint(value: &str) -> String {
    normalize_socket_endpoint(value, "/android/", true)
}

fn proxy_target_authority(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn connect_socket(
    request: Request<()>,
    proxy_override: Option<&str>,
) -> Result<(Socket, Response), Box<dyn std::error::Error + Send + Sync>> {
    let proxy_value = proxy_override
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("NEKTOME_PROXY").ok());
    let Some(proxy_value) = proxy_value else {
        return Ok(connect_async(request).await?);
    };
    let target_url = url::Url::parse(request.uri().to_string().as_str())?;
    let target_host = target_url
        .host_str()
        .ok_or("WebSocket request has no host")?;
    let target_port = target_url
        .port_or_known_default()
        .ok_or("WebSocket request has no port")?;
    let stream = open_tcp_tunnel(Some(proxy_value.as_str()), target_host, target_port).await?;
    Ok(client_async_tls_with_config(request, stream, None, None).await?)
}

fn decode_proxy_credential(
    value: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    Ok(percent_decode_str(value)
        .decode_utf8()?
        .into_owned()
        .into_bytes())
}

/// Open a TCP stream to `target_host:target_port`, optionally through one of
/// the proxy formats accepted by the client. This is shared by signaling and
/// the TURN-over-TCP media forwarder.
pub(crate) async fn open_tcp_tunnel(
    proxy_override: Option<&str>,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream, Box<dyn std::error::Error + Send + Sync>> {
    let Some(proxy_value) = proxy_override
        .filter(|value| !value.trim().is_empty())
        .map(normalize_proxy_url)
    else {
        return Ok(TcpStream::connect((target_host, target_port)).await?);
    };
    let proxy = url::Url::parse(&proxy_value)?;
    let proxy_host = proxy.host_str().ok_or("proxy has no host")?;
    let proxy_port = proxy.port_or_known_default().ok_or("proxy has no port")?;
    let mut stream = TcpStream::connect((proxy_host, proxy_port)).await?;

    match proxy.scheme() {
        "http" | "https" => {
            let target = proxy_target_authority(target_host, target_port);
            let mut connect_request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
            if !proxy.username().is_empty() {
                let username = decode_proxy_credential(proxy.username())?;
                let password = decode_proxy_credential(proxy.password().unwrap_or(""))?;
                let credentials = format!(
                    "{}:{}",
                    String::from_utf8(username)?,
                    String::from_utf8(password)?
                );
                connect_request.push_str("Proxy-Authorization: Basic ");
                connect_request
                    .push_str(&base64::engine::general_purpose::STANDARD.encode(credentials));
                connect_request.push_str("\r\n");
            }
            connect_request.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
            stream.write_all(connect_request.as_bytes()).await?;
            let response = read_proxy_headers(&mut stream).await?;
            let response_text = String::from_utf8_lossy(&response);
            let status_line = response_text.lines().next().unwrap_or_default();
            if !status_line
                .split_whitespace()
                .nth(1)
                .is_some_and(|code| code == "200")
            {
                return Err(format!("proxy CONNECT failed: {status_line}").into());
            }
        }
        "socks5" | "socks5h" => {
            connect_socks5(&mut stream, &proxy, target_host, target_port).await?;
        }
        _ => return Err("proxy supports http(s), socks5, and socks5h".into()),
    }

    Ok(stream)
}

async fn read_proxy_headers(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut response = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    while response.len() < 16 * 1024 {
        stream.read_exact(&mut byte).await?;
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            return Ok(response);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "proxy response headers exceed 16 KiB",
    ))
}

async fn connect_socks5(
    stream: &mut TcpStream,
    proxy: &url::Url,
    target_host: &str,
    target_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let has_auth = !proxy.username().is_empty();
    stream
        .write_all(if has_auth { &[5, 2, 0, 2] } else { &[5, 1, 0] })
        .await?;
    let mut selected = [0u8; 2];
    stream.read_exact(&mut selected).await?;
    if selected[0] != 5 {
        return Err("invalid SOCKS5 proxy response".into());
    }
    if selected[1] == 2 {
        let username = decode_proxy_credential(proxy.username())?;
        let password = decode_proxy_credential(proxy.password().unwrap_or(""))?;
        if username.len() > u8::MAX as usize || password.len() > u8::MAX as usize {
            return Err("SOCKS5 proxy credentials are too long".into());
        }
        let mut auth = Vec::with_capacity(3 + username.len() + password.len());
        auth.extend([1, username.len() as u8]);
        auth.extend(username);
        auth.extend([password.len() as u8]);
        auth.extend(password);
        stream.write_all(&auth).await?;
        let mut result = [0u8; 2];
        stream.read_exact(&mut result).await?;
        if result != [1, 0] {
            return Err("SOCKS5 proxy authentication failed".into());
        }
    } else if selected[1] != 0 {
        return Err("SOCKS5 proxy offered no supported authentication method".into());
    }

    let (address_type, address) =
        socks5_target_address(proxy.scheme(), target_host, target_port).await?;
    let mut connect = Vec::with_capacity(7 + address.len());
    connect.extend([5, 1, 0, address_type]);
    if address_type == 3 {
        connect.push(address.len() as u8);
    }
    connect.extend(address);
    connect.extend(target_port.to_be_bytes());
    stream.write_all(&connect).await?;

    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[0] != 5 || header[1] != 0 {
        return Err(format!("SOCKS5 CONNECT failed with status {}", header[1]).into());
    }
    let address_len = match header[3] {
        1 => 4,
        3 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await?;
            usize::from(length[0])
        }
        4 => 16,
        _ => return Err("invalid SOCKS5 address type".into()),
    };
    let mut ignored = vec![0u8; address_len + 2];
    stream.read_exact(&mut ignored).await?;
    Ok(())
}

async fn socks5_target_address(
    proxy_scheme: &str,
    target_host: &str,
    target_port: u16,
) -> Result<(u8, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    if proxy_scheme == "socks5h" {
        let host = target_host.as_bytes();
        if host.len() > u8::MAX as usize {
            return Err("WebSocket host name is too long for SOCKS5".into());
        }
        return Ok((3, host.to_vec()));
    }

    if let Ok(address) = target_host.parse::<IpAddr>() {
        return Ok(match address {
            IpAddr::V4(address) => (1, address.octets().to_vec()),
            IpAddr::V6(address) => (4, address.octets().to_vec()),
        });
    }
    let address = tokio::net::lookup_host((target_host, target_port))
        .await?
        .next()
        .ok_or("SOCKS5 local DNS resolution returned no addresses")?
        .ip();
    Ok(match address {
        IpAddr::V4(address) => (1, address.octets().to_vec()),
        IpAddr::V6(address) => (4, address.octets().to_vec()),
    })
}

pub struct ChatTransport {
    outbound: mpsc::Sender<String>,
    shutdown: watch::Sender<bool>,
}

#[derive(Clone)]
pub struct VoiceTransport {
    outbound: mpsc::Sender<String>,
    shutdown: watch::Sender<bool>,
    proxy: Option<String>,
}

#[derive(Clone)]
pub struct WebVoiceTransport {
    outbound: mpsc::Sender<String>,
    shutdown: watch::Sender<bool>,
    proxy: Option<String>,
}

#[derive(Clone)]
pub enum VoiceTransportClient {
    Android(VoiceTransport),
    Web(WebVoiceTransport),
}

impl VoiceTransportClient {
    pub const DEFAULT_ANDROID_USER_AGENT: &str =
        "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S918B Build/UP1A.231005.007)";
    pub const DEFAULT_WEB_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

    pub fn random_android_user_agent() -> &'static str {
        const PROFILES: &[&str] = &[
            // Samsung flagships and high-volume devices.
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S928B Build/UP1A.240305.011)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S926B Build/UP1A.240305.011)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S921B Build/UP1A.240305.011)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S918B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S916B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S911B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-S908B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; SM-G998B Build/TP1A.220624.014)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-F731B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-A556B Build/UP1A.240305.011)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; SM-A546B Build/UP1A.231005.007)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; SM-A536B Build/TP1A.220624.014)",
            // Google Pixel devices.
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; Pixel 8 Pro Build/UQ1A.240205.002)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; Pixel 8 Build/UQ1A.240205.002)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; Pixel 7 Pro Build/UQ1A.240105.004)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; Pixel 7a Build/UQ1A.240205.002)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; Pixel 7 Build/TQ3A.230805.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; Pixel 6a Build/TQ3A.230901.001)",
            // Xiaomi, OnePlus, Realme and other common Android devices.
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; 23127PN0CG Build/UKQ1.230804.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; 24030PN60G Build/UKQ1.231008.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; 23078PND5G Build/UKQ1.230804.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; CPH2581 Build/UKQ1.230924.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; CPH2449 Build/UKQ1.230804.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; RMX3840 Build/UKQ1.230924.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; RMX3709 Build/TP1A.220905.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; V2318 Build/UKQ1.230924.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; V2250 Build/TP1A.220905.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 14; CPH2499 Build/UKQ1.230924.001)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; CK8n Build/TP1A.220624.014)",
            "NektoMeAudio96/2.1.0 (Linux; U; Android 13; moto g84 5G Build/T3SC33.16-56-7)",
        ];
        PROFILES[rand::random_range(0..PROFILES.len())]
    }

    pub fn random_android_locale() -> &'static str {
        const LOCALES: &[&str] = &["ru", "uk", "en"];
        LOCALES[rand::random_range(0..LOCALES.len())]
    }

    pub fn random_android_time_zone() -> &'static str {
        const TIME_ZONES: &[&str] = &[
            "Europe/Kyiv",
            "Europe/Warsaw",
            "Europe/Bucharest",
            "Europe/Chisinau",
            "Europe/Prague",
            "Europe/Sofia",
            "Europe/Vilnius",
            "Europe/Riga",
            "Europe/Tallinn",
            "Europe/Minsk",
            "Europe/Moscow",
            "Asia/Almaty",
            "Asia/Tashkent",
            "Asia/Tbilisi",
            "Asia/Yerevan",
        ];
        TIME_ZONES[rand::random_range(0..TIME_ZONES.len())]
    }

    pub async fn connect(
        web: bool,
        notices: mpsc::Sender<String>,
        proxy: Option<&str>,
        user_agent: Option<&str>,
        android_locale: Option<&str>,
        server_override: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let candidates = proxy_candidates(proxy);
        let candidate_count = candidates.len();
        let mut errors = Vec::new();
        for (index, candidate) in candidates.into_iter().enumerate() {
            let result = timeout(VOICE_CANDIDATE_TIMEOUT, async {
                if web {
                    WebVoiceTransport::connect(
                        notices.clone(),
                        candidate.as_deref(),
                        server_override,
                    )
                    .await
                    .map(Self::Web)
                } else {
                    VoiceTransport::connect(
                        notices.clone(),
                        candidate.as_deref(),
                        user_agent.unwrap_or(Self::DEFAULT_ANDROID_USER_AGENT),
                        android_locale,
                        server_override,
                    )
                    .await
                    .map(Self::Android)
                }
            })
            .await
            .unwrap_or_else(|_| Err("voice transport candidate timed out".into()));
            match result {
                Ok(client) => return Ok(client),
                Err(error) => {
                    let error_text = error.to_string();
                    let detail = redact_proxy_error(&error_text, candidate.as_deref());
                    let _ = notices.try_send(format!(
                        "voice:proxy-attempt-failed:{}/{}:{detail}",
                        index + 1,
                        candidate_count
                    ));
                    errors.push(format!("candidate {}: {detail}", index + 1));
                }
            }
        }
        if errors.is_empty() {
            Err("no proxy candidates configured".into())
        } else {
            Err(format!(
                "all voice transport candidates failed: {}",
                errors.join("; ")
            )
            .into())
        }
    }

    pub async fn send(
        &self,
        payload: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match self {
            Self::Android(client) => client.send(payload).await,
            Self::Web(client) => client.send(payload).await,
        }
    }

    pub fn is_web(&self) -> bool {
        matches!(self, Self::Web(_))
    }

    /// Return the single proxy candidate that successfully established this
    /// connection. This matters when a pool was supplied: media forwarding
    /// must use the winning candidate, not the original pool string.
    pub fn active_proxy(&self) -> Option<&str> {
        match self {
            Self::Android(client) => client.proxy.as_deref(),
            Self::Web(client) => client.proxy.as_deref(),
        }
    }

    pub fn shutdown(&self) {
        match self {
            Self::Android(client) => client.shutdown(),
            Self::Web(client) => client.shutdown(),
        }
    }
}

fn proxy_candidates(input: Option<&str>) -> Vec<Option<String>> {
    let input = input
        .map(str::to_owned)
        .or_else(|| std::env::var("NEKTOME_PROXY").ok());
    let Some(input) = input else {
        return vec![None];
    };
    let candidates = proxy_entries(&input)
        .into_iter()
        .map(|value| Some(normalize_proxy_url(&value)))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        vec![None]
    } else {
        candidates
    }
}

/// Normalize the user-facing proxy syntax accepted by the legacy client.
/// Bare `host:port` entries are HTTP CONNECT proxies by default.
pub fn normalize_proxy_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.contains("://") {
        trimmed.to_owned()
    } else {
        format!("http://{trimmed}")
    }
}

/// Split the user-facing proxy pool syntax into ordered proxy URLs.
pub fn proxy_entries(input: &str) -> Vec<String> {
    input
        .split(['\r', '\n', ',', ';'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn redact_proxy_error(detail: &str, candidate: Option<&str>) -> String {
    let Some(candidate) = candidate else {
        return detail.to_owned();
    };
    let normalized = normalize_proxy_url(candidate);
    let Ok(mut url) = url::Url::parse(&normalized) else {
        return detail.to_owned();
    };
    if url.username().is_empty() && url.password().is_none() {
        return detail.to_owned();
    }
    let _ = url.set_username("redacted");
    let _ = url.set_password(Some("redacted"));
    detail
        .replace(candidate, url.as_str())
        .replace(&normalized, url.as_str())
}

impl ChatTransport {
    pub async fn connect(
        notices: mpsc::Sender<String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = normalize_chat_endpoint(&configured_endpoint(SERVER, "NEKTOME_CHAT_SERVER"));
        let mut request = endpoint.into_client_request()?;
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static("https://nekto.me"));
        request.headers_mut().insert(
            "User-Agent",
            HeaderValue::from_static(
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36",
            ),
        );
        let connection = timeout(HANDSHAKE_TIMEOUT, connect_socket(request, None))
            .await
            .map_err(|_| "chat WebSocket connection timed out")?;
        let (socket, _) = connection.map_err(|error| {
            let error_text = error.to_string();
            let proxy = std::env::var("NEKTOME_PROXY").ok();
            io::Error::other(redact_proxy_error(&error_text, proxy.as_deref()))
        })?;
        let (mut write, mut read) = socket.split();
        let (outbound, mut rx) = mpsc::channel::<String>(32);
        let control = outbound.clone();
        let (frame_tx, mut frame_rx) = mpsc::channel::<WsMessage>(8);
        let reader_frame_tx = frame_tx.clone();
        let (shutdown, _) = watch::channel(false);
        let transport_failed = Arc::new(AtomicBool::new(false));
        let writer_transport_failed = Arc::clone(&transport_failed);
        let writer_shutdown_signal = shutdown.clone();
        let mut writer_shutdown = shutdown.subscribe();
        let writer_notices = notices.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_shutdown.changed() => break,
                    frame = frame_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if let Err(error) = send_websocket_frame(&mut write, frame).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                    payload = rx.recv() => {
                        let Some(payload) = payload else { break };
                        if let Err(error) = send_websocket_frame(
                            &mut write,
                            WsMessage::Text(payload.into()),
                        ).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                }
            }
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let reader_transport_failed = Arc::clone(&transport_failed);
        let reader_shutdown_signal = shutdown.clone();
        let mut reader_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let mut namespace_ready = false;
            let mut intentionally_shutdown = false;
            loop {
                let result = tokio::select! {
                    _ = reader_shutdown.changed() => {
                        intentionally_shutdown = !reader_transport_failed.load(Ordering::Relaxed);
                        break;
                    },
                    result = read.next() => result,
                };
                let Some(result) = result else { break };
                let text = match result {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Ping(payload)) => {
                        let _ = reader_frame_tx.send(WsMessage::Pong(payload)).await;
                        continue;
                    }
                    Ok(WsMessage::Close(frame)) => {
                        let _ = reader_frame_tx.send(WsMessage::Close(frame.clone())).await;
                        let reason = frame
                            .map(|frame| frame.reason.to_string())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "server closed the connection".into());
                        let _ = notices.send(format!("transport-closed:{reason}")).await;
                        break;
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        let _ = notices.send(format!("transport-error:{error}")).await;
                        break;
                    }
                };
                if text.starts_with('0') {
                    let _ = notices.send("connected".into()).await;
                    let _ = control.send("40".into()).await;
                } else if text == "1" {
                    let _ = notices
                        .send("transport-closed:Engine.IO server close".into())
                        .await;
                    break;
                } else if let Some(pong) = engine_io_pong(&text) {
                    let _ = control.send(pong).await;
                }
                if text == "40" && !namespace_ready {
                    namespace_ready = true;
                    let _ = notices.send("ready".into()).await;
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(());
                    }
                }
                if let Some(envelope) = parse_socket_event(&text)
                    && let Ok(serialized) = serde_json::to_string(&envelope)
                {
                    let _ = notices.send(serialized).await;
                }
            }
            let _ = reader_shutdown_signal.send(true);
            if !intentionally_shutdown {
                let _ = notices.send("disconnected".into()).await;
            }
        });
        match timeout(HANDSHAKE_TIMEOUT, ready_rx).await {
            Err(_) => {
                let _ = shutdown.send(true);
                return Err("chat Engine.IO handshake timed out".into());
            }
            Ok(Err(_)) => {
                let _ = shutdown.send(true);
                return Err("chat Engine.IO handshake closed before ready".into());
            }
            Ok(Ok(())) => {}
        }
        Ok(Self { outbound, shutdown })
    }
    pub async fn send(
        &self,
        payload: String,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match timeout(SEND_TIMEOUT, self.outbound.send(payload)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.shutdown();
                Err(error.into())
            }
            Err(_) => {
                self.shutdown();
                Err("chat transport send timed out".into())
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for ChatTransport {
    fn drop(&mut self) {
        // Keep the socket tasks bounded even if a caller replaces the
        // transport without explicitly calling `shutdown` first.
        let _ = self.shutdown.send(true);
    }
}

impl VoiceTransport {
    pub async fn connect(
        notices: tokio::sync::mpsc::Sender<String>,
        proxy: Option<&str>,
        user_agent: &str,
        locale: Option<&str>,
        server_override: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut request =
            crate::voice::resolve_android_server_with_override(proxy, server_override)
                .await?
                .into_client_request()?;
        let headers = request.headers_mut();
        let language = locale
            .map(str::to_owned)
            .unwrap_or_else(crate::protocol::local_locale);
        headers.insert("Android-Language", HeaderValue::from_str(&language)?);
        headers.insert("App-Android-Code", HeaderValue::from_static("96"));
        headers.insert("App-Android-Version", HeaderValue::from_static("1.7.1"));
        headers.insert("NektoMe-Chat-Version", HeaderValue::from_static("2"));
        headers.insert("Origin", HeaderValue::from_static("https://nekto.me"));
        headers.insert("User-Agent", HeaderValue::from_str(user_agent)?);
        let (socket, _) = timeout(HANDSHAKE_TIMEOUT, connect_socket(request, proxy))
            .await
            .map_err(|_| "voice WebSocket connection timed out")??;
        let (mut write, mut read) = socket.split();
        let (outbound, mut rx) = mpsc::channel::<String>(32);
        let control = outbound.clone();
        let (frame_tx, mut frame_rx) = mpsc::channel::<WsMessage>(8);
        let reader_frame_tx = frame_tx.clone();
        let (shutdown, _) = watch::channel(false);
        let (keepalive_tx, mut keepalive_rx) = watch::channel(DEFAULT_VOICE_KEEPALIVE_INTERVAL);
        let transport_failed = Arc::new(AtomicBool::new(false));
        let writer_transport_failed = Arc::clone(&transport_failed);
        let writer_shutdown_signal = shutdown.clone();
        let mut writer_shutdown = shutdown.subscribe();
        let writer_notices = notices.clone();
        tokio::spawn(async move {
            let mut interval_duration = DEFAULT_VOICE_KEEPALIVE_INTERVAL;
            let mut keepalive = tokio::time::interval(interval_duration);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = writer_shutdown.changed() => break,
                    Ok(()) = keepalive_rx.changed() => {
                        let new_duration = *keepalive_rx.borrow_and_update();
                        if new_duration != interval_duration {
                            interval_duration = new_duration;
                            keepalive = tokio::time::interval(interval_duration);
                            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        }
                    }
                    frame = frame_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if let Err(error) = send_websocket_frame(&mut write, frame).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if let Err(error) = send_websocket_frame(
                            &mut write,
                            WsMessage::Text("2".into()),
                        ).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                    payload = rx.recv() => {
                        let Some(payload) = payload else { break };
                        if let Err(error) = send_websocket_frame(
                            &mut write,
                            WsMessage::Text(payload.into()),
                        ).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                }
            }
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let reader_transport_failed = Arc::clone(&transport_failed);
        let reader_shutdown_signal = shutdown.clone();
        let mut reader_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let mut namespace_ready = false;
            let mut intentionally_shutdown = false;
            loop {
                let result = tokio::select! {
                    _ = reader_shutdown.changed() => {
                        intentionally_shutdown = !reader_transport_failed.load(Ordering::Relaxed);
                        break;
                    },
                    result = read.next() => result,
                };
                let Some(result) = result else { break };
                let text = match result {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Ping(payload)) => {
                        let _ = reader_frame_tx.send(WsMessage::Pong(payload)).await;
                        continue;
                    }
                    Ok(WsMessage::Close(frame)) => {
                        let _ = reader_frame_tx.send(WsMessage::Close(frame.clone())).await;
                        let reason = frame
                            .map(|frame| frame.reason.to_string())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "server closed the connection".into());
                        let _ = notices
                            .send(format!("voice:transport-closed:{reason}"))
                            .await;
                        break;
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        let _ = notices.send(format!("voice:transport-error:{error}")).await;
                        break;
                    }
                };
                if text.starts_with('0') {
                    if let Some(interval) = parse_engine_io_ping_interval(&text) {
                        let _ = keepalive_tx.send(interval);
                    }
                    let _ = notices.send("voice:connected".into()).await;
                    let _ = control.send("40".into()).await;
                } else if text == "1" {
                    let _ = notices
                        .send("voice:transport-closed:Engine.IO server close".into())
                        .await;
                    break;
                } else if let Some(pong) = engine_io_pong(&text) {
                    let _ = control.send(pong).await;
                } else if text.starts_with('3') {
                    // Engine.IO pong response from server
                } else if text == "40" {
                    if !namespace_ready {
                        namespace_ready = true;
                        let _ = notices.send("voice:ready".into()).await;
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(());
                        }
                    }
                } else if text == "41" {
                    let _ = notices
                        .send("voice:transport-closed:Socket.IO namespace disconnected".into())
                        .await;
                    break;
                } else if let Some(event) = text.strip_prefix("42")
                    && let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(event)
                    && items.first().and_then(serde_json::Value::as_str) == Some("event")
                    && let Some(payload) = items.get(1)
                {
                    let _ = notices.send(format!("voice:{}", payload)).await;
                }
            }
            let _ = reader_shutdown_signal.send(true);
            if !intentionally_shutdown {
                let _ = notices.send("voice:disconnected".into()).await;
            }
        });
        match timeout(HANDSHAKE_TIMEOUT, ready_rx).await {
            Err(_) => {
                let _ = shutdown.send(true);
                return Err("voice Engine.IO handshake timed out".into());
            }
            Ok(Err(_)) => {
                let _ = shutdown.send(true);
                return Err("voice Engine.IO handshake closed before ready".into());
            }
            Ok(Ok(())) => {}
        }
        Ok(Self {
            outbound,
            shutdown,
            proxy: proxy.map(normalize_proxy_url),
        })
    }

    pub async fn send(
        &self,
        payload: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let frame = serde_json::to_string(&[serde_json::Value::String("event".into()), payload])?;
        match timeout(SEND_TIMEOUT, self.outbound.send(format!("42{frame}"))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.shutdown();
                Err(error.into())
            }
            Err(_) => {
                self.shutdown();
                Err("voice transport send timed out".into())
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl WebVoiceTransport {
    const SERVER: &str = "wss://audio.nekto.me/websocket/";

    pub async fn connect(
        notices: mpsc::Sender<String>,
        proxy: Option<&str>,
        server_override: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let endpoint = server_override
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(normalize_web_endpoint)
            .unwrap_or_else(|| {
                normalize_web_endpoint(&configured_endpoint(
                    Self::SERVER,
                    "NEKTOME_WEB_VOICE_SERVER",
                ))
            });
        let mut request = endpoint.into_client_request()?;
        let headers = request.headers_mut();
        headers.insert(
            "User-Agent",
            HeaderValue::from_static(VoiceTransportClient::DEFAULT_WEB_USER_AGENT),
        );
        headers.insert("Origin", HeaderValue::from_static("https://nekto.me"));
        headers.insert(
            "Referer",
            HeaderValue::from_static("https://nekto.me/audiochat"),
        );
        headers.insert("Pragma", HeaderValue::from_static("no-cache"));
        headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
        let (socket, _) = timeout(HANDSHAKE_TIMEOUT, connect_socket(request, proxy))
            .await
            .map_err(|_| "web voice WebSocket connection timed out")??;
        let (mut write, mut read) = socket.split();
        let (outbound, mut rx) = mpsc::channel::<String>(32);
        let control = outbound.clone();
        let (frame_tx, mut frame_rx) = mpsc::channel::<WsMessage>(8);
        let reader_frame_tx = frame_tx.clone();
        let (shutdown, _) = watch::channel(false);
        let (keepalive_tx, mut keepalive_rx) = watch::channel(DEFAULT_VOICE_KEEPALIVE_INTERVAL);
        let transport_failed = Arc::new(AtomicBool::new(false));
        let writer_transport_failed = Arc::clone(&transport_failed);
        let writer_shutdown_signal = shutdown.clone();
        let mut writer_shutdown = shutdown.subscribe();
        let writer_notices = notices.clone();
        tokio::spawn(async move {
            let mut interval_duration = DEFAULT_VOICE_KEEPALIVE_INTERVAL;
            let mut keepalive = tokio::time::interval(interval_duration);
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = writer_shutdown.changed() => break,
                    Ok(()) = keepalive_rx.changed() => {
                        let new_duration = *keepalive_rx.borrow_and_update();
                        if new_duration != interval_duration {
                            interval_duration = new_duration;
                            keepalive = tokio::time::interval(interval_duration);
                            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        }
                    }
                    frame = frame_rx.recv() => {
                        let Some(frame) = frame else { break };
                        if let Err(error) = send_websocket_frame(&mut write, frame).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if let Err(error) = send_websocket_frame(
                            &mut write,
                            WsMessage::Text("2".into()),
                        ).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    },
                    payload = rx.recv() => {
                        let Some(payload) = payload else { break };
                        if let Err(error) = send_websocket_frame(
                            &mut write,
                            WsMessage::Text(payload.into()),
                        ).await {
                            writer_transport_failed.store(true, Ordering::Relaxed);
                            let _ = writer_notices
                                .send(format!("voice:transport-error:{error}"))
                                .await;
                            let _ = writer_shutdown_signal.send(true);
                            break;
                        }
                    }
                }
            }
        });
        let (ready_tx, ready_rx) = oneshot::channel();
        let reader_transport_failed = Arc::clone(&transport_failed);
        let reader_shutdown_signal = shutdown.clone();
        let mut reader_shutdown = shutdown.subscribe();
        tokio::spawn(async move {
            let mut ready_tx = Some(ready_tx);
            let mut namespace_ready = false;
            let mut intentionally_shutdown = false;
            loop {
                let result = tokio::select! {
                    _ = reader_shutdown.changed() => {
                        intentionally_shutdown = !reader_transport_failed.load(Ordering::Relaxed);
                        break;
                    },
                    result = read.next() => result,
                };
                let Some(result) = result else { break };
                let text = match result {
                    Ok(WsMessage::Text(text)) => text,
                    Ok(WsMessage::Ping(payload)) => {
                        let _ = reader_frame_tx.send(WsMessage::Pong(payload)).await;
                        continue;
                    }
                    Ok(WsMessage::Close(frame)) => {
                        let _ = reader_frame_tx.send(WsMessage::Close(frame.clone())).await;
                        let reason = frame
                            .map(|frame| frame.reason.to_string())
                            .filter(|reason| !reason.is_empty())
                            .unwrap_or_else(|| "server closed the connection".into());
                        let _ = notices
                            .send(format!("voice:transport-closed:{reason}"))
                            .await;
                        break;
                    }
                    Ok(_) => continue,
                    Err(error) => {
                        let _ = notices.send(format!("voice:transport-error:{error}")).await;
                        break;
                    }
                };
                if text.starts_with('0') {
                    if let Some(interval) = parse_engine_io_ping_interval(&text) {
                        let _ = keepalive_tx.send(interval);
                    }
                    let _ = notices.send("voice:connected".into()).await;
                    let _ = control.send("40".into()).await;
                } else if text == "1" {
                    let _ = notices
                        .send("voice:transport-closed:Engine.IO server close".into())
                        .await;
                    break;
                } else if let Some(pong) = engine_io_pong(&text) {
                    let _ = control.send(pong).await;
                } else if text.starts_with('3') {
                    // Engine.IO pong response from server
                } else if text == "40" {
                    if !namespace_ready {
                        namespace_ready = true;
                        let _ = notices.send("voice:ready".into()).await;
                        if let Some(ready_tx) = ready_tx.take() {
                            let _ = ready_tx.send(());
                        }
                    }
                } else if text == "41" {
                    let _ = notices
                        .send("voice:transport-closed:Socket.IO namespace disconnected".into())
                        .await;
                    break;
                } else if let Some(event) = text.strip_prefix("42")
                    && let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(event)
                    && items.first().and_then(serde_json::Value::as_str) == Some("event")
                    && let Some(payload) = items.get(1)
                {
                    let _ = notices.send(format!("voice:{}", payload)).await;
                }
            }
            let _ = reader_shutdown_signal.send(true);
            if !intentionally_shutdown {
                let _ = notices.send("voice:disconnected".into()).await;
            }
        });
        match timeout(HANDSHAKE_TIMEOUT, ready_rx).await {
            Err(_) => {
                let _ = shutdown.send(true);
                return Err("web voice Engine.IO handshake timed out".into());
            }
            Ok(Err(_)) => {
                let _ = shutdown.send(true);
                return Err("web voice Engine.IO handshake closed before ready".into());
            }
            Ok(Ok(())) => {}
        }
        Ok(Self {
            outbound,
            shutdown,
            proxy: proxy.map(normalize_proxy_url),
        })
    }

    pub async fn send(
        &self,
        payload: serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let frame = serde_json::to_string(&[serde_json::Value::String("event".into()), payload])?;
        match timeout(SEND_TIMEOUT, self.outbound.send(format!("42{frame}"))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.shutdown();
                Err(error.into())
            }
            Err(_) => {
                self.shutdown();
                Err("web voice transport send timed out".into())
            }
        }
    }

    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ChatTransport, VoiceTransport, VoiceTransportClient, WebVoiceTransport,
        decode_proxy_credential, engine_io_pong, normalize_chat_endpoint, normalize_proxy_url,
        normalize_web_endpoint, parse_engine_io_ping_interval, proxy_candidates,
        proxy_target_authority, select_endpoint, socks5_target_address,
    };
    use std::time::Duration;

    #[test]
    fn engine_io_ping_payload_is_echoed_in_pong() {
        assert_eq!(engine_io_pong("2"), Some("3".into()));
        assert_eq!(engine_io_pong("2probe"), Some("3probe".into()));
        assert_eq!(engine_io_pong("42event"), None);
    }

    #[test]
    fn engine_io_ping_interval_is_parsed_and_scaled() {
        assert_eq!(
            parse_engine_io_ping_interval(
                r#"0{"sid":"abc","pingInterval":5000,"pingTimeout":5000}"#
            ),
            Some(Duration::from_millis(4000))
        );
        assert_eq!(
            parse_engine_io_ping_interval(
                r#"0{"sid":"abc","pingInterval":25000,"pingTimeout":20000}"#
            ),
            Some(Duration::from_millis(20000))
        );
        assert_eq!(parse_engine_io_ping_interval(r#"0{"sid":"abc"}"#), None);
        assert_eq!(parse_engine_io_ping_interval("2"), None);
    }

    #[test]
    fn web_endpoint_normalization_adds_browser_path_for_host_values() {
        assert_eq!(
            normalize_web_endpoint("audio.example///"),
            "wss://audio.example/websocket/?EIO=3&transport=websocket"
        );
        assert_eq!(
            normalize_web_endpoint("https://audio.example"),
            "wss://audio.example/websocket/?EIO=3&transport=websocket"
        );
        assert_eq!(
            normalize_web_endpoint("wss://audio.example/websocket"),
            "wss://audio.example/websocket/?EIO=3&transport=websocket"
        );
        assert_eq!(
            normalize_web_endpoint("http://audio.example/custom?mode=web"),
            "ws://audio.example/custom?mode=web&EIO=3&transport=websocket"
        );
    }

    #[test]
    fn chat_endpoint_normalization_adds_socket_io_path_and_query() {
        assert_eq!(
            normalize_chat_endpoint("im.example///"),
            "wss://im.example/android/?EIO=3&transport=websocket"
        );
        assert_eq!(
            normalize_chat_endpoint("https://im.example"),
            "wss://im.example/android/?EIO=3&transport=websocket"
        );
        assert_eq!(
            normalize_chat_endpoint("wss://im.example/custom?EIO=3"),
            "wss://im.example/custom?EIO=3&transport=websocket"
        );
    }

    #[test]
    fn http_connect_authority_brackets_ipv6_targets() {
        assert_eq!(
            proxy_target_authority("2001:db8::7", 443),
            "[2001:db8::7]:443"
        );
        assert_eq!(
            proxy_target_authority("example.test", 443),
            "example.test:443"
        );
    }

    #[tokio::test]
    async fn socks5h_keeps_hostnames_for_proxy_side_dns() {
        assert_eq!(
            socks5_target_address("socks5h", "voice.example", 443)
                .await
                .unwrap(),
            (3, b"voice.example".to_vec())
        );
        assert_eq!(
            socks5_target_address("socks5h", "2001:db8::7", 443)
                .await
                .unwrap(),
            (3, b"2001:db8::7".to_vec())
        );
    }

    #[tokio::test]
    async fn socks5_uses_literal_ip_without_dns() {
        assert_eq!(
            socks5_target_address("socks5", "2001:db8::7", 443)
                .await
                .unwrap(),
            (
                4,
                [0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7].to_vec()
            )
        );
    }

    #[tokio::test]
    async fn chat_send_failure_signals_shutdown() {
        let (outbound, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let (shutdown, mut observed) = tokio::sync::watch::channel(false);
        let transport = ChatTransport { outbound, shutdown };

        assert!(transport.send("payload".into()).await.is_err());
        assert!(*observed.borrow_and_update());
    }

    #[tokio::test]
    async fn voice_send_failure_signals_shutdown_for_both_backends() {
        let (outbound, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let (shutdown, mut observed) = tokio::sync::watch::channel(false);
        let transport = VoiceTransport {
            outbound,
            shutdown,
            proxy: None,
        };
        assert!(
            transport
                .send(serde_json::json!({"type": "event"}))
                .await
                .is_err()
        );
        assert!(*observed.borrow_and_update());

        let (outbound, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let (shutdown, mut observed) = tokio::sync::watch::channel(false);
        let transport = WebVoiceTransport {
            outbound,
            shutdown,
            proxy: None,
        };
        assert!(
            transport
                .send(serde_json::json!({"type": "event"}))
                .await
                .is_err()
        );
        assert!(*observed.borrow_and_update());
    }

    #[test]
    fn endpoint_override_preserves_default_and_accepts_custom_value() {
        assert_eq!(select_endpoint("wss://default", None), "wss://default");
        assert_eq!(
            select_endpoint("wss://default", Some(" wss://staging ")),
            "wss://staging".to_owned()
        );
        assert_eq!(
            select_endpoint("wss://default", Some("  ")),
            "wss://default"
        );
    }

    #[test]
    fn proxy_lists_accept_common_separators_and_ignore_blanks() {
        assert_eq!(
            proxy_candidates(Some("http://one:1, socks5://two:2;\nsocks5h://three:3")),
            vec![
                Some("http://one:1".into()),
                Some("socks5://two:2".into()),
                Some("socks5h://three:3".into()),
            ]
        );
    }

    #[test]
    fn proxy_entries_preserve_order_for_ui_rotation() {
        assert_eq!(
            super::proxy_entries(" first:1\n;second:2, first:1 "),
            vec!["first:1", "second:2", "first:1"]
        );
    }

    #[test]
    fn bare_proxy_entries_are_normalized_for_network_clients() {
        assert_eq!(
            normalize_proxy_url(" proxy.example:8080 "),
            "http://proxy.example:8080"
        );
        assert_eq!(
            normalize_proxy_url("socks5h://proxy.example:1080"),
            "socks5h://proxy.example:1080"
        );
    }

    #[test]
    fn proxy_error_redaction_removes_credentials_from_raw_and_normalized_text() {
        let raw = "failed for http://user:secret@proxy.example:8080";
        let redacted =
            super::redact_proxy_error(raw, Some("http://user:secret@proxy.example:8080"));
        assert_eq!(
            redacted,
            "failed for http://redacted:redacted@proxy.example:8080/"
        );
        assert!(!redacted.contains("secret"));

        let encoded = "failed for http://user%40example:p%3Ass@proxy.example:8080";
        let redacted = super::redact_proxy_error(
            encoded,
            Some("http://user%40example:p%3Ass@proxy.example:8080"),
        );
        assert_eq!(
            redacted,
            "failed for http://redacted:redacted@proxy.example:8080/"
        );
        assert!(!redacted.contains("p%3Ass"));
    }

    #[test]
    fn proxy_credentials_are_percent_decoded_before_authentication() {
        assert_eq!(
            decode_proxy_credential("user%40example").unwrap(),
            b"user@example"
        );
        assert_eq!(
            decode_proxy_credential("p%3Ass%2Fword").unwrap(),
            b"p:ss/word"
        );
    }

    #[test]
    fn empty_proxy_input_keeps_direct_connection_candidate() {
        assert_eq!(proxy_candidates(Some("  \n ; ")), vec![None]);
        assert_eq!(proxy_candidates(None), vec![None]);
    }

    #[tokio::test]
    async fn failed_proxy_attempts_are_aggregated_without_exposing_proxy_url() {
        let (notices, mut received) = tokio::sync::mpsc::channel(4);
        let result = VoiceTransportClient::connect(
            false,
            notices,
            Some("http://user:secret@[invalid"),
            None,
            None,
            None,
        )
        .await;
        let error = match result {
            Ok(_) => panic!("malformed proxy must not establish a voice connection"),
            Err(error) => error,
        };

        let message = error.to_string();
        assert!(message.contains("candidate 1"));
        assert!(!message.contains("secret"));
        let notice = received.try_recv().expect("proxy failure notice");
        assert!(notice.starts_with("voice:proxy-attempt-failed:1/1:"));
        assert!(!notice.contains("secret"));
    }

    #[test]
    fn randomized_android_profile_matches_wire_user_agent_contract() {
        let profile = VoiceTransportClient::random_android_user_agent();
        assert!(profile.starts_with("NektoMeAudio96/2.1.0 (Linux; U; Android "));
        assert!(profile.contains(" Build/"));
        assert!(profile.ends_with(')'));
    }
}
