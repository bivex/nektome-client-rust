use serde::{Deserialize, Serialize};
use std::{fs, io, path::PathBuf};

use crate::session_recorder::{RecordingFormat, RecordingParameters};

/// Reads the Web voice token from Chrome's local-storage LevelDB files.
/// Chrome keeps these files open while running, so failures are intentionally
/// ignored and the newest readable database is searched first.
pub fn try_extract_web_token() -> Option<String> {
    let leveldb = chrome_leveldb_path()?;
    let mut files = fs::read_dir(leveldb)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| {
            matches!(
                entry.path().extension().and_then(|value| value.to_str()),
                Some("ldb") | Some("log")
            )
        })
        .collect::<Vec<_>>();
    files.sort_by_key(|entry| {
        std::cmp::Reverse(
            entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok(),
        )
    });

    for entry in files {
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let marker = b"storage_audio_v2";
        let Some(start) = bytes
            .windows(marker.len())
            .position(|window| window == marker)
        else {
            continue;
        };
        let end = (start + 2_000).min(bytes.len());
        if let Some(token) = extract_auth_token(&bytes[start..end]) {
            return Some(token);
        }
    }
    None
}

pub fn new_web_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn new_voice_user_id() -> String {
    format!("google-{}", uuid::Uuid::new_v4())
}

pub fn normalize_voice_user_id(value: &str) -> String {
    value
        .trim_matches(|character: char| {
            character.is_whitespace() || matches!(character, '"' | '\'')
        })
        .to_owned()
}

pub fn normalize_voice_sex_filter(value: &str) -> String {
    match value.trim().to_ascii_uppercase().as_str() {
        "F" | "FEMALE" | "GIRL" | "WOMAN" => "FEMALE".into(),
        "M" | "MALE" | "BOY" | "MAN" => "MALE".into(),
        _ => "ANY".into(),
    }
}

fn extract_auth_token(bytes: &[u8]) -> Option<String> {
    let marker = b"authToken";
    let start = bytes
        .windows(marker.len())
        .position(|window| window == marker)?
        + marker.len();
    let mut index = start;
    if bytes.get(index) == Some(&b'"') {
        index += 1;
    }
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b':') {
        return None;
    }
    index += 1;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'"') {
        return None;
    }
    index += 1;
    let end = bytes[index..].iter().position(|byte| *byte == b'"')? + index;
    let token = std::str::from_utf8(&bytes[index..end]).ok()?;
    let compact = token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit());
    let hyphenated = token.len() == 36
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    (compact || hyphenated).then(|| token.to_owned())
}

fn chrome_leveldb_path() -> Option<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let home = std::env::var_os("HOME").map(PathBuf::from);
    #[cfg(target_os = "linux")]
    {
        return home.map(|path| path.join(".config/google-chrome/Default/Local Storage/leveldb"));
    }
    #[cfg(target_os = "macos")]
    {
        return home.map(|path| {
            path.join("Library/Application Support/Google/Chrome/Default/Local Storage/leveldb")
        });
    }
    #[cfg(target_os = "windows")]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("Google/Chrome/User Data/Default/Local Storage/leveldb"));
    }
    #[allow(unreachable_code)]
    None
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeviceIdentity {
    #[serde(rename = "deviceId", alias = "device_id")]
    pub device_id: String,
    #[serde(rename = "deviceName", default, alias = "device_name")]
    pub device_name: String,
}

fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "desktop".into())
}

