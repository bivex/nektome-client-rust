use crate::voice;
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum VoiceState {
    #[default]
    Disconnected,
    Registered,
    Searching,
    Connecting,
    InCall,
    Muted,
}

#[derive(Debug, Default)]
pub struct VoiceSession {
    state: VoiceState,
    connection_id: Option<String>,
    resume_peer_id: Option<String>,
    peer_muted: bool,
    pub initiator: bool,
    pub remote_description_set: bool,
    pending_candidates: VecDeque<Value>,
}

impl VoiceSession {
    pub fn state(&self) -> VoiceState {
        self.state
    }
    pub fn connection_id(&self) -> Option<&str> {
        self.connection_id.as_deref()
    }
    pub fn resume_peer_id(&self) -> Option<&str> {
        self.resume_peer_id.as_deref()
    }
    pub fn peer_muted(&self) -> bool {
        self.peer_muted
    }
    pub fn pending_candidate_count(&self) -> usize {
        self.pending_candidates.len()
    }

    pub fn registered(&mut self, connection_id: Option<String>) {
        self.connection_id = connection_id;
        self.resume_peer_id = None;
        self.peer_muted = false;
        self.initiator = false;
        self.remote_description_set = false;
        self.pending_candidates.clear();
        self.state = VoiceState::Registered;
    }

    pub fn start_search(&mut self) -> Value {
        self.state = VoiceState::Searching;
        voice::scan_for_peer("ANY", "ANY", None, &[], None)
    }

    pub fn stop_search(&mut self) -> Value {
        self.state = VoiceState::Registered;
        voice::type_only("stop-scan")
    }

    pub fn set_muted(&mut self, muted: bool) -> Option<Value> {
        if !matches!(
            self.state,
            VoiceState::Connecting | VoiceState::InCall | VoiceState::Muted
        ) {
            return None;
        }
        let connection_id = self.connection_id.as_deref()?;
        self.state = if muted {
            VoiceState::Muted
        } else {
            VoiceState::InCall
        };
        Some(serde_json::json!({"type":"peer-mute","connectionId":connection_id,"muted":muted}))
    }

    pub fn disconnect(&mut self) -> Option<Value> {
        let had_peer = matches!(
            self.state,
            VoiceState::Connecting | VoiceState::InCall | VoiceState::Muted
        );
        let connection_id = self.connection_id.take();
        self.resume_peer_id = None;
        self.peer_muted = false;
        self.state = VoiceState::Disconnected;
        self.remote_description_set = false;
        self.pending_candidates.clear();
        had_peer
            .then_some(connection_id)
            .flatten()
            .map(|value| voice::connection_event("peer-disconnect", &value))
    }

    pub fn matches_current_peer(&self, event: &Value) -> bool {
        match event
            .get("connectionId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(incoming) => self.connection_id.as_deref() == Some(incoming),
            None => self.connection_id.is_some(),
        }
    }

