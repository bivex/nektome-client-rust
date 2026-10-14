use crate::domain::{AgeGroup, AppConfig, Dialog, Message, Sex, UserId, VoiceConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SERVER: &str = "wss://im.nektome.online/android/?EIO=3&transport=websocket";
const WEB_CLIENT_VERSION: i32 = 14;

pub fn local_locale() -> String {
    std::env::var("LANG")
        .ok()
        .and_then(|value| value.split(['_', '-']).next().map(str::to_owned))
        .filter(|value| value.len() == 2 && value.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or_else(|| "en".into())
}

pub fn local_time_zone() -> String {
    if let Some(zone) = env_text("TZ") {
        return zone;
    }
    if let Ok(target) = std::fs::read_link("/etc/localtime")
        && let Some(target) = target.to_str()
        && let Some((_, zone)) = target.split_once("zoneinfo/")
    {
        return zone.to_owned();
    }
    format_utc_offset(chrono::Local::now().offset().local_minus_utc())
}

/// Browser-profile metadata shared by the CLI and GUI authentication paths.
/// Explicit environment values override the detected locale and timezone.
#[derive(Clone, Debug, Default)]
pub struct AuthOptions {
    pub locale: Option<String>,
    pub time_zone: Option<String>,
}

impl AuthOptions {
    pub fn from_env() -> Self {
        Self {
            locale: env_text("NEKTOME_LOCALE"),
            time_zone: env_text("NEKTOME_TIME_ZONE"),
        }
    }
}

fn env_text(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn format_utc_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let magnitude = seconds.unsigned_abs();
    let hours = magnitude / 3_600;
    let minutes = (magnitude % 3_600) / 60;
    format!("{sign}{hours:02}:{minutes:02}")
}

#[derive(Clone, Debug)]
pub struct SearchCriteria {
    pub my_sex: Option<Sex>,
    pub wish_sex: Option<Sex>,
    pub my_age: Option<Vec<i32>>,
    pub wish_age: Option<Vec<(i32, i32)>>,
    pub include_adult: bool,
    pub role_play: bool,
    pub disable_voice: bool,
}

fn sex(value: Option<Sex>) -> Option<&'static str> {
    value.map(|s| match s {
        Sex::Female => "F",
        Sex::Male => "M",
    })
}

pub fn action(name: &str) -> Value {
    json!({"action": name})
}

pub fn search_run(c: &SearchCriteria) -> Value {
    let wish_age = c.wish_age.as_ref().map(|ranges| {
        ranges
            .iter()
            .map(|(a, b)| json!([a, b]))
            .collect::<Vec<_>>()
    });
    without_null_fields(
        json!({"action":"search.run", "isAdult": c.include_adult.then_some(true), "isRole":c.role_play,
        "isVoiceDisable":c.disable_voice.then_some(true), "myAge":c.my_age, "mySex":sex(c.my_sex),
        "wishAge":wish_age, "wishSex":sex(c.wish_sex)}),
    )
}

pub fn auth_get_token(device_id: &str, device_name: &str, key: &str) -> Value {
    auth_get_token_with_options(device_id, device_name, key, &AuthOptions::from_env())
}

pub fn auth_get_token_with_options(
    _device_id: &str,
    _device_name: &str,
    _key: &str,
    options: &AuthOptions,
) -> Value {
    json!({"action":"auth.getToken", "deviceType":2, "isTouch":false,
        "version":WEB_CLIENT_VERSION, "t":chrono::Utc::now().timestamp_millis(),
        "timeZone":options.time_zone.clone().unwrap_or_else(local_time_zone),
        "locale":options.locale.clone().unwrap_or_else(local_locale)})
}

pub fn auth_send_token(token: &str, key: &str) -> Value {
    auth_send_token_with_options(token, key, &AuthOptions::from_env())
}

pub fn auth_send_token_with_options(token: &str, _key: &str, options: &AuthOptions) -> Value {
    json!({"action":"auth.sendToken", "token":token, "deviceType":2, "isTouch":false,
        "version":WEB_CLIENT_VERSION, "t":chrono::Utc::now().timestamp_millis(),
        "timeZone":options.time_zone.clone().unwrap_or_else(local_time_zone),
        "locale":options.locale.clone().unwrap_or_else(local_locale)})
}

fn without_null_fields(mut value: Value) -> Value {
    if let Value::Object(fields) = &mut value {
        fields.retain(|_, value| !value.is_null());
    }
    value
}

pub fn message(dialog_id: i64, text: &str, random_id: i64) -> Value {
    json!({"action":"anon.message", "dialogId":dialog_id, "message":text, "randomId":random_id})
}

pub fn dialog_typing(dialog_id: i64, typing: bool, voice: bool) -> Value {
    json!({"action":"dialog.setTyping", "dialogId":dialog_id, "typing":typing, "voice":voice})
}

pub fn dialog_info(dialog_id: i64) -> Value {
    json!({"action":"dialog.info", "dialogId":dialog_id})
}

pub fn online_track(enabled: bool) -> Value {
    json!({"action":"online.track", "on":enabled})
}

pub fn report_dialog(dialog_id: i64, message_id: i64, reason_id: i32) -> Value {
    json!({"action":"antispam.report", "dialogId":dialog_id, "messageId":message_id, "reasonId":reason_id})
}

pub fn leave_dialog(dialog_id: i64) -> Value {
    json!({"action":"anon.leaveDialog", "dialogId":dialog_id})
}

pub fn read_messages(dialog_id: i64, last_message_id: i64) -> Value {
    json!({"action":"anon.readMessages", "dialogId":dialog_id, "lastMessageId":last_message_id})
}

pub fn captcha_verify(solution: &str) -> Value {
    json!({"action":"captcha.verify", "solution":solution})
}

/// Decode the canonical server message shape used by both the desktop and CLI
/// frontends. Keeping this mapping here prevents the two Rust frontends from
/// drifting apart as the wire protocol evolves.
pub fn parse_message(data: &Value) -> Option<Message> {
    let id = data.get("id").and_then(Value::as_i64)?;
    let text = data.get("message").and_then(Value::as_str)?;
    Message::new(
        id,
        data.get("dialogId")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        data.get("senderId")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        text.to_owned(),
        from_unix(data.get("createTime").and_then(Value::as_i64)),
        data.get("randomId")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        data.get("read").and_then(Value::as_bool).unwrap_or(false),
    )
    .ok()
}

/// Hydrate a dialog and its history from the server's `dialog.opened` /
/// `dialog.info` payload. Invalid history entries are ignored just as the
/// legacy session ignores messages it cannot map.
pub fn parse_dialog(data: &Value) -> Option<Dialog> {
    let id = data.get("id").and_then(Value::as_i64)?;
    let interlocutors = data
        .get("interlocutors")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .map(UserId)
        .collect();
    let mut dialog = Dialog::new(
        id,
        interlocutors,
        data.get("supportVoice")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        data.get("createTime")
            .and_then(Value::as_i64)
            .map(|value| from_unix(Some(value))),
        data.get("updateTime")
            .and_then(Value::as_i64)
            .map(|value| from_unix(Some(value))),
    );
    if let Some(messages) = data.get("messages").and_then(Value::as_array) {
        for mut message in messages.iter().filter_map(parse_message) {
            if message.dialog_id.0 == 0 {
                message.dialog_id.0 = id;
            }
            let _ = dialog.try_add_message(message);
        }
    }
    if data.get("close").and_then(Value::as_bool) == Some(true) {
        dialog.close();
    }
    Some(dialog)
}

pub fn communication_banned(data: &Value) -> bool {
    data.get("communicationBan")
        .and_then(Value::as_bool)
        .or_else(|| data.pointer("/config/banTalk").and_then(Value::as_bool))
        .or_else(|| {
            data.pointer("/statusInfo/communicationBan")
                .and_then(Value::as_bool)
        })
        == Some(true)
}

pub fn app_config(data: &Value) -> AppConfig {
    fn age_groups(value: Option<&Value>) -> Vec<AgeGroup> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|item| AgeGroup {
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                to: item
                    .get("to")
                    .and_then(Value::as_i64)
                    .map(|value| value as i32),
                selected: item
                    .get("selected")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
            .collect()
    }

    let ages = data.get("ages");
    let mut adult_ages = age_groups(ages.and_then(|value| value.get("adult")));
    if adult_ages.is_empty() {
        adult_ages = age_groups(data.get("rulesAdult"));
    }
    let mut communication_ages = age_groups(ages.and_then(|value| value.get("communication")));
    if communication_ages.is_empty() {
        communication_ages = age_groups(data.get("rulesTalk"));
    }
    let mut role_ages = age_groups(ages.and_then(|value| value.get("role")));
    if role_ages.is_empty() {
        role_ages = age_groups(data.get("rulesRole"));
    }
    let voice = data
        .get("voiceConfig")
        .filter(|value| value.is_object())
        .map(|value| VoiceConfig {
            available_seconds: value
                .get("availableSeconds")
                .and_then(Value::as_i64)
                .unwrap_or_default() as i32,
            max_seconds: value
                .get("maxSeconds")
                .and_then(Value::as_i64)
                .unwrap_or_default() as i32,
            min_seconds: value
                .get("minSeconds")
                .and_then(Value::as_i64)
                .unwrap_or_default() as i32,
            beta: value.get("beta").and_then(Value::as_bool).unwrap_or(false),
        });
    AppConfig {
        alert_info: data
            .get("alertInfo")
            .and_then(Value::as_str)
            .map(str::to_owned),
        ban_talk: data
            .get("banTalk")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rules_enabled: data
            .get("rulesEnable")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        audio_chat_enabled: data
            .get("audioChat")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        subscriptions_disabled: data
            .get("subsDisabled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        report_reasons: data
            .get("reportReasons")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        adult_ages,
        communication_ages,
        role_ages,
        voice,
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct NoticeEnvelope {
    pub notice: Option<String>,
    pub data: Option<Value>,
    pub badge: Option<i64>,
    pub page: Option<i64>,
}

pub fn parse_socket_event(raw: &str) -> Option<NoticeEnvelope> {
    if !raw.starts_with("42") {
        return None;
    }
    let event: Vec<Value> = serde_json::from_str(&raw[2..]).ok()?;
    if event.first()?.as_str()? != "notice" {
        return None;
    }
    serde_json::from_value(event.get(1)?.clone()).ok()
}

#[derive(Debug, Serialize)]
struct SocketEvent<'a>(&'a str, Value);

pub fn socket_action(payload: Value) -> String {
    serde_json::to_string(&SocketEvent("action", payload))
        .map(|s| format!("42{s}"))
        .expect("JSON serialization cannot fail")
}

pub fn from_unix(value: Option<i64>) -> i64 {
    match value {
        Some(v) if v >= 1_000_000_000_000 => v / 1000,
        Some(v) if v != 0 => v,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn socket_action_has_socket_io_event_frame() {
        assert_eq!(
            socket_action(action("search.sendOut")),
            "42[\"action\",{\"action\":\"search.sendOut\"}]"
        );
    }
    #[test]
    fn parses_notice_frame() {
        let n = parse_socket_event("42[\"notice\",{\"notice\":\"search.success\"}]").unwrap();
        assert_eq!(n.notice.as_deref(), Some("search.success"));
    }

    #[test]
    fn notice_parser_preserves_optional_metadata() {
        let n = parse_socket_event(
            "42[\"notice\",{\"notice\":\"messages.new\",\"badge\":2,\"page\":3}]",
        )
        .unwrap();
        assert_eq!(n.badge, Some(2));
        assert_eq!(n.page, Some(3));
    }

    #[test]
    fn unix_timestamp_conversion_preserves_missing_values() {
        assert_eq!(from_unix(None), 0);
        assert_eq!(from_unix(Some(0)), 0);
        assert_eq!(from_unix(Some(1_700_000_000)), 1_700_000_000);
        assert_eq!(from_unix(Some(1_700_000_000_000)), 1_700_000_000);
    }
    #[test]
    fn detects_milliseconds() {
        assert_eq!(from_unix(Some(1_700_000_000_000)), 1_700_000_000);
    }
    #[test]
    fn typing_action_matches_chat_wire_shape() {
        assert_eq!(
            dialog_typing(7, true, false),
            json!({"action":"dialog.setTyping","dialogId":7,"typing":true,"voice":false})
        );
    }
    #[test]
    fn dialog_info_and_online_tracking_actions_match_wire_names() {
        assert_eq!(dialog_info(9)["action"], "dialog.info");
        assert_eq!(
            online_track(true),
            json!({"action":"online.track","on":true})
        );
    }
    #[test]
    fn report_action_contains_dialog_message_and_reason() {
        assert_eq!(
            report_dialog(9, 12, 1),
            json!({"action":"antispam.report","dialogId":9,"messageId":12,"reasonId":1})
        );
    }

    #[test]
    fn auth_payload_uses_current_web_profile() {
        let payload = auth_get_token("device-1", "Desktop", "key");
        assert_eq!(payload["deviceType"], 2);
        assert_eq!(payload["version"], WEB_CLIENT_VERSION);
        assert_eq!(payload["isTouch"], false);
        assert!(payload["t"].as_i64().is_some_and(|value| value > 0));
        assert!(payload.get("deviceId").is_none());
        assert!(payload.get("key").is_none());
        assert!(payload.get("sigKey").is_none());
    }

    #[test]
    fn auth_options_set_current_locale_and_timezone_fields() {
        let options = AuthOptions {
            locale: Some("ru".into()),
            time_zone: Some("Europe/Moscow".into()),
        };
        let get = auth_get_token_with_options("device-1", "Desktop", "key", &options);
        assert_eq!(get["locale"], "ru");
        assert_eq!(get["timeZone"], "Europe/Moscow");
        assert_eq!(get["deviceType"], 2);
        assert_eq!(get["version"], WEB_CLIENT_VERSION);
        assert!(get.get("deviceId").is_none());
        assert!(get.get("sigKey").is_none());
        assert!(get.get("vending").is_none());
        assert!(get.get("push").is_none());

        let send = auth_send_token_with_options("token", "key", &options);
        assert_eq!(send["token"], "token");
        assert_eq!(send["deviceType"], 2);
        assert_eq!(send["timeZone"], "Europe/Moscow");
        assert_eq!(send["locale"], "ru");
        assert!(send.get("key").is_none());
        assert!(send.get("pushToken").is_none());
    }

    #[test]
    fn communication_ban_accepts_legacy_auth_payload_locations() {
        assert!(communication_banned(&json!({
            "statusInfo": {"communicationBan": true}
        })));
        assert!(communication_banned(&json!({"config": {"banTalk": true}})));
        assert!(communication_banned(&json!({"communicationBan": true})));
        assert!(!communication_banned(
            &json!({"statusInfo": {"communicationBan": false}})
        ));
    }

    #[test]
    fn app_config_maps_server_feature_flags_and_age_groups() {
        let config = app_config(&json!({
            "alertInfo": "hello",
            "banTalk": true,
            "audioChat": true,
            "reportReasons": ["spam"],
            "ages": {"adult": [{"name": "18+", "to": 99, "selected": true}]},
            "voiceConfig": {"availableSeconds": 30, "maxSeconds": 60, "minSeconds": 5, "beta": true}
        }));
        assert_eq!(config.alert_info.as_deref(), Some("hello"));
        assert!(config.ban_talk && config.audio_chat_enabled);
        assert_eq!(config.report_reasons, vec!["spam"]);
        assert_eq!(config.adult_ages[0].to, Some(99));
        assert_eq!(config.voice.unwrap().max_seconds, 60);
    }

    #[test]
    fn app_config_accepts_legacy_rules_age_group_aliases() {
        let config = app_config(&json!({
            "rulesAdult": [{"name": "adult", "to": 99}],
            "rulesTalk": [{"name": "talk", "to": 17}],
            "rulesRole": [{"name": "role", "to": 24}]
        }));
        assert_eq!(config.adult_ages[0].name, "adult");
        assert_eq!(config.communication_ages[0].to, Some(17));
        assert_eq!(config.role_ages[0].name, "role");
    }

    #[test]
    fn null_voice_config_is_not_mapped_to_an_empty_config() {
        let config = app_config(&json!({"voiceConfig": null}));
        assert_eq!(config.voice, None);
    }

    #[test]
    fn timezone_offsets_use_the_reference_numeric_shape() {
        assert_eq!(format_utc_offset(0), "+00:00");
        assert_eq!(format_utc_offset(3 * 3_600), "+03:00");
        assert_eq!(format_utc_offset(-(5 * 3_600 + 30 * 60)), "-05:30");
    }

    #[test]
    fn search_payload_omits_unselected_optional_filters() {
        let payload = search_run(&SearchCriteria {
            my_sex: None,
            wish_sex: None,
            my_age: None,
            wish_age: None,
            include_adult: false,
            role_play: false,
            disable_voice: false,
        });
        assert_eq!(payload, json!({"action":"search.run", "isRole":false}));
    }

    #[test]
    fn message_parser_maps_wire_fields_and_rejects_blank_text() {
        let parsed = parse_message(&json!({
            "id": 7,
            "dialogId": 12,
            "senderId": 9,
            "message": "hello",
            "createTime": 1_700_000_000_000i64,
            "randomId": 44,
            "read": true
        }))
        .expect("valid wire message should parse");
        assert_eq!(parsed.id.0, 7);
        assert_eq!(parsed.dialog_id.0, 12);
        assert_eq!(parsed.sender_id.0, 9);
        assert_eq!(parsed.random_id, 44);
        assert!(parsed.read);
        assert!(parse_message(&json!({"id": 8, "message": "  "})).is_none());
    }

    #[test]
    fn dialog_parser_hydrates_history_and_closed_state() {
        let dialog = parse_dialog(&json!({
            "id": 12,
            "interlocutors": [7, 9],
            "supportVoice": true,
            "messages": [
                {"id": 1, "dialogId": 12, "senderId": 7, "message": "hi"},
                {"id": 2, "senderId": 9, "message": "there"},
                {"id": 3, "dialogId": 12, "message": "  "}
            ],
            "close": true
        }))
        .expect("valid dialog should parse");
        assert_eq!(dialog.id.0, 12);
        assert_eq!(dialog.interlocutors.len(), 2);
        assert!(dialog.supports_voice);
        assert_eq!(dialog.messages.len(), 2);
        assert_eq!(dialog.messages[1].dialog_id.0, 12);
        assert_eq!(dialog.status, crate::domain::DialogStatus::Closed);
    }
}
