use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::{StreamExt, stream};
use serde_json::{Map, Value, json};

pub const SERVER: &str = "wss://audio.nekto.me/androiduk/?EIO=3&transport=websocket";
const MAX_CONCURRENT_PING_PROBES: usize = 4;
const MAX_PING_SERVERS: usize = 16;

pub async fn resolve_android_server(
    proxy: Option<&str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    resolve_android_server_with_override(proxy, None).await
}

pub async fn resolve_android_server_with_override(
    proxy: Option<&str>,
    server_override: Option<&str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(server) = server_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(normalize_server_endpoint(server, "/androiduk/"));
    }
    if let Some(server) = std::env::var("NEKTOME_VOICE_SERVER")
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(normalize_server_endpoint(server.trim(), "/androiduk/"));
    }
    let Some(bootstrap_url) = std::env::var("NEKTOME_VOICE_BOOTSTRAP_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(SERVER.into());
    };
    let proxy_value = proxy
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| std::env::var("NEKTOME_PROXY").ok());
    let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(15));
    if let Some(proxy_value) = proxy_value {
        builder = builder.proxy(reqwest::Proxy::all(crate::transport::normalize_proxy_url(
            &proxy_value,
        ))?);
    }
    let client = builder.build()?;
    let document: Value = client
        .get(bootstrap_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    bootstrap_endpoint(&document)
        .ok_or_else(|| "voice bootstrap did not provide an enabled socket endpoint".into())
}

fn normalize_server_endpoint(value: &str, path: &str) -> String {
    let input = value.trim();
    let parse_input = if input.contains("://") {
        input.to_owned()
    } else {
        format!("wss://{}", input.trim_end_matches('/'))
    };
    let mut endpoint = url::Url::parse(&parse_input).ok();

    let Some(endpoint) = endpoint.as_mut() else {
        return value.to_owned();
    };

    match endpoint.scheme() {
        "http" => {
            let _ = endpoint.set_scheme("ws");
        }
        "https" => {
            let _ = endpoint.set_scheme("wss");
        }
        "ws" | "wss" => {}
        _ => return value.to_owned(),
    }

    if endpoint.path().trim_matches('/').is_empty() {
        endpoint.set_path(path);
    }
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
    endpoint.to_string()
}