    /// Applies a server event and returns candidates that become usable once a
    /// remote description is installed. SDP creation itself belongs to media.
    pub fn on_event(&mut self, event: &Value) -> Vec<Value> {
        match event.get("type").and_then(Value::as_str) {
            Some("registered") => {
                if event.get("success").and_then(Value::as_bool) == Some(false) {
                    self.connection_id = None;
                    self.resume_peer_id = None;
                    self.peer_muted = false;
                    self.initiator = false;
                    self.remote_description_set = false;
                    self.pending_candidates.clear();
                    self.state = VoiceState::Disconnected;
                    return Vec::new();
                }
                self.registered(
                    event
                        .get("connectionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                );
                Vec::new()
            }
            Some("peer-connect") => {
                let Some(connection_id) = event
                    .get("connectionId")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .map(|value| value.trim().to_owned())
                else {
                    return Vec::new();
                };
                if Some(connection_id.as_str()) == self.connection_id.as_deref()
                    && matches!(
                        self.state,
                        VoiceState::Connecting | VoiceState::InCall | VoiceState::Muted
                    )
                {
                    return Vec::new();
                }
                self.connection_id = Some(connection_id);
                self.resume_peer_id = None;
                self.peer_muted = false;
                self.initiator = event
                    .get("initiator")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.remote_description_set = false;
                self.state = VoiceState::Connecting;
                Vec::new()
            }
            Some("search.success") => {
                self.state = VoiceState::Searching;
                Vec::new()
            }
            Some("search.out") | Some("search.stop") => {
                if matches!(self.state, VoiceState::Searching) {
                    self.state = VoiceState::Registered;
                }
                Vec::new()
            }
            Some("captcha-request") => {
                if matches!(self.state, VoiceState::Searching) {
                    self.state = VoiceState::Registered;
                }
                Vec::new()
            }
            Some("error") => {
                if error_is_ban(event) {
                    self.connection_id = None;
                    self.resume_peer_id = None;
                    self.peer_muted = false;
                    self.initiator = false;
                    self.remote_description_set = false;
                    self.pending_candidates.clear();
                    self.state = VoiceState::Disconnected;
                } else if matches!(self.state, VoiceState::Searching) {
                    self.state = VoiceState::Registered;
                }
                Vec::new()
            }
            Some("ban") => {
                self.connection_id = None;
                self.resume_peer_id = None;
                self.peer_muted = false;
                self.initiator = false;
                self.remote_description_set = false;
                self.pending_candidates.clear();
                self.state = VoiceState::Disconnected;
                Vec::new()
            }
            Some("offer") | Some("answer") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                self.remote_description_set = true;
                self.pending_candidates.drain(..).collect()
            }
            Some("ice-candidate") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                let candidate = event.get("candidate").cloned().unwrap_or(Value::Null);
                let candidate_is_empty = candidate.is_null()
                    || candidate
                        .as_str()
                        .is_some_and(|value| value.trim().is_empty())
                    || candidate.get("candidate").is_some_and(|value| {
                        value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
                    });
                if candidate_is_empty {
                    return Vec::new();
                }
                if self.remote_description_set {
                    vec![candidate]
                } else if self
                    .pending_candidates
                    .iter()
                    .all(|queued| queued != &candidate)
                {
                    self.pending_candidates.push_back(candidate);
                    if self.pending_candidates.len() > 32 {
                        self.pending_candidates.pop_front();
                    }
                    Vec::new()
                } else {
                    Vec::new()
                }
            }
            Some("stream-received") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                if self.state != VoiceState::Muted {
                    self.state = VoiceState::InCall;
                }
                Vec::new()
            }
            Some("peer-connection") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                if event.get("connection") == Some(&Value::Bool(false)) {
                    self.peer_muted = false;
                    self.remote_description_set = false;
                    self.pending_candidates.clear();
                    // Media going down does not mean that the server accepted a
                    // new scan. Keep the registered/session context so the UI
                    // can distinguish an idle route from an active search.
                    self.state = VoiceState::Registered;
                } else if self.state != VoiceState::Muted {
                    self.state = VoiceState::InCall;
                }
                Vec::new()
            }
            Some("peer-mute") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                self.peer_muted = event.get("muted").and_then(Value::as_bool).unwrap_or(false);
                Vec::new()
            }
            Some("peer-disconnect") | Some("peer-soft-disconnect") => {
                if !self.matches_current_peer(event) {
                    return Vec::new();
                }
                let is_soft =
                    event.get("type").and_then(Value::as_str) == Some("peer-soft-disconnect");
                self.resume_peer_id = is_soft
                    .then(|| {
                        self.connection_id.clone().or_else(|| {
                            event
                                .get("connectionId")
                                .and_then(Value::as_str)
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_owned)
                        })
                    })
                    .flatten();
                self.connection_id = None;
                self.peer_muted = false;
                self.remote_description_set = false;
                self.pending_candidates.clear();
                self.state = VoiceState::Searching;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