#[derive(Debug, Default, Deserialize)]
struct LegacySettings {
    #[serde(default, alias = "WebToken", alias = "webToken")]
    web_token: Option<String>,
    #[serde(default, alias = "AndroidId", alias = "androidId")]
    android_id: Option<String>,
    #[serde(default, alias = "SavedProxies", alias = "savedProxies")]
    saved_proxies: Option<String>,
    #[serde(default, alias = "ProxySellerApiKey", alias = "proxySellerApiKey")]
    proxy_seller_api_key: Option<String>,
    #[serde(default, alias = "ProxySellerListName", alias = "proxySellerListName")]
    proxy_seller_list_name: Option<String>,
    #[serde(
        default,
        alias = "ProxySellerPortsCount",
        alias = "proxySellerPortsCount"
    )]
    proxy_seller_ports_count: Option<i64>,
    #[serde(
        default,
        alias = "ProxySellerRotationSeconds",
        alias = "proxySellerRotationSeconds"
    )]
    proxy_seller_rotation_seconds: Option<i64>,
    #[serde(
        default,
        alias = "ProxySellerWhitelistIp",
        alias = "proxySellerWhitelistIp"
    )]
    proxy_seller_whitelist_ip: Option<String>,
    #[serde(default, alias = "SpeakerGainPercent", alias = "speakerGainPercent")]
    speaker_gain_percent: Option<f32>,
    #[serde(default, alias = "MicGainPercent", alias = "micGainPercent")]
    microphone_gain_percent: Option<f32>,
    #[serde(default, alias = "MicAgcEnabled", alias = "micAgcEnabled")]
    #[allow(dead_code)]
    microphone_agc_enabled: Option<bool>,
    #[serde(default, alias = "MicDspEnabled", alias = "micDspEnabled")]
    microphone_dsp_enabled: Option<bool>,
    #[serde(
        default,
        alias = "MicNoiseSuppressionEnabled",
        alias = "micNoiseSuppressionEnabled",
        alias = "noiseSuppressionEnabled"
    )]
    microphone_noise_suppression_enabled: Option<bool>,
    #[serde(default, alias = "SpeakerDspEnabled", alias = "speakerDspEnabled")]
    speaker_dsp_enabled: Option<bool>,
    #[serde(
        default,
        alias = "EchoCancellationEnabled",
        alias = "echoCancellationEnabled"
    )]
    echo_cancellation_enabled: Option<bool>,
    #[serde(default, alias = "NoiseGateThreshold", alias = "noiseGateThreshold")]
    noise_gate_threshold: Option<f32>,
    #[serde(default, alias = "CompressorThreshold", alias = "compressorThreshold")]
    compressor_threshold: Option<f32>,
    #[serde(default, alias = "CompressorRatio", alias = "compressorRatio")]
    compressor_ratio: Option<f32>,
    #[serde(default, alias = "MakeupGain", alias = "makeupGain")]
    makeup_gain: Option<f32>,
    #[serde(default, alias = "MicWarmthEnabled", alias = "micWarmthEnabled")]
    microphone_warmth_enabled: Option<bool>,
    #[serde(default, alias = "SaturationDrive", alias = "saturationDrive")]
    saturation_drive: Option<f32>,
    #[serde(default, alias = "SaturationAsymmetry", alias = "saturationAsymmetry")]
    saturation_asymmetry: Option<f32>,
    #[serde(default, alias = "SaturationBias", alias = "saturationBias")]
    saturation_bias: Option<f32>,
    #[serde(default, alias = "SaturationProfile", alias = "saturationProfile")]
    saturation_profile: Option<crate::audio::SaturationProfile>,
    #[serde(default, alias = "PresenceBoostDb", alias = "presenceBoostDb")]
    presence_boost_db: Option<f32>,
    #[serde(default, alias = "ProximityGainDb", alias = "proximityGainDb")]
    proximity_gain_db: Option<f32>,
    #[serde(default, alias = "AirBoostDb", alias = "airBoostDb")]
    air_boost_db: Option<f32>,
    #[serde(default, alias = "CharacterProfile", alias = "characterProfile")]
    character_profile: Option<crate::audio::VoiceCharacterProfile>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceIdentity {
    #[serde(rename = "userId", alias = "user_id")]
    pub user_id: String,
    #[serde(
        rename = "connectionId",
        default,
        alias = "connection_id",
        alias = "last_connection_id",
        alias = "connectionIdLast"
    )]
    pub last_connection_id: Option<String>,
    #[serde(rename = "resumePeerId", default, alias = "resume_peer_id")]
    pub resume_peer_id: Option<String>,
    #[serde(
        rename = "webToken",
        default,
        alias = "web_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub web_token: Option<String>,
}