fn bootstrap_endpoint(document: &Value) -> Option<String> {
    if document.get("socket").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let base = document.get("url").and_then(Value::as_str)?;
    let path = document
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("/audiochat");
    let mut endpoint = url::Url::parse(base).ok()?.join(path).ok()?;
    if endpoint.scheme() == "https" {
        let _ = endpoint.set_scheme("wss");
    } else if endpoint.scheme() == "http" {
        let _ = endpoint.set_scheme("ws");
    }
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
    Some(endpoint.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

const CLIENT_VERSION: i64 = 24;

fn challenge_common(event: &Value) -> (String, i64, String, String, Option<Value>) {
    let stamp = event
        .get("stamp")
        .and_then(Value::as_i64)
        .unwrap_or_else(chrono_free_unix);
    let seed = event
        .get("challengeId")
        .or_else(|| event.get("seed"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let bucket = ["pulse", "echo", "mirror"][(stamp.unsigned_abs() as usize) % 3].to_owned();
    let checksum = BASE64
        .encode(format!("{seed}:{bucket}:{stamp}"))
        .chars()
        .take(48)
        .collect();
    let echo = event.get("echo").cloned();
    (seed, stamp, bucket, checksum, echo)
}

fn chrono_free_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

pub fn challenge_proof(event: &Value) -> Value {
    let (seed, stamp, bucket, checksum, echo) = challenge_common(event);
    let nonce = uuid::Uuid::new_v4().to_string();
    let proof = BASE64
        .encode(format!("{seed}:{bucket}:{stamp}:{nonce}"))
        .chars()
        .take(96)
        .collect::<String>();
    json!({"type":"challenge-proof","challengeId":seed,"stamp":stamp,"bucket":bucket,
        "checksum":checksum,"clientVersion":CLIENT_VERSION,"echo":echo,"proofNonce":nonce,
        "proof":proof,"mode":event.get("mode").and_then(Value::as_str).unwrap_or("sync")})
}

pub fn challenge_ack(event: &Value) -> Value {
    let (seed, stamp, bucket, checksum, echo) = challenge_common(event);
    json!({"type":"challenge-ack","challengeId":seed,"stamp":stamp,"bucket":bucket,
        "checksum":checksum,"clientVersion":CLIENT_VERSION,"echo":echo,
        "mode":event.get("mode").and_then(Value::as_str).unwrap_or("passive")})
}

pub fn challenge_trace(event: &Value) -> Value {
    let (seed, stamp, bucket, checksum, echo) = challenge_common(event);
    json!({"type":"challenge-trace","challengeId":seed,"stamp":stamp,"bucket":bucket,
        "checksum":checksum,"clientVersion":CLIENT_VERSION,"echo":echo,
        "traceId":uuid::Uuid::new_v4().to_string(),
        "signal":event.get("signal").and_then(Value::as_str).unwrap_or("hold"),
        "weight":event.get("weight").and_then(Value::as_f64).filter(|value| value.is_finite()).unwrap_or(1.0)})
}

pub fn register(
    user_id: &str,
    connection_id: Option<&str>,
    peer_success: Option<&str>,
    locale: &str,
    time_zone: &str,
) -> Value {
    let mut event = Map::from_iter([
        ("type".into(), json!("register")),
        ("android".into(), json!(true)),
        ("userId".into(), json!(user_id)),
        ("locale".into(), json!(locale)),
        ("timeZone".into(), json!(time_zone)),
    ]);
    if let Some(value) = connection_id {
        event.insert("connectionId".into(), json!(value));
    }
    if let Some(value) = peer_success {
        event.insert("peerSuccess".into(), json!(value));
    }
    Value::Object(event)
}

pub fn register_web(user_id: &str, locale: &str, time_zone: &str) -> Value {
    json!({
        "type": "register",
        "android": false,
        "version": CLIENT_VERSION,
        "userId": user_id,
        "isTouch": false,
        "messengerNeedAuth": true,
        "timeZone": time_zone,
        "locale": locale
    })
}

pub fn web_set_fpt(user_agent: &str, token: &str, locale: &str) -> Value {
    let visitor_id = sha256_hex(&format!("{user_agent}|{token}"))[..32].to_owned();
    let canvas_id = sha256_hex(&format!("canvas|{visitor_id}"))[..32].to_owned();
    let stamp = chrono_free_unix();
    let info_data = json!({
        "cvha": canvas_id,
        "lsa": true,
        "lst": true,
        "bgi": 1,
        "bgh": 1,
        "fcb": 1,
        "lcb": 1,
        "tbc": -1,
        "vtkn": if token.is_empty() { 0 } else { 1 },
        "tsp": stamp,
        "useragent": user_agent,
        "isf": false,
        "ref": Value::Null,
        "los": "https://nekto.me",
        "lsh": "nekto.me",
        "aips": [],
        "symb": [],
        "isha": "",
        "ifrb": [],
        "ifha": "",
        "sftest": true,
        "aos": [],
        "stamp": stamp,
        "deviceInfo": {
            "platform": "MacIntel",
            "language": locale,
            "languages": [locale],
            "cookieEnabled": true,
            "doNotTrack": Value::Null,
            "hardwareConcurrency": 8,
            "deviceMemory": 8,
            "maxTouchPoints": 0
        }
    });
    json!({"type":"set-fpt", "fpt":visitor_id, "infoData":info_data.to_string()})
}

/// The raw web transport uses the registration connection id as the first
/// fingerprint token. Keep the persisted browser token as a fallback for
/// servers that omit the id in their registration response.
pub fn web_registration_fpt_token(event: &Value, fallback: &str) -> String {
    event
        .get("connectionId")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub fn web_agent(user_id: &str, internal_id: i64) -> Value {
    let digest = sha256_hex(&format!("{user_id}BYdKPTYYGZ7ALwA8oNm2{internal_id}"));
    json!({"type":"web-agent", "data":BASE64.encode(digest)})
}

pub fn web_fpt(user_id: &str, internal_id: i64, user_agent: &str, time_zone: &str) -> Value {
    use aes::Aes256;
    use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
    use md5::{Digest as Md5Digest, Md5};

    let fpt_value = Md5::digest(format!("{user_id}dildopenis").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let key = sha2::Sha256::digest(format!("{fpt_value}{internal_id}").as_bytes());
    let fpt = json!({
        "fonts":{"value":["Calibri","Century","Century Gothic","Franklin Gothic","MS Reference Specialty","Segoe UI Light"]},
        "languages":{"value":[["ru-RU"]]},
        "colorDepth":{"value":32}, "deviceMemory":{"value":16},
        "screenResolution":{"value":[1920,1080]}, "hardwareConcurrency":{"value":8},
        "timezone":{"value":time_zone}, "sessionStorage":{"value":true},
        "localStorage":{"value":true}, "indexedDB":{"value":true},
        "platform":{"value":"MacIntel"},
        "touchSupport":{"value":{"maxTouchPoints":0,"touchEvent":false,"touchStart":false}},
        "vendor":{"value":"Google Inc."}, "cookiesEnabled":{"value":true},
        "lsa":1, "lst":1, "bgi":1, "bgh":1, "fcb":1, "lcb":1, "tbc":1, "vtkn":1,
        "tsp":chrono_free_unix(), "useragent":user_agent, "stamp":uuid::Uuid::new_v4().to_string()
    });
    let iv: [u8; 16] = rand::random();
    type Encryptor = cbc::Encryptor<Aes256>;
    let mut buffer = fpt.to_string().into_bytes();
    let plaintext_len = buffer.len();
    buffer.resize(plaintext_len + 16, 0);
    let ciphertext = Encryptor::new_from_slices(&key, &iv)
        .expect("AES-256 key and IV have fixed sizes")
        .encrypt_padded::<Pkcs7>(&mut buffer, plaintext_len)
        .expect("fingerprint padding cannot fail");
    json!({"type":"set-fpt", "fpt":fpt_value,
        "infoDataS":format!("{}:{}", BASE64.encode(iv), BASE64.encode(ciphertext))})
}

fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn scan_for_peer(
    user_sex: &str,
    peer_sex: &str,
    user_age: Option<(i32, i32)>,
    peer_ages: &[(i32, i32)],
    token: Option<&str>,
) -> Value {
    scan_for_peer_with_token_type(user_sex, peer_sex, user_age, peer_ages, token, None)
}

pub fn scan_for_peer_with_token_type(
    user_sex: &str,
    peer_sex: &str,
    user_age: Option<(i32, i32)>,
    peer_ages: &[(i32, i32)],
    token: Option<&str>,
    token_type: Option<&str>,
) -> Value {
    let mut criteria = Map::from_iter([
        ("group".into(), json!(0)),
        ("userSex".into(), json!(user_sex)),
        ("peerSex".into(), json!(peer_sex)),
    ]);
    if let Some((from, to)) = user_age {
        criteria.insert("userAge".into(), json!({"from":from,"to":to}));
    }
    if !peer_ages.is_empty() {
        criteria.insert(
            "peerAges".into(),
            json!(
                peer_ages
                    .iter()
                    .map(|(from, to)| json!({"from":from,"to":to}))
                    .collect::<Vec<_>>()
            ),
        );
    }
    let mut event = Map::from_iter([
        ("type".into(), json!("scan-for-peer")),
        ("peerToPeer".into(), json!(true)),
        ("searchCriteria".into(), Value::Object(criteria)),
    ]);
    if let Some(value) = token {
        event.insert("token".into(), json!(value));
        event.insert(
            "tokenType".into(),
            json!(
                token_type
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("IMAGE")
            ),
        );
    }
    Value::Object(event)
}

pub fn type_only(event_type: &str) -> Value {
    json!({"type": event_type})
}

pub fn users_count_request() -> Value {
    type_only("users-count-request")
}

pub fn ping_server_response(echo: Option<Value>) -> Value {
    json!({"type":"ping-server-response", "echo":echo})
}

pub fn log_ping_results(results: Vec<Value>) -> Value {
    json!({"type":"log-ping-results", "result":results})
}

pub async fn probe_servers(servers: &[String], attempts: i32) -> Vec<Value> {
    let attempts = attempts.clamp(1, 5);
    stream::iter(servers.iter().take(MAX_PING_SERVERS).cloned())
        .map(|server| probe_server(server, attempts))
        .buffered(MAX_CONCURRENT_PING_PROBES)
        .collect()
        .await
}

async fn probe_server(server: String, attempts: i32) -> Value {
    let mut command = if cfg!(target_os = "windows") {
        let mut command = tokio::process::Command::new("ping.exe");
        command.args(["-n", &attempts.to_string(), &server]);
        command
    } else {
        let mut command = tokio::process::Command::new("ping");
        command.args(["-c", &attempts.to_string(), "-W", "1", &server]);
        command
    };
    let output = tokio::time::timeout(
        std::time::Duration::from_millis((attempts as u64 * 500 + 1_000).min(4_000)),
        command.output(),
    )
    .await;
    let mut best_ms: Option<i64> = None;
    let failures = if let Ok(Ok(output)) = output {
        let text = String::from_utf8_lossy(&output.stdout);
        let (parsed_best, successes) = parse_ping_output(&text);
        best_ms = parsed_best;
        attempts - successes as i32
    } else {
        attempts
    };
    json!({"server":server, "ping":best_ms.unwrap_or(-1), "fails":failures.max(0)})
}

fn parse_ping_output(text: &str) -> (Option<i64>, usize) {
    let mut best_ms: Option<i64> = None;
    let mut successes = 0;
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(start) = lower.find("time=").or_else(|| lower.find("time<")) {
            successes += 1;
            let value = &line[start + 5..];
            let number = value
                .split(|character: char| !character.is_ascii_digit() && character != '.')
                .next()
                .and_then(|value| value.parse::<f64>().ok());
            if let Some(number) = number {
                let rounded = number.round() as i64;
                best_ms = Some(best_ms.map_or(rounded, |current| current.min(rounded)));
            }
        }
    }
    (best_ms, successes)
}
pub fn connection_event(event_type: &str, connection_id: &str) -> Value {
    json!({"type":event_type,"connectionId":connection_id})
}

pub fn peer_mute(connection_id: &str, muted: bool) -> Value {
    json!({"type":"peer-mute","connectionId":connection_id,"muted":muted})
}
pub fn peer_connection(connected: bool, connection_id: Option<&str>) -> Value {
    let mut event = Map::from_iter([
        ("type".into(), json!("peer-connection")),
        ("connection".into(), json!(connected)),
    ]);
    if let Some(value) = connection_id {
        event.insert("connectionId".into(), json!(value));
    }
    Value::Object(event)
}

/// Messages emitted when WebRTC media becomes usable. The ordering mirrors the
/// Android voice client and is observable by the remote signaling peer.
pub fn media_ready_events(connection_id: &str, muted: bool) -> [Value; 3] {
    [
        connection_event("stream-received", connection_id),
        peer_connection(true, Some(connection_id)),
        peer_mute(connection_id, muted),
    ]
}

pub fn sdp_event(event_type: &str, connection_id: &str, sdp: &str) -> Value {
    json!({"type":event_type,"connectionId":connection_id,event_type:serde_json::to_string(&json!({"type":event_type,"sdp":sdp})).expect("SDP serialization cannot fail")})
}

pub fn ice_candidate(
    connection_id: &str,
    sdp_mid: &str,
    sdp_m_line_index: i32,
    candidate: &str,
) -> Value {
    json!({"type":"ice-candidate","connectionId":connection_id,"candidate":serde_json::to_string(&json!({"candidate":{"sdpMid":sdp_mid,"sdpMLineIndex":sdp_m_line_index,"candidate":candidate}})).expect("ICE serialization cannot fail")})
}

pub fn parse_sdp(raw: &str) -> Result<(String, String), serde_json::Error> {
    if !raw.trim_start().starts_with('{') {
        let sdp = raw.trim();
        if sdp.is_empty() {
            return Err(invalid_sdp("SDP is empty"));
        }
        return Ok(("offer".into(), sdp.into()));
    }
    let value: Value = serde_json::from_str(raw)?;
    parse_sdp_object(&value)
}

fn parse_sdp_object(value: &Value) -> Result<(String, String), serde_json::Error> {
    let sdp = value
        .get("sdp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if sdp.is_empty() {
        return Err(invalid_sdp("SDP is empty"));
    }
    Ok((
        value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("offer")
            .into(),
        sdp.into(),
    ))
}

pub fn parse_sdp_value(raw: &Value) -> Result<(String, String), serde_json::Error> {
    match raw {
        Value::String(value) => parse_sdp(value),
        Value::Object(_) => parse_sdp_object(raw),
        _ => Err(invalid_sdp("SDP payload must be a string or object")),
    }
}

fn invalid_sdp(message: &str) -> serde_json::Error {
    serde_json::Error::io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

pub fn sdp_payload<'a>(event: &'a Value, event_type: &str) -> Option<&'a Value> {
    let primary = if event_type == "offer" {
        "offer"
    } else {
        "answer"
    };
    let alternate = if event_type == "offer" {
        "answer"
    } else {
        "offer"
    };
    event
        .get(primary)
        .or_else(|| event.get("sdp"))
        .or_else(|| event.get(alternate))
}

pub fn parse_candidate(raw: &str) -> Result<(String, i32, String), serde_json::Error> {
    if !raw.trim_start().starts_with('{') {
        let candidate = raw.trim();
        if candidate.is_empty() || candidate.eq_ignore_ascii_case("null") {
            return Err(serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ICE candidate is empty",
            )));
        }
        return Ok(("0".into(), 0, candidate.into()));
    }
    let value: Value = serde_json::from_str(raw)?;
    let candidate = value
        .get("candidate")
        .filter(|value| value.is_object())
        .unwrap_or(&value);
    let candidate_text = candidate
        .get("candidate")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            candidate
                .get("candidate")
                .filter(|value| !value.is_null())
                .map(Value::to_string)
        })
        .unwrap_or_default();
    if candidate_text.trim().is_empty() || candidate_text == "null" {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ICE candidate is empty",
        )));
    }
    let sdp_m_line_index = candidate
        .get("sdpMLineIndex")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if !(0..=i64::from(u16::MAX)).contains(&sdp_m_line_index) {
        return Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ICE candidate m-line index is out of range",
        )));
    }
    Ok((
        candidate
            .get("sdpMid")
            .and_then(Value::as_str)
            .unwrap_or("0")
            .into(),
        sdp_m_line_index as i32,
        candidate_text,
    ))
}