fn error_is_ban(event: &Value) -> bool {
    let nested = event.get("error").filter(|value| value.is_object());
    let code = nested
        .and_then(|value| value.get("code"))
        .or_else(|| event.get("code"))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|number| number.to_string()))
        });
    if code.as_deref() == Some("201") {
        return true;
    }
    let description = nested
        .and_then(|value| value.get("text"))
        .or_else(|| nested.and_then(|value| value.get("message")))
        .or_else(|| nested.and_then(|value| value.get("description")))
        .or_else(|| event.get("message"))
        .or_else(|| event.get("description"))
        .or_else(|| event.get("error"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_lowercase();
    description.contains("banned")
        || description.contains("blocked")
        || description.contains("заблокирован")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers_ice_until_remote_description_arrives() {
        let mut session = VoiceSession::default();
        session.on_event(
            &serde_json::json!({"type":"peer-connect","connectionId":"p1","initiator":true}),
        );
        session.on_event(&serde_json::json!({
            "type": "ice-candidate",
            "connectionId": "p1",
            "candidate": null
        }));
        session.on_event(&serde_json::json!({
            "type": "ice-candidate",
            "connectionId": "p1",
            "candidate": "  "
        }));
        assert!(
            session
                .on_event(&serde_json::json!({"type":"ice-candidate","connectionId":"p1","candidate":"c1"}))
                .is_empty()
        );
        assert_eq!(session.pending_candidate_count(), 1);
        let candidates =
            session.on_event(&serde_json::json!({"type":"offer","connectionId":"p1","offer":"{}"}));
        assert_eq!(candidates, vec![Value::String("c1".into())]);
        assert!(session.remote_description_set);
    }

    #[test]
    fn stream_received_enters_call_and_disconnect_returns_searching() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({"type":"peer-connect","connectionId":"p1"}));
        session.on_event(&serde_json::json!({"type":"stream-received","connectionId":"p1"}));
        assert_eq!(session.state(), VoiceState::InCall);
        session.on_event(&serde_json::json!({"type":"peer-disconnect","connectionId":"p1"}));
        assert_eq!(session.state(), VoiceState::Searching);
    }

    #[test]
    fn peer_connection_media_down_leaves_call_ready_for_explicit_search() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "p1"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "p1",
            "connection": true
        }));
        assert_eq!(session.state(), VoiceState::InCall);
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "p1",
            "connection": false
        }));
        assert_eq!(session.state(), VoiceState::Registered);
        assert_eq!(session.connection_id(), Some("p1"));
        assert!(!session.remote_description_set);
    }

    #[test]
    fn soft_disconnect_retains_resume_peer_but_hard_disconnect_does_not() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer-1"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-soft-disconnect"
        }));
        assert_eq!(session.connection_id(), None);
        assert_eq!(session.resume_peer_id(), Some("peer-1"));

        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer-2"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-disconnect",
            "connectionId": "peer-2"
        }));
        assert_eq!(session.resume_peer_id(), None);
    }

    #[test]
    fn registered_event_updates_resume_connection_state() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "registered",
            "connectionId": "c1"
        }));
        assert_eq!(session.state(), VoiceState::Registered);
        assert_eq!(session.connection_id(), Some("c1"));
    }

    #[test]
    fn search_events_update_voice_state() {
        let mut session = VoiceSession::default();
        session.registered(Some("c1".into()));
        session.on_event(&serde_json::json!({"type": "search.success"}));
        assert_eq!(session.state(), VoiceState::Searching);
        session.on_event(&serde_json::json!({"type": "search.stop"}));
        assert_eq!(session.state(), VoiceState::Registered);
        session.on_event(&serde_json::json!({"type": "search.success"}));
        session.on_event(&serde_json::json!({"type": "search.out"}));
        assert_eq!(session.state(), VoiceState::Registered);
    }

    #[test]
    fn captcha_pauses_search_and_ban_disconnects() {
        let mut session = VoiceSession::default();
        session.registered(Some("c1".into()));
        session.on_event(&serde_json::json!({"type": "search.success"}));
        session.on_event(&serde_json::json!({"type": "captcha-request"}));
        assert_eq!(session.state(), VoiceState::Registered);

        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer-1"
        }));
        session.on_event(&serde_json::json!({"type": "ban"}));
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);
        assert_eq!(session.resume_peer_id(), None);
    }

    #[test]
    fn non_ban_error_stops_search_without_tearing_down_registration() {
        let mut session = VoiceSession::default();
        session.registered(Some("c1".into()));
        session.on_event(&serde_json::json!({"type": "search.success"}));
        session.on_event(&serde_json::json!({
            "type": "error",
            "code": "temporary",
            "message": "try again"
        }));
        assert_eq!(session.state(), VoiceState::Registered);
        assert_eq!(session.connection_id(), Some("c1"));
    }

    #[test]
    fn ban_shaped_error_disconnects_the_voice_session() {
        let mut session = VoiceSession::default();
        session.registered(Some("c1".into()));
        session.on_event(&serde_json::json!({"type": "search.success"}));
        session.on_event(&serde_json::json!({
            "type": "error",
            "error": {"code": 201, "text": "IP blocked"}
        }));
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);

        session.registered(Some("c2".into()));
        session.on_event(&serde_json::json!({
            "type": "error",
            "message": "user is banned"
        }));
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);
    }

    #[test]
    fn failed_registration_does_not_enter_registered_state() {
        let mut session = VoiceSession::default();
        session.registered(Some("old".into()));
        session.on_event(&serde_json::json!({
            "type": "registered",
            "success": false,
            "connectionId": "rejected"
        }));
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);
    }

    #[test]
    fn mute_is_only_emitted_for_an_active_call() {
        let mut session = VoiceSession::default();
        session.registered(Some("registration".into()));
        assert!(session.set_muted(true).is_none());

        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer"
        }));
        assert!(session.set_muted(true).is_some());
        session.on_event(&serde_json::json!({
            "type": "stream-received",
            "connectionId": "peer"
        }));
        assert_eq!(session.state(), VoiceState::Muted);
        assert_eq!(
            session.set_muted(true),
            Some(serde_json::json!({
                "type": "peer-mute",
                "connectionId": "peer",
                "muted": true
            }))
        );
    }

    #[test]
    fn repeated_peer_connect_is_idempotent_for_active_peer() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "p1",
            "initiator": true
        }));
        session.on_event(&serde_json::json!({
            "type": "offer",
            "connectionId": "p1"
        }));
        assert!(session.remote_description_set);
        assert!(session.set_muted(true).is_some());
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "p1",
            "initiator": false
        }));
        assert!(session.remote_description_set);
        assert_eq!(session.state(), VoiceState::Muted);
        assert_eq!(session.connection_id(), Some("p1"));
    }

    #[test]
    fn stale_peer_events_do_not_mutate_active_negotiation() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "current"
        }));
        session.on_event(&serde_json::json!({
            "type": "ice-candidate",
            "connectionId": "stale",
            "candidate": "old"
        }));
        assert_eq!(session.pending_candidate_count(), 0);
        session.on_event(&serde_json::json!({
            "type": "peer-disconnect",
            "connectionId": "stale"
        }));
        assert_eq!(session.connection_id(), Some("current"));
    }

    #[test]
    fn stale_peer_connection_down_does_not_end_active_call() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "current"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "current",
            "connection": true
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "stale",
            "connection": false
        }));
        assert_eq!(session.state(), VoiceState::InCall);
        assert_eq!(session.connection_id(), Some("current"));
    }

    #[test]
    fn peer_connection_down_returns_to_registered_without_starting_search() {
        let mut session = VoiceSession::default();
        session.registered(Some("registered-connection".into()));
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "peer",
            "connection": true
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "peer",
            "connection": false
        }));
        assert_eq!(session.state(), VoiceState::Registered);
        assert_eq!(session.connection_id(), Some("peer"));
    }

    #[test]
    fn events_without_connection_id_use_the_active_peer_context() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "current"
        }));
        session.on_event(&serde_json::json!({
            "type": "peer-mute",
            "muted": true
        }));
        assert_eq!(session.connection_id(), Some("current"));
        assert!(session.peer_muted());
        assert_eq!(session.state(), VoiceState::Connecting);
        session.on_event(&serde_json::json!({"type": "stream-received"}));
        assert_eq!(session.state(), VoiceState::InCall);
        session.on_event(&serde_json::json!({
            "type": "peer-mute",
            "muted": false
        }));
        assert!(!session.peer_muted());
        session.on_event(&serde_json::json!({"type": "peer-disconnect"}));
        assert_eq!(session.connection_id(), None);
        assert_eq!(session.state(), VoiceState::Searching);
    }

    #[test]
    fn peer_mute_does_not_change_local_mute_state() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer"
        }));
        session.on_event(&serde_json::json!({"type": "stream-received"}));
        assert!(session.set_muted(true).is_some());

        session.on_event(&serde_json::json!({
            "type": "peer-mute",
            "connectionId": "peer",
            "muted": true
        }));
        assert_eq!(session.state(), VoiceState::Muted);
        assert!(session.peer_muted());
    }

    #[test]
    fn local_disconnect_returns_to_disconnected_state() {
        let mut session = VoiceSession::default();
        session.registered(Some("connection-1".into()));
        assert!(session.disconnect().is_none());
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);
    }

    #[test]
    fn local_disconnect_notifies_only_an_active_peer() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer-1"
        }));
        assert_eq!(
            session.disconnect(),
            Some(serde_json::json!({
                "type": "peer-disconnect",
                "connectionId": "peer-1"
            }))
        );
    }

    #[test]
    fn disconnect_without_peer_still_exits_searching_state() {
        let mut session = VoiceSession::default();
        session.start_search();
        assert!(session.disconnect().is_none());
        assert_eq!(session.state(), VoiceState::Disconnected);
    }

    #[test]
    fn malformed_peer_connect_does_not_enter_negotiation() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({"type":"peer-connect"}));
        assert_eq!(session.state(), VoiceState::Disconnected);
        assert_eq!(session.connection_id(), None);
    }

    #[test]
    fn peer_connection_ids_are_normalized_before_matching_follow_up_events() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "  peer-1  "
        }));
        assert_eq!(session.connection_id(), Some("peer-1"));
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "peer-1",
            "connection": true
        }));
        assert_eq!(session.state(), VoiceState::InCall);
    }

    #[test]
    fn peer_connection_event_without_explicit_boolean_enters_incall_state() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer-1"
        }));
        assert_eq!(session.state(), VoiceState::Connecting);
        // Server sends peer-connection with connectionId only (no boolean connection field)
        session.on_event(&serde_json::json!({
            "type": "peer-connection",
            "connectionId": "peer-1"
        }));
        assert_eq!(session.state(), VoiceState::InCall);
    }

    #[test]
    fn registration_clears_previous_negotiation_state() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "old",
            "initiator": true
        }));
        session.on_event(&serde_json::json!({
            "type": "ice-candidate",
            "connectionId": "old",
            "candidate": "old-candidate"
        }));
        assert_eq!(session.pending_candidate_count(), 1);
        session.registered(Some("new-registration".into()));
        assert_eq!(session.state(), VoiceState::Registered);
        assert_eq!(session.pending_candidate_count(), 0);
        assert!(!session.remote_description_set);
        assert!(!session.initiator);
    }

    #[test]
    fn pending_candidates_are_deduplicated_and_bounded() {
        let mut session = VoiceSession::default();
        session.on_event(&serde_json::json!({
            "type": "peer-connect",
            "connectionId": "peer"
        }));

        for index in 0..40 {
            session.on_event(&serde_json::json!({
                "type": "ice-candidate",
                "connectionId": "peer",
                "candidate": format!("candidate-{index}")
            }));
        }
        session.on_event(&serde_json::json!({
            "type": "ice-candidate",
            "connectionId": "peer",
            "candidate": "candidate-39"
        }));

        assert_eq!(session.pending_candidate_count(), 32);
        let candidates = session.on_event(&serde_json::json!({
            "type": "offer",
            "connectionId": "peer",
            "offer": "{}"
        }));
        assert_eq!(candidates.len(), 32);
        assert_eq!(
            candidates.first(),
            Some(&Value::String("candidate-8".into()))
        );
        assert_eq!(
            candidates.last(),
            Some(&Value::String("candidate-39".into()))
        );
    }
}