impl VoiceIdentity {
    /// Migrates the compact web token written by older clients out of the
    /// voice user-id slot while retaining the token for the caller to save in
    /// the current preferences store.
    pub fn normalize(&mut self) {
        self.user_id = normalize_voice_user_id(&self.user_id);
        self.last_connection_id = self
            .last_connection_id
            .take()
            .map(|value| normalize_voice_user_id(&value))
            .filter(|value| !value.is_empty());
        self.resume_peer_id = self
            .resume_peer_id
            .take()
            .map(|value| normalize_voice_user_id(&value))
            .filter(|value| !value.is_empty());
        self.web_token = self
            .web_token
            .take()
            .map(|value| normalize_voice_user_id(&value))
            .filter(|value| !value.is_empty());
        let compact_token =
            self.user_id.len() == 32 && self.user_id.bytes().all(|value| value.is_ascii_hexdigit());
        if compact_token {
            self.web_token.get_or_insert_with(|| self.user_id.clone());
            self.user_id = new_voice_user_id();
            self.last_connection_id = None;
            self.resume_peer_id = None;
        }
        if self.user_id.trim().is_empty() {
            self.user_id = new_voice_user_id();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoicePreferences {
    pub proxy_seller_api_key: String,
    pub proxy_seller_list_name: String,
    pub proxy_seller_ports_count: i64,
    pub proxy_seller_rotation_seconds: i64,
    pub proxy_seller_whitelist_ip: String,
    pub web_token: String,
    pub web_mode: bool,
    pub voice_server_host: String,
    pub voice_server_override: bool,
    pub auto_next: bool,
    pub proxy_url: String,
    pub android_user_agent: String,
    pub android_locale: String,
    pub android_time_zone: String,
    pub remote_audio_wav_path: String,
    #[serde(alias = "record_sessions_mp3")]
    pub record_sessions: bool,
    pub recording_format: RecordingFormat,
    pub recording_parameters: RecordingParameters,
    pub microphone_device_name: String,
    pub speaker_device_name: String,
    pub push_to_talk: bool,
    pub speaker_gain_percent: f32,
    pub speaker_dsp_enabled: bool,
    pub microphone_gain_percent: f32,
    pub microphone_dsp_enabled: bool,
    pub microphone_noise_suppression_enabled: bool,
    pub echo_cancellation_enabled: bool,
    pub noise_gate_threshold: f32,
    pub compressor_threshold: f32,
    pub compressor_ratio: f32,
    pub makeup_gain: f32,
    pub microphone_warmth_enabled: bool,
    pub saturation_drive: f32,
    pub saturation_asymmetry: f32,
    pub saturation_bias: f32,
    pub saturation_profile: crate::audio::SaturationProfile,
    pub presence_boost_db: f32,
    pub proximity_gain_db: f32,
    pub air_boost_db: f32,
    pub voice_changer_enabled: bool,
    pub voice_changer_model: String,
    pub voice_changer_pitch: f32,
    pub voice_changer_wet: f32,
    pub character_profile: crate::audio::VoiceCharacterProfile,
    pub user_sex: String,
    pub peer_sex: String,
    pub user_age_from: String,
    pub user_age_to: String,
    pub peer_age_from: String,
    pub peer_age_to: String,
}

impl Default for VoicePreferences {
    fn default() -> Self {
        Self {
            proxy_seller_api_key: String::new(),
            proxy_seller_list_name: "nektome_app".into(),
            proxy_seller_ports_count: 20,
            proxy_seller_rotation_seconds: 3600,
            proxy_seller_whitelist_ip: String::new(),
            web_token: String::new(),
            web_mode: false,
            voice_server_host: "audio.nekto.me".into(),
            voice_server_override: false,
            auto_next: false,
            proxy_url: String::new(),
            android_user_agent: String::new(),
            android_locale: String::new(),
            android_time_zone: String::new(),
            remote_audio_wav_path: String::new(),
            record_sessions: false,
            recording_format: RecordingFormat::Flac,
            recording_parameters: RecordingParameters::default(),
            microphone_device_name: String::new(),
            speaker_device_name: String::new(),
            push_to_talk: false,
            speaker_gain_percent: 100.0,
            speaker_dsp_enabled: true,
            microphone_gain_percent: 100.0,
            microphone_dsp_enabled: true,
            microphone_warmth_enabled: true,
            saturation_drive: 1.08,
            saturation_asymmetry: 1.15,
            saturation_bias: 0.0,
            saturation_profile: crate::audio::SaturationProfile::NaturalVoice,
            presence_boost_db: 0.0,
            proximity_gain_db: 0.0,
            air_boost_db: 0.0,
            voice_changer_enabled: false,
            voice_changer_model: "models/voices/woman_1.onnx".into(),
            voice_changer_pitch: 0.0,
            voice_changer_wet: 1.0,
            character_profile: crate::audio::VoiceCharacterProfile::Natural,
            microphone_noise_suppression_enabled: true,
            echo_cancellation_enabled: true,
            noise_gate_threshold: 0.002,
            compressor_threshold: 0.12,
            compressor_ratio: 2.5,
            makeup_gain: 1.35,
            user_sex: "ANY".into(),
            peer_sex: "ANY".into(),
            user_age_from: String::new(),
            user_age_to: String::new(),
            peer_age_from: String::new(),
            peer_age_to: String::new(),
        }
    }
}

impl VoicePreferences {
    fn from_legacy(settings: LegacySettings) -> Self {
        let mut preferences = Self::default();
        if let Some(value) = settings.web_token {
            preferences.web_token = value;
        }
        if let Some(value) = settings.saved_proxies {
            preferences.proxy_url = value;
        }
        if let Some(value) = settings.proxy_seller_api_key {
            preferences.proxy_seller_api_key = value;
        }
        if let Some(value) = settings.proxy_seller_list_name {
            preferences.proxy_seller_list_name = value;
        }
        if let Some(value) = settings.proxy_seller_ports_count {
            preferences.proxy_seller_ports_count = value;
        }
        if let Some(value) = settings.proxy_seller_rotation_seconds {
            preferences.proxy_seller_rotation_seconds = value;
        }
        if let Some(value) = settings.proxy_seller_whitelist_ip {
            preferences.proxy_seller_whitelist_ip = value;
        }
        if let Some(value) = settings.speaker_gain_percent {
            preferences.speaker_gain_percent = value;
        }
        if let Some(value) = settings.microphone_gain_percent {
            preferences.microphone_gain_percent = value;
        }
        if let Some(value) = settings.microphone_dsp_enabled {
            preferences.microphone_dsp_enabled = value;
        }
        if let Some(value) = settings.microphone_noise_suppression_enabled {
            preferences.microphone_noise_suppression_enabled = value;
        }
        if let Some(value) = settings.speaker_dsp_enabled {
            preferences.speaker_dsp_enabled = value;
        }
        if let Some(value) = settings.echo_cancellation_enabled {
            preferences.echo_cancellation_enabled = value;
        }
        if let Some(value) = settings.noise_gate_threshold {
            preferences.noise_gate_threshold = value;
        }
        if let Some(value) = settings.compressor_threshold {
            preferences.compressor_threshold = value;
        }
        if let Some(value) = settings.compressor_ratio {
            preferences.compressor_ratio = value;
        }
        if let Some(value) = settings.makeup_gain {
            preferences.makeup_gain = value;
        }
        if let Some(value) = settings.microphone_warmth_enabled {
            preferences.microphone_warmth_enabled = value;
        }
        if let Some(value) = settings.saturation_drive {
            preferences.saturation_drive = value;
        }
        if let Some(value) = settings.saturation_asymmetry {
            preferences.saturation_asymmetry = value;
        }
        if let Some(value) = settings.saturation_bias {
            preferences.saturation_bias = value;
        }
        if let Some(value) = settings.saturation_profile {
            preferences.saturation_profile = value;
        } else {
            preferences.saturation_profile = crate::audio::SaturationProfile::from_params(
                preferences.saturation_drive,
                preferences.saturation_asymmetry,
                preferences.saturation_bias,
            );
        }
        if let Some(value) = settings.presence_boost_db {
            preferences.presence_boost_db = value;
        }
        if let Some(value) = settings.proximity_gain_db {
            preferences.proximity_gain_db = value;
        }
        if let Some(value) = settings.air_boost_db {
            preferences.air_boost_db = value;
        }
        if let Some(value) = settings.character_profile {
            preferences.character_profile = value;
        } else if preferences.saturation_profile != crate::audio::SaturationProfile::Custom {
            preferences.character_profile = preferences.saturation_profile.to_character_profile();
        } else {
            preferences.character_profile = crate::audio::VoiceCharacterProfile::from_params(
                preferences.presence_boost_db,
                preferences.saturation_drive,
                preferences.saturation_asymmetry,
                preferences.proximity_gain_db,
                preferences.air_boost_db,
                preferences.compressor_ratio,
            );
        }
        preferences
    }

    pub fn normalize(&mut self) {
        self.proxy_seller_api_key = self.proxy_seller_api_key.trim().to_owned();
        self.proxy_seller_list_name = self.proxy_seller_list_name.trim().to_owned();
        self.proxy_seller_whitelist_ip = self.proxy_seller_whitelist_ip.trim().to_owned();
        if self.proxy_seller_list_name.is_empty() {
            self.proxy_seller_list_name = "nektome_app".into();
        }
        if !(1..=1000).contains(&self.proxy_seller_ports_count) {
            self.proxy_seller_ports_count = 20;
        }
        if !(60..=31_536_000).contains(&self.proxy_seller_rotation_seconds) {
            self.proxy_seller_rotation_seconds = 3600;
        }
        self.web_token = self.web_token.trim().to_owned();
        self.voice_server_host = self.voice_server_host.trim().to_owned();
        if self.voice_server_host.is_empty() {
            self.voice_server_host = "audio.nekto.me".into();
            self.voice_server_override = false;
        }
        self.proxy_url = self.proxy_url.trim().to_owned();
        self.android_user_agent = self.android_user_agent.trim().to_owned();
        self.android_locale = self.android_locale.trim().to_owned();
        self.android_time_zone = self.android_time_zone.trim().to_owned();
        self.remote_audio_wav_path = self.remote_audio_wav_path.trim().to_owned();
        self.microphone_device_name = self.microphone_device_name.trim().to_owned();
        self.speaker_device_name = self.speaker_device_name.trim().to_owned();
        self.recording_parameters.normalize();
        self.user_sex = normalize_voice_sex_filter(&self.user_sex);
        self.peer_sex = normalize_voice_sex_filter(&self.peer_sex);
        self.user_age_from = self.user_age_from.trim().to_owned();
        self.user_age_to = self.user_age_to.trim().to_owned();
        self.peer_age_from = self.peer_age_from.trim().to_owned();
        self.peer_age_to = self.peer_age_to.trim().to_owned();
        if !self.speaker_gain_percent.is_finite()
            || !(0.0..=200.0).contains(&self.speaker_gain_percent)
        {
            self.speaker_gain_percent = 100.0;
        }
        if !self.microphone_gain_percent.is_finite()
            || !(25.0..=400.0).contains(&self.microphone_gain_percent)
        {
            self.microphone_gain_percent = 100.0;
        }
        if !self.noise_gate_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.noise_gate_threshold)
        {
            self.noise_gate_threshold = 0.002;
        }
        if !self.compressor_threshold.is_finite()
            || !(0.0..=1.0).contains(&self.compressor_threshold)
        {
            self.compressor_threshold = 0.12;
        }
        if !self.compressor_ratio.is_finite() || !(1.0..=20.0).contains(&self.compressor_ratio) {
            self.compressor_ratio = 2.5;
        }
        if !self.makeup_gain.is_finite() || !(0.0..=4.0).contains(&self.makeup_gain) {
            self.makeup_gain = 1.35;
        }
        if !self.presence_boost_db.is_finite() || !(0.0..=6.0).contains(&self.presence_boost_db) {
            self.presence_boost_db = 0.0;
        }
        if !self.proximity_gain_db.is_finite() || !(-3.0..=4.0).contains(&self.proximity_gain_db) {
            self.proximity_gain_db = 0.0;
        }
        if !self.air_boost_db.is_finite() || !(0.0..=4.0).contains(&self.air_boost_db) {
            self.air_boost_db = 0.0;
        }
        if !self.saturation_drive.is_finite() || !(1.0..=3.0).contains(&self.saturation_drive) {
            self.saturation_drive = 1.08;
        }
        if !self.saturation_asymmetry.is_finite()
            || !(1.0..=3.0).contains(&self.saturation_asymmetry)
        {
            self.saturation_asymmetry = 1.15;
        }
        if !self.saturation_bias.is_finite() || !(-0.02..=0.05).contains(&self.saturation_bias) {
            self.saturation_bias = 0.0;
        }
        if self.character_profile != crate::audio::VoiceCharacterProfile::Custom {
            if let Some(p) = self.character_profile.params() {
                if (self.presence_boost_db - p.presence_db).abs() > 0.25
                    || (self.saturation_drive - p.drive).abs() > 0.08
                    || (self.saturation_asymmetry - p.asymmetry).abs() > 0.08
                    || (self.proximity_gain_db - p.proximity_db).abs() > 0.25
                    || (self.air_boost_db - p.air_db).abs() > 0.25
                    || (self.compressor_ratio - p.compressor_ratio).abs() > 0.25
                {
                    self.character_profile = crate::audio::VoiceCharacterProfile::from_params(
                        self.presence_boost_db,
                        self.saturation_drive,
                        self.saturation_asymmetry,
                        self.proximity_gain_db,
                        self.air_boost_db,
                        self.compressor_ratio,
                    );
                }
            }
        }
        if self.saturation_profile != crate::audio::SaturationProfile::Custom {
            if let Some((d, a, b)) = self.saturation_profile.params() {
                if (self.saturation_drive - d).abs() > 0.06
                    || (self.saturation_asymmetry - a).abs() > 0.06
                    || (self.saturation_bias - b).abs() > 0.003
                {
                    self.saturation_profile = crate::audio::SaturationProfile::from_params(
                        self.saturation_drive,
                        self.saturation_asymmetry,
                        self.saturation_bias,
                    );
                }
            }
        }
    }
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new() -> Self {
        let base = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            root: base.join(".nektome-client"),
        }
    }

    pub fn load_or_create_device(&self) -> io::Result<DeviceIdentity> {
        let path = self.root.join("device.json");
        if let Ok(bytes) = fs::read(&path)
            && let Ok(mut device) = serde_json::from_slice::<DeviceIdentity>(&bytes)
            && !device.device_id.trim().is_empty()
        {
            if device.device_name.trim().is_empty() {
                device.device_name = default_device_name();
            }
            let _ = self.save_json(&path, &device);
            return Ok(device);
        }
        let device = DeviceIdentity {
            device_id: crate::auth::new_device_id(),
            device_name: default_device_name(),
        };
        self.save_json(&path, &device)?;
        Ok(device)
    }

    pub fn load_token(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(self.root.join("token.txt")) {
            Ok(token) if !token.trim().is_empty() => Ok(Some(token.trim().to_owned())),
            Ok(_) => Ok(None),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn load_or_create_voice_identity(&self, _device_id: &str) -> io::Result<VoiceIdentity> {
        let path = self.root.join("voice-user.json");
        if let Ok(bytes) = fs::read(&path)
            && let Ok(mut identity) = serde_json::from_slice::<VoiceIdentity>(&bytes)
        {
            identity.normalize();
            self.save_json(&path, &identity)?;
            return Ok(identity);
        }
        let legacy = self.load_legacy_settings();
        let identity = VoiceIdentity {
            user_id: legacy
                .and_then(|settings| settings.android_id)
                .map(|value| normalize_voice_user_id(&value))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(new_voice_user_id),
            last_connection_id: None,
            resume_peer_id: None,
            web_token: None,
        };
        self.save_json(&path, &identity)?;
        Ok(identity)
    }

    pub fn save_voice_identity(&self, identity: &VoiceIdentity) -> io::Result<()> {
        let mut normalized = VoiceIdentity {
            user_id: identity.user_id.clone(),
            last_connection_id: identity.last_connection_id.clone(),
            resume_peer_id: identity.resume_peer_id.clone(),
            web_token: identity.web_token.clone(),
        };
        normalized.normalize();
        self.save_json(&self.root.join("voice-user.json"), &normalized)
    }

    pub fn reset_voice_identity(&self) -> io::Result<VoiceIdentity> {
        let identity = VoiceIdentity {
            user_id: new_voice_user_id(),
            last_connection_id: None,
            resume_peer_id: None,
            web_token: None,
        };
        self.save_voice_identity(&identity)?;
        Ok(identity)
    }

    pub fn load_voice_preferences(&self) -> VoicePreferences {
        let path = self.root.join("voice-preferences.json");
        let (mut preferences, migrated) = match fs::read(&path)
            .ok()
            .and_then(|bytes| decode_voice_preferences(&bytes))
        {
            Some(preferences) => preferences,
            None => self
                .load_legacy_settings()
                .map(|settings| (VoicePreferences::from_legacy(settings), true))
                .unwrap_or_default(),
        };
        preferences.normalize();
        if migrated {
            let _ = self.save_json(&path, &preferences);
        }
        preferences
    }

    pub fn save_voice_preferences(&self, preferences: &VoicePreferences) -> io::Result<()> {
        let mut normalized = preferences.clone();
        normalized.normalize();
        self.save_json(&self.root.join("voice-preferences.json"), &normalized)
    }

    pub fn save_token(&self, token: &str) -> io::Result<()> {
        self.save_bytes_atomic(
            &self.root.join("token.txt"),
            format!("{token}\n").as_bytes(),
        )
    }

    pub fn clear_token(&self) -> io::Result<()> {
        match fs::remove_file(self.root.join("token.txt")) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn save_json<T: Serialize>(&self, path: &PathBuf, value: &T) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(value).expect("identity serialization cannot fail");
        self.save_bytes_atomic(path, &bytes)
    }

    fn save_bytes_atomic(&self, path: &PathBuf, bytes: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state.json");
        let temporary = path.with_file_name(format!(
            ".{file_name}.tmp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        if let Err(error) = fs::write(&temporary, bytes) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        match fs::rename(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) => {
                // Windows does not replace an existing destination on rename.
                // Remove only the exact state file being replaced, then retry.
                if error.kind() == io::ErrorKind::AlreadyExists {
                    fs::remove_file(path)?;
                    let result = fs::rename(&temporary, path);
                    if result.is_err() {
                        let _ = fs::remove_file(&temporary);
                    }
                    result
                } else {
                    let _ = fs::remove_file(&temporary);
                    Err(error)
                }
            }
        }
    }

    fn load_legacy_settings(&self) -> Option<LegacySettings> {
        let mut paths = vec![self.root.join("config.json")];
        if let Ok(executable) = std::env::current_exe()
            && let Some(directory) = executable.parent()
        {
            paths.push(directory.join("config.json"));
        }
        if let Ok(current_directory) = std::env::current_dir() {
            paths.push(current_directory.join("config.json"));
            // The former Avalonia application stored settings beside its
            // framework-dependent executable. Keep these paths as a
            // migration source after the Rust binary takes over.
            for configuration in ["Debug", "Release"] {
                paths.push(
                    current_directory
                        .join("src/NektoMe.Ui/bin")
                        .join(configuration)
                        .join("net10.0/config.json"),
                );
            }
        }
        paths.into_iter().find_map(|path| {
            fs::read(path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<LegacySettings>(&bytes).ok())
        })
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new()
    }
}

fn decode_voice_preferences(bytes: &[u8]) -> Option<(VoicePreferences, bool)> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let migrated_mp3 =
        value.get("record_sessions_mp3").is_some() && value.get("recording_format").is_none();
    let mut preferences = serde_json::from_value::<VoicePreferences>(value).ok()?;
    if migrated_mp3 {
        preferences.recording_format = RecordingFormat::Mp3;
    }
    Some((preferences, migrated_mp3))
}

#[cfg(test)]
mod tests {
    use super::{
        DeviceIdentity, LegacySettings, Store, VoiceIdentity, VoicePreferences,
        decode_voice_preferences, extract_auth_token, new_voice_user_id, new_web_token,
    };
    use crate::session_recorder::{RecordingFormat, RecordingParameters};

    #[test]
    fn legacy_device_identity_fields_are_read() {
        let device: DeviceIdentity =
            serde_json::from_str(r#"{"deviceId":"legacy-device","deviceName":"old-host"}"#)
                .expect("legacy device identity should remain readable");
        assert_eq!(device.device_id, "legacy-device");
        assert_eq!(device.device_name, "old-host");
    }

    #[test]
    fn legacy_device_identity_without_name_keeps_id_readable() {
        let device: DeviceIdentity = serde_json::from_str(r#"{"deviceId":"legacy-device"}"#)
            .expect("legacy device identity without a name should remain readable");
        assert_eq!(device.device_id, "legacy-device");
        assert!(device.device_name.is_empty());
    }

    #[test]
    fn identity_serialization_keeps_legacy_camel_case_keys() {
        let device = DeviceIdentity {
            device_id: "device-1".into(),
            device_name: "host".into(),
        };
        assert_eq!(
            serde_json::to_value(device).unwrap(),
            serde_json::json!({"deviceId":"device-1","deviceName":"host"})
        );

        let identity = VoiceIdentity {
            user_id: "google-user".into(),
            last_connection_id: Some("connection-1".into()),
            resume_peer_id: Some("peer-1".into()),
            web_token: None,
        };
        assert_eq!(
            serde_json::to_value(identity).unwrap(),
            serde_json::json!({
                "userId":"google-user",
                "connectionId":"connection-1",
                "resumePeerId":"peer-1"
            })
        );
    }

    #[test]
    fn legacy_settings_migrate_into_voice_preferences() {
        let settings: LegacySettings = serde_json::from_str(
            r#"{"WebToken":"web-token","SavedProxies":"http://u:p@host:1","MicGainPercent":275,"MicAgcEnabled":false,"ProxySellerListName":"legacy-list","ProxySellerPortsCount":12,"ProxySellerRotationSeconds":900,"ProxySellerWhitelistIp":"1.2.3.4"}"#,
        )
        .expect("legacy settings should remain readable");
        let preferences = VoicePreferences::from_legacy(settings);
        assert_eq!(preferences.web_token, "web-token");
        assert_eq!(preferences.proxy_url, "http://u:p@host:1");
        assert_eq!(preferences.microphone_gain_percent, 275.0);
        assert_eq!(preferences.proxy_seller_list_name, "legacy-list");
        assert_eq!(preferences.proxy_seller_ports_count, 12);
        assert_eq!(preferences.proxy_seller_rotation_seconds, 900);
        assert_eq!(preferences.proxy_seller_whitelist_ip, "1.2.3.4");
    }

    #[test]
    fn lower_camel_case_settings_are_imported() {
        let settings: LegacySettings = serde_json::from_str(
            r#"{"webToken":"token","androidId":"android","savedProxies":"http://host:1","micGainPercent":150}"#,
        )
        .unwrap();
        let preferences = VoicePreferences::from_legacy(settings);
        assert_eq!(preferences.web_token, "token");
        assert_eq!(preferences.microphone_gain_percent, 150.0);
        assert_eq!(preferences.proxy_url, "http://host:1");
    }

    #[test]
    fn legacy_voice_identity_fields_are_read_and_normalized() {
        let mut identity: VoiceIdentity =
            serde_json::from_str(r#"{"userId":"google-legacy","connectionId":"conn-1"}"#)
                .expect("legacy voice identity should remain readable");
        identity.normalize();
        assert_eq!(identity.user_id, "google-legacy");
        assert_eq!(identity.last_connection_id.as_deref(), Some("conn-1"));
        assert_eq!(identity.resume_peer_id, None);
    }

    #[test]
    fn compact_legacy_voice_identity_migrates_to_web_token() {
        let token = "0123456789abcdef0123456789abcdef";
        let mut identity: VoiceIdentity =
            serde_json::from_str(&format!(r#"{{"userId":"{token}","connectionId":"stale"}}"#))
                .expect("compact legacy identity should remain readable");
        identity.normalize();
        assert_eq!(identity.web_token.as_deref(), Some(token));
        assert!(identity.user_id.starts_with("google-"));
        assert_eq!(identity.last_connection_id, None);
        assert_eq!(identity.resume_peer_id, None);
    }

    #[test]
    fn voice_identity_normalization_trims_padded_fields() {
        let mut identity: VoiceIdentity = serde_json::from_str(
            r#"{"userId":"  google-user  ","connectionId":" \"conn-1\" ","webToken":" token "}"#,
        )
        .unwrap();
        identity.normalize();
        assert_eq!(identity.user_id, "google-user");
        assert_eq!(identity.last_connection_id.as_deref(), Some("conn-1"));
        assert_eq!(identity.web_token.as_deref(), Some("token"));
    }

    #[test]
    fn old_voice_preferences_get_current_audio_defaults() {
        let preferences: VoicePreferences =
            serde_json::from_str(r#"{"web_token":"legacy-token","user_sex":"ANY"}"#)
                .expect("legacy preferences should remain readable");
        assert_eq!(preferences.web_token, "legacy-token");
        assert_eq!(preferences.voice_server_host, "audio.nekto.me");
        assert!(!preferences.voice_server_override);
        assert_eq!(preferences.speaker_gain_percent, 100.0);
        assert!(preferences.speaker_dsp_enabled);
        assert!(preferences.microphone_dsp_enabled);
        assert!(preferences.echo_cancellation_enabled);
        assert!(!preferences.record_sessions);
        assert_eq!(preferences.recording_format, RecordingFormat::Flac);
        assert_eq!(
            preferences.recording_parameters,
            RecordingParameters::default()
        );
        assert!(preferences.microphone_device_name.is_empty());
        assert_eq!(
            preferences.saturation_profile,
            crate::audio::SaturationProfile::NaturalVoice
        );
        assert_eq!(preferences.presence_boost_db, 0.0);
        assert_eq!(preferences.proximity_gain_db, 0.0);
        assert_eq!(preferences.air_boost_db, 0.0);
        assert_eq!(
            preferences.character_profile,
            crate::audio::VoiceCharacterProfile::Natural
        );
        assert!(!preferences.push_to_talk);
    }

    #[test]
    fn explicit_saturation_profiles_round_trip() {
        for profile in crate::audio::SaturationProfile::ALL {
            let (d, a, b) = profile.params().unwrap_or((1.08, 1.15, 0.0));
            let preferences = VoicePreferences {
                saturation_profile: profile,
                saturation_drive: d,
                saturation_asymmetry: a,
                saturation_bias: b,
                ..VoicePreferences::default()
            };
            let encoded = serde_json::to_vec(&preferences).unwrap();
            let (decoded, _) = decode_voice_preferences(&encoded).unwrap();
            assert_eq!(decoded.saturation_profile, profile);
            assert_eq!(decoded.saturation_drive, d);
        }
    }

    #[test]
    fn explicit_character_profiles_round_trip() {
        for profile in crate::audio::VoiceCharacterProfile::ALL {
            let p = profile
                .params()
                .unwrap_or(crate::audio::CharacterPresetParams {
                    presence_db: 0.0,
                    drive: 1.08,
                    asymmetry: 1.15,
                    bias: 0.0,
                    proximity_db: 0.0,
                    air_db: 0.0,
                    compressor_ratio: 2.5,
                    makeup_gain: 1.35,
                });
            let preferences = VoicePreferences {
                character_profile: profile,
                presence_boost_db: p.presence_db,
                saturation_drive: p.drive,
                saturation_asymmetry: p.asymmetry,
                saturation_bias: p.bias,
                proximity_gain_db: p.proximity_db,
                air_boost_db: p.air_db,
                compressor_ratio: p.compressor_ratio,
                makeup_gain: p.makeup_gain,
                ..VoicePreferences::default()
            };
            let encoded = serde_json::to_vec(&preferences).unwrap();
            let (decoded, _) = decode_voice_preferences(&encoded).unwrap();
            assert_eq!(decoded.character_profile, profile);
            assert_eq!(decoded.proximity_gain_db, p.proximity_db);
            assert_eq!(decoded.air_boost_db, p.air_db);
        }
    }

    #[test]
    fn old_mp3_recording_preference_preserves_its_codec() {
        let (preferences, migrated) =
            decode_voice_preferences(br#"{"record_sessions_mp3":true}"#).unwrap();
        assert!(preferences.record_sessions);
        assert_eq!(preferences.recording_format, RecordingFormat::Mp3);
        assert!(migrated);
    }

    #[test]
    fn explicit_recording_formats_are_preserved() {
        for format in [RecordingFormat::Mp3, RecordingFormat::Opus] {
            let preferences = VoicePreferences {
                recording_format: format,
                ..VoicePreferences::default()
            };
            let encoded = serde_json::to_vec(&preferences).unwrap();
            let (decoded, migrated) = decode_voice_preferences(&encoded).unwrap();
            assert_eq!(decoded.recording_format, format);
            assert!(!migrated);
        }
    }

    #[test]
    fn voice_server_override_round_trips_with_preferences() {
        let preferences: VoicePreferences = serde_json::from_str(
            r#"{"voice_server_host":"audio.nekto-me.kz","voice_server_override":true}"#,
        )
        .unwrap();
        assert_eq!(preferences.voice_server_host, "audio.nekto-me.kz");
        assert!(preferences.voice_server_override);

        let encoded = serde_json::to_value(&preferences).unwrap();
        let decoded: VoicePreferences = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.voice_server_host, "audio.nekto-me.kz");
        assert!(decoded.voice_server_override);
    }

    #[test]
    fn empty_voice_server_override_falls_back_to_environment_resolution() {
        let mut preferences = VoicePreferences {
            voice_server_host: "  ".into(),
            voice_server_override: true,
            ..VoicePreferences::default()
        };
        preferences.normalize();
        assert_eq!(preferences.voice_server_host, "audio.nekto.me");
        assert!(!preferences.voice_server_override);
    }

    #[test]
    fn invalid_speaker_gain_is_reset_to_safe_default() {
        let mut preferences = VoicePreferences {
            speaker_gain_percent: 900.0,
            noise_gate_threshold: -1.0,
            compressor_ratio: 100.0,
            ..VoicePreferences::default()
        };
        preferences.normalize();
        assert_eq!(preferences.speaker_gain_percent, 100.0);
        assert_eq!(preferences.noise_gate_threshold, 0.002);
        assert_eq!(preferences.compressor_ratio, 2.5);
    }

    #[test]
    fn recording_parameters_are_normalized_with_preferences() {
        let mut preferences = VoicePreferences {
            recording_parameters: RecordingParameters {
                mp3_bitrate_kbps: 999,
                mp3_vbr_quality: -3.0,
                opus_bitrate_kbps: 0,
                opus_complexity: -1,
                ..RecordingParameters::default()
            },
            ..VoicePreferences::default()
        };

        preferences.normalize();

        assert_eq!(preferences.recording_parameters.mp3_bitrate_kbps, 320);
        assert!(preferences.recording_parameters.mp3_vbr_quality.abs() < f32::EPSILON);
        assert_eq!(preferences.recording_parameters.opus_bitrate_kbps, 32);
        assert_eq!(preferences.recording_parameters.opus_complexity, 0);
    }

    #[test]
    fn json_state_replacement_is_atomic_and_cleans_up_temporary_file() {
        let root =
            std::env::temp_dir().join(format!("nektome-persistence-test-{}", uuid::Uuid::new_v4()));
        let store = Store { root: root.clone() };
        let path = root.join("state.json");

        store
            .save_json(&path, &serde_json::json!({"version": 1}))
            .unwrap();
        store
            .save_json(&path, &serde_json::json!({"version": 2}))
            .unwrap();
        store.save_token("token-value").unwrap();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
            serde_json::json!({"version": 2})
        );
        assert_eq!(
            std::fs::read_to_string(root.join("token.txt")).unwrap(),
            "token-value\n"
        );
        assert!(
            !std::fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .any(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn persisted_text_preferences_are_trimmed_and_sexes_normalized() {
        let mut preferences = VoicePreferences {
            proxy_seller_api_key: "  key  ".into(),
            web_token: " token ".into(),
            voice_server_host: "  ".into(),
            proxy_url: "  first:1, second:2  ".into(),
            microphone_device_name: "  Studio Mic  ".into(),
            speaker_device_name: "  USB Headphones  ".into(),
            user_sex: " female ".into(),
            peer_sex: " male ".into(),
            ..VoicePreferences::default()
        };
        preferences.normalize();
        assert_eq!(preferences.proxy_seller_api_key, "key");
        assert_eq!(preferences.web_token, "token");
        assert_eq!(preferences.voice_server_host, "audio.nekto.me");
        assert_eq!(preferences.proxy_url, "first:1, second:2");
        assert_eq!(preferences.microphone_device_name, "Studio Mic");
        assert_eq!(preferences.speaker_device_name, "USB Headphones");
        assert_eq!(preferences.user_sex, "FEMALE");
        assert_eq!(preferences.peer_sex, "MALE");
    }

    #[test]
    fn chrome_token_extractor_reads_uuid_after_storage_marker() {
        let token = extract_auth_token(
            br#"storage_audio_v2\0"authToken":"123e4567-e89b-12d3-a456-426614174000""#,
        );
        assert_eq!(
            token.as_deref(),
            Some("123e4567-e89b-12d3-a456-426614174000")
        );
    }

    #[test]
    fn chrome_token_extractor_reads_compact_browser_token() {
        let token = "0123456789abcdef0123456789abcdef";
        let payload = format!(r#"storage_audio_v2\0"authToken":"{token}""#);
        assert_eq!(
            extract_auth_token(payload.as_bytes()).as_deref(),
            Some(token)
        );
    }

    #[test]
    fn new_web_tokens_match_browser_identity_format() {
        let token = new_web_token();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|value| value.is_ascii_hexdigit()));
        assert!(!token.contains('-'));
    }

    #[test]
    fn fresh_voice_identity_uses_android_store_prefix() {
        let user_id = new_voice_user_id();
        let (prefix, uuid) = user_id.split_once('-').expect("voice id has a prefix");
        assert_eq!(prefix, "google");
        assert!(uuid::Uuid::parse_str(uuid).is_ok());
    }

    #[test]
    fn voice_identity_input_strips_quotes_and_surrounding_whitespace() {
        assert_eq!(
            super::normalize_voice_user_id("  \"google-custom\"  "),
            "google-custom"
        );
    }

    #[test]
    fn voice_identity_reads_legacy_connection_id_last_alias() {
        let identity: VoiceIdentity =
            serde_json::from_str(r#"{"userId":"google-user","connectionIdLast":"conn-1"}"#)
                .unwrap();
        assert_eq!(identity.last_connection_id.as_deref(), Some("conn-1"));
    }
}