pub fn parse_candidate_value(raw: &Value) -> Result<(String, i32, String), serde_json::Error> {
    match raw {
        Value::String(value) => parse_candidate(value),
        Value::Object(_) => {
            parse_candidate(&serde_json::to_string(raw).expect("JSON value is serializable"))
        }
        _ => Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "ICE candidate payload must be a string or object",
        ))),
    }
}

pub fn ice_urls_from_peer_event(event: &Value) -> Vec<String> {
    ice_servers_from_peer_event(event)
        .into_iter()
        .flat_map(|server| server.urls)
        .collect()
}

pub fn ice_servers_from_peer_event(event: &Value) -> Vec<IceServer> {
    let mut servers = Vec::new();
    // Match the Android client: TURN is configured first, followed by the
    // server-provided STUN endpoint. This preserves relay preference while
    // retaining STUN as a fallback candidate source.
    if let Some(turn) = event.get("turnParams") {
        let parsed = if let Some(raw) = turn.as_str() {
            serde_json::from_str::<Value>(raw).unwrap_or(Value::Null)
        } else {
            turn.clone()
        };
        let values = parsed.as_array().cloned().unwrap_or_else(|| {
            if parsed.is_object() {
                vec![parsed]
            } else {
                Vec::new()
            }
        });
        for item in values {
            let urls = item
                .get("urls")
                .map(|value| match value {
                    Value::Array(items) => items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                    Value::String(url) if !url.trim().is_empty() => vec![url.to_owned()],
                    _ => Vec::new(),
                })
                .filter(|urls: &Vec<String>| !urls.is_empty())
                .or_else(|| {
                    item.get("url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.trim().is_empty())
                        .map(|url| vec![url.to_owned()])
                })
                .unwrap_or_default();
            if !urls.is_empty() {
                servers.push(IceServer {
                    urls,
                    username: item
                        .get("username")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    credential: item
                        .get("credential")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
        }
    }
    if let Some(url) = event
        .get("stunUrl")
        .and_then(Value::as_str)
        .filter(|url| !url.trim().is_empty())
    {
        servers.push(IceServer {
            urls: vec![url.to_owned()],
            username: None,
            credential: None,
        });
    }
    if servers.is_empty() {
        servers.push(IceServer {
            urls: vec!["stun:stun.l.google.com:19302".into()],
            username: None,
            credential: None,
        });
    }
    servers
}

pub fn pcmu_encode(pcm: &[i16]) -> Vec<u8> {
    g711_encode(0, pcm)
}

/// Encode mono 8 kHz PCM using the RTP G.711 payload type: PT 0 is PCMU and
/// PT 8 is PCMA.
pub fn g711_encode(payload_type: u8, pcm: &[i16]) -> Vec<u8> {
    let mut output = Vec::with_capacity(pcm.len());
    g711_encode_into(payload_type, pcm, &mut output);
    output
}

pub fn g711_encode_into(payload_type: u8, pcm: &[i16], output: &mut Vec<u8>) {
    output.clear();
    output.reserve(pcm.len());
    output.extend(pcm.iter().map(|&sample| {
        if payload_type == 8 {
            linear_to_pcma(sample)
        } else {
            linear_to_pcmu(sample)
        }
    }));
}

pub fn pcma_encode(pcm: &[i16]) -> Vec<u8> {
    g711_encode(8, pcm)
}

pub fn decode(payload_type: u8, payload: &[u8]) -> Vec<i16> {
    let mut output = Vec::with_capacity(payload.len());
    decode_into(payload_type, payload, &mut output);
    output
}

pub fn decode_into(payload_type: u8, payload: &[u8], output: &mut Vec<i16>) {
    output.clear();
    output.reserve(payload.len());
    output.extend(payload.iter().map(|&value| {
        if payload_type == 8 {
            alaw_to_linear(value)
        } else {
            pcmu_to_linear(value)
        }
    }));
}

fn linear_to_pcmu(sample: i16) -> u8 {
    let bias = 0x84;
    let clip = 32635;
    let mut value = i32::from(sample);
    let sign = value < 0;
    if sign {
        value = -value;
    }
    value = value.min(clip) + bias;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while value & mask == 0 && exponent > 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (value >> (exponent + 3)) & 0x0f;
    (!(if sign { 0x80 } else { 0 } | exponent << 4 | mantissa)) as u8
}

fn pcmu_to_linear(value: u8) -> i16 {
    let value = i32::from(!value);
    let sign = value & 0x80 != 0;
    let exponent = (value >> 4) & 7;
    let mantissa = value & 15;
    let sample = (((mantissa << 3) + 0x84) << exponent) - 0x84;
    if sign { -sample as i16 } else { sample as i16 }
}

fn linear_to_pcma(sample: i16) -> u8 {
    let magnitude = i32::from(sample).abs().min(0x7fff);
    let sign = if sample < 0 { 0x80 } else { 0 };
    let exponent = if magnitude < 0x100 {
        0
    } else if magnitude < 0x200 {
        1
    } else if magnitude < 0x400 {
        2
    } else if magnitude < 0x800 {
        3
    } else if magnitude < 0x1000 {
        4
    } else if magnitude < 0x2000 {
        5
    } else if magnitude < 0x4000 {
        6
    } else {
        7
    };
    let mantissa = if exponent == 0 {
        (magnitude >> 4) & 0x0f
    } else {
        (magnitude >> (exponent + 3)) & 0x0f
    };
    (sign | (exponent << 4) | mantissa) as u8 ^ 0x55
}

fn alaw_to_linear(value: u8) -> i16 {
    let value = i32::from(value ^ 0x55);
    let sign = value & 0x80 != 0;
    let exponent = (value >> 4) & 7;
    let mantissa = value & 15;
    let sample = if exponent == 0 {
        (mantissa << 4) + 8
    } else {
        ((mantissa << 4) + 0x108) << (exponent - 1)
    };
    if sign { -sample as i16 } else { sample as i16 }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn register_omits_resume_fields_when_absent() {
        let json = register("user-7", None, None, "en", "+00:00");
        assert!(!json.to_string().contains("connectionId"));
    }

    #[test]
    fn android_registration_preserves_resume_contract() {
        let json = register(
            "user-7",
            Some("connection-3"),
            Some("success"),
            "ru",
            "Europe/Moscow",
        );
        assert_eq!(
            json,
            json!({
                "type": "register",
                "android": true,
                "userId": "user-7",
                "locale": "ru",
                "timeZone": "Europe/Moscow",
                "connectionId": "connection-3",
                "peerSuccess": "success"
            })
        );
    }

    #[test]
    fn web_registration_uses_flat_browser_shape() {
        let json = register_web("user-7", "en", "UTC");
        assert_eq!(json["android"], false);
        assert_eq!(json["version"], CLIENT_VERSION);
        assert_eq!(json["messengerNeedAuth"], true);
    }

    #[test]
    fn web_fingerprint_contains_serialized_info_data() {
        let json = web_set_fpt("agent", "user-7", "en");
        assert_eq!(json["type"], "set-fpt");
        assert_eq!(json["fpt"].as_str().unwrap().len(), 32);
        let info: Value = serde_json::from_str(json["infoData"].as_str().unwrap()).unwrap();
        assert_eq!(info["deviceInfo"]["language"], "en");
        assert_eq!(info["deviceInfo"]["platform"], "MacIntel");
        assert_eq!(info["useragent"], "agent");
    }

    #[test]
    fn web_agent_and_encrypted_fingerprint_have_reference_shapes() {
        let agent = web_agent("token-7", 42);
        assert_eq!(agent["type"], "web-agent");
        assert!(!agent["data"].as_str().unwrap().is_empty());
        let fingerprint = web_fpt("token-7", 42, "agent", "UTC");
        assert_eq!(fingerprint["type"], "set-fpt");
        assert_eq!(fingerprint["fpt"].as_str().unwrap().len(), 32);
        assert_eq!(
            fingerprint["infoDataS"]
                .as_str()
                .unwrap()
                .matches(':')
                .count(),
            1
        );
    }
    #[test]
    fn scan_adds_image_token_only_when_present() {
        let json = scan_for_peer("ANY", "ANY", None, &[], Some("tok"));
        assert_eq!(json["tokenType"], "IMAGE");
    }

    #[test]
    fn scan_preserves_server_selected_captcha_type() {
        let json =
            scan_for_peer_with_token_type("ANY", "ANY", None, &[], Some("tok"), Some("RECAPTCHA"));
        assert_eq!(json["tokenType"], "RECAPTCHA");
    }

    #[test]
    fn scan_encodes_filters_and_omits_optional_token_fields() {
        let json = scan_for_peer(
            "FEMALE",
            "MALE",
            Some((25, 35)),
            &[(30, 40), (45, 55)],
            None,
        );
        assert_eq!(json["type"], "scan-for-peer");
        assert_eq!(json["peerToPeer"], true);
        assert_eq!(json["searchCriteria"]["group"], 0);
        assert_eq!(json["searchCriteria"]["userSex"], "FEMALE");
        assert_eq!(json["searchCriteria"]["peerSex"], "MALE");
        assert_eq!(
            json["searchCriteria"]["userAge"],
            json!({"from": 25, "to": 35})
        );
        assert_eq!(
            json["searchCriteria"]["peerAges"],
            json!([
                {"from": 30, "to": 40},
                {"from": 45, "to": 55}
            ])
        );
        assert!(json.get("token").is_none());
        assert!(json.get("tokenType").is_none());
    }

    #[test]
    fn peer_connection_omits_optional_connection_id_when_absent() {
        assert_eq!(
            peer_connection(false, None),
            json!({"type": "peer-connection", "connection": false})
        );
        assert_eq!(
            peer_connection(false, Some("peer-1")),
            json!({
                "type": "peer-connection",
                "connection": false,
                "connectionId": "peer-1"
            })
        );
    }

    #[test]
    fn web_registration_fpt_prefers_connection_id() {
        assert_eq!(
            web_registration_fpt_token(&json!({"connectionId":"server-id"}), "browser-id"),
            "server-id"
        );
        assert_eq!(
            web_registration_fpt_token(&json!({"connectionId":" "}), "browser-id"),
            "browser-id"
        );
    }
    #[test]
    fn sdp_is_embedded_as_json_string() {
        let json = sdp_event("offer", "p1", "v=0");
        assert_eq!(json["offer"]["sdp"], Value::Null);
        assert_eq!(
            parse_sdp(json["offer"].as_str().unwrap()).unwrap(),
            ("offer".into(), "v=0".into())
        );
    }

    #[test]
    fn parsers_accept_raw_and_flat_media_descriptions() {
        assert_eq!(
            parse_sdp("v=0\r\no=- 1 1 IN IP4 0.0.0.0").unwrap(),
            ("offer".into(), "v=0\r\no=- 1 1 IN IP4 0.0.0.0".into())
        );
        assert_eq!(
            parse_candidate("candidate:1 1 UDP 1 127.0.0.1 9 typ host").unwrap(),
            (
                "0".into(),
                0,
                "candidate:1 1 UDP 1 127.0.0.1 9 typ host".into()
            )
        );
        assert_eq!(
            parse_candidate(r#"{"sdpMid":"audio","sdpMLineIndex":1,"candidate":"candidate:x"}"#)
                .unwrap(),
            ("audio".into(), 1, "candidate:x".into())
        );
    }

    #[test]
    fn candidate_parser_rejects_raw_null_sentinel() {
        assert!(parse_candidate("null").is_err());
        assert!(parse_candidate("  NULL  ").is_err());
    }

    #[test]
    fn candidate_parser_rejects_end_of_candidates_sentinels() {
        assert!(parse_candidate(" ").is_err());
        assert!(parse_candidate(r#"{"candidate":null}"#).is_err());
        assert!(parse_candidate(r#"{"candidate":""}"#).is_err());
    }

    #[test]
    fn candidate_parser_rejects_out_of_range_m_line_indices() {
        assert!(
            parse_candidate(r#"{"sdpMid":"audio","sdpMLineIndex":-1,"candidate":"candidate:x"}"#)
                .is_err()
        );
        assert!(
            parse_candidate(
                r#"{"sdpMid":"audio","sdpMLineIndex":65536,"candidate":"candidate:x"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn value_parsers_accept_object_media_payloads() {
        assert_eq!(
            parse_sdp_value(&json!({"type":"answer","sdp":"v=0"})).unwrap(),
            ("answer".into(), "v=0".into())
        );
        assert_eq!(
            parse_candidate_value(&json!({
                "candidate": {"sdpMid":"audio","sdpMLineIndex":0,"candidate":"candidate:y"}
            }))
            .unwrap(),
            ("audio".into(), 0, "candidate:y".into())
        );
    }

    #[test]
    fn sdp_parser_rejects_missing_or_blank_descriptions() {
        assert!(parse_sdp(" ").is_err());
        assert!(parse_sdp_value(&json!({"type": "offer"})).is_err());
        assert!(parse_sdp_value(&json!({"type": "answer", "sdp": "  "})).is_err());
    }

    #[test]
    fn sdp_payload_supports_top_level_and_alternate_fields() {
        let top_level = json!({"type":"offer","sdp":{"type":"offer","sdp":"v=0"}});
        assert!(sdp_payload(&top_level, "offer").is_some());
        let alternate = json!({"type":"offer","answer":{"type":"offer","sdp":"v=0"}});
        assert_eq!(
            parse_sdp_value(sdp_payload(&alternate, "offer").unwrap()).unwrap(),
            ("offer".into(), "v=0".into())
        );
    }
    #[test]
    fn g711_round_trip_has_small_quantization_error() {
        for sample in [-30000, -1000, 0, 1000, 30000] {
            let decoded = decode(0, &pcmu_encode(&[sample]))[0];
            assert!((i32::from(sample) - i32::from(decoded)).abs() < 2000);
        }
    }

    #[test]
    fn pcma_round_trip_has_small_quantization_error() {
        for sample in [-30000, -1000, 0, 1000, 30000] {
            let decoded = decode(8, &pcma_encode(&[sample]))[0];
            assert!((i32::from(sample) - i32::from(decoded)).abs() < 2000);
        }
    }

    #[test]
    fn g711_encoder_selects_codec_by_rtp_payload_type() {
        let pcm = [-1000, 0, 1000];
        assert_eq!(g711_encode(0, &pcm), pcmu_encode(&pcm));
        assert_eq!(g711_encode(8, &pcm), pcma_encode(&pcm));
        assert_ne!(pcma_encode(&pcm), pcmu_encode(&pcm));
    }

    #[test]
    fn peer_event_preserves_turn_credentials() {
        let event = json!({
            "stunUrl": "stun:stun.example:3478",
            "turnParams": [{"url":"turn:turn.example:3478", "username":"u", "credential":"c"}]
        });
        let servers = ice_servers_from_peer_event(&event);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].username.as_deref(), Some("u"));
        assert_eq!(servers[0].credential.as_deref(), Some("c"));
        assert_eq!(servers[1].urls, vec!["stun:stun.example:3478"]);
    }

    #[test]
    fn peer_event_accepts_standard_turn_urls_string_and_array_forms() {
        let event = json!({
            "turnParams": [
                {"urls":"turn:one.example:3478", "username":"u1", "credential":"c1"},
                {"urls":["turn:two.example:3478", "turns:two.example:5349"], "username":"u2", "credential":"c2"}
            ]
        });
        let servers = ice_servers_from_peer_event(&event);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].urls, vec!["turn:one.example:3478"]);
        assert_eq!(
            servers[1].urls,
            vec!["turn:two.example:3478", "turns:two.example:5349"]
        );
        assert_eq!(servers[1].username.as_deref(), Some("u2"));
    }

    #[test]
    fn ping_messages_use_reference_wire_shapes() {
        assert_eq!(
            ping_server_response(Some(json!(7))),
            json!({"type":"ping-server-response","echo":7})
        );
        assert_eq!(
            log_ping_results(vec![json!({"server":"relay","ping":42,"fails":0})]),
            json!({"type":"log-ping-results","result":[{"server":"relay","ping":42,"fails":0}]})
        );
    }

    #[test]
    fn bootstrap_endpoint_matches_legacy_socket_document() {
        let document = json!({
            "socket": true,
            "url": "https://audio.example/",
            "path": "/androiduk"
        });
        assert_eq!(
            super::bootstrap_endpoint(&document).as_deref(),
            Some("wss://audio.example/androiduk?EIO=3&transport=websocket")
        );
    }

    #[test]
    fn server_host_override_builds_android_endpoint_without_touching_full_urls() {
        assert_eq!(
            super::normalize_server_endpoint("audio.example", "/androiduk/"),
            "wss://audio.example/androiduk/?EIO=3&transport=websocket"
        );
        assert_eq!(
            super::normalize_server_endpoint("audio.example///", "/androiduk/"),
            "wss://audio.example/androiduk/?EIO=3&transport=websocket"
        );
        assert_eq!(
            super::normalize_server_endpoint(
                "wss://audio.example/custom?EIO=3&transport=websocket",
                "/androiduk/"
            ),
            "wss://audio.example/custom?EIO=3&transport=websocket"
        );
        assert_eq!(
            super::normalize_server_endpoint("https://audio.example/custom", "/androiduk/"),
            "wss://audio.example/custom?EIO=3&transport=websocket"
        );
        assert_eq!(
            super::normalize_server_endpoint("https://audio.example", "/androiduk/"),
            "wss://audio.example/androiduk/?EIO=3&transport=websocket"
        );
    }

    #[test]
    fn disabled_bootstrap_socket_is_rejected() {
        assert_eq!(super::bootstrap_endpoint(&json!({"socket": false})), None);
    }

    #[test]
    fn bootstrap_endpoint_fills_missing_transport_parameter() {
        let endpoint = super::bootstrap_endpoint(&json!({
            "socket": true,
            "url": "https://audio.example",
            "path": "/audiochat?EIO=3"
        }))
        .unwrap();
        assert!(endpoint.contains("EIO=3"));
        assert!(endpoint.contains("transport=websocket"));
    }

    #[test]
    fn ping_parser_counts_less_than_replies_as_successes() {
        let (best, successes) = parse_ping_output(
            "64 bytes from relay: time=12.4 ms\n64 bytes from relay: time<1 ms\nrequest timeout",
        );
        assert_eq!(best, Some(1));
        assert_eq!(successes, 2);
    }

    #[test]
    fn users_count_request_is_type_only() {
        assert_eq!(users_count_request(), json!({"type":"users-count-request"}));
    }

    #[test]
    fn media_ready_events_follow_reference_order() {
        let events = media_ready_events("peer-1", false);
        assert_eq!(events[0]["type"], "stream-received");
        assert_eq!(events[1]["type"], "peer-connection");
        assert_eq!(events[2]["type"], "peer-mute");
        assert_eq!(events[2]["muted"], false);
        assert_eq!(media_ready_events("peer-1", true)[2]["muted"], true);
    }

    #[test]
    fn peer_connection_preserves_identifier_when_disconnecting() {
        assert_eq!(
            peer_connection(false, Some("peer-1")),
            json!({"type":"peer-connection","connection":false,"connectionId":"peer-1"})
        );
    }

    #[test]
    fn ice_server_parser_falls_back_to_public_stun() {
        assert_eq!(
            ice_servers_from_peer_event(&json!([])),
            vec![IceServer {
                urls: vec!["stun:stun.l.google.com:19302".into()],
                username: None,
                credential: None,
            }]
        );
    }

    #[test]
    fn blank_stun_url_does_not_suppress_public_fallback() {
        let servers = ice_servers_from_peer_event(&json!({"stunUrl": "  "}));
        assert_eq!(servers[0].urls, vec!["stun:stun.l.google.com:19302"]);
    }
}
