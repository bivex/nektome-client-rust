use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DialogStatus {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserId(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DialogId(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageId(pub i64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Sex {
    Female,
    Male,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgeGroup {
    pub name: String,
    pub to: Option<i32>,
    pub selected: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceConfig {
    pub available_seconds: i32,
    pub max_seconds: i32,
    pub min_seconds: i32,
    pub beta: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AppConfig {
    pub alert_info: Option<String>,
    pub ban_talk: bool,
    pub rules_enabled: bool,
    pub audio_chat_enabled: bool,
    pub subscriptions_disabled: bool,
    pub report_reasons: Vec<String>,
    pub adult_ages: Vec<AgeGroup>,
    pub communication_ages: Vec<AgeGroup>,
    pub role_ages: Vec<AgeGroup>,
    pub voice: Option<VoiceConfig>,
}

impl fmt::Display for Sex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Female => "F",
            Self::Male => "M",
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub id: MessageId,
    pub dialog_id: DialogId,
    pub sender_id: UserId,
    pub text: String,
    pub created_at: i64,
    pub random_id: i64,
    pub read: bool,
}

impl Message {
    pub fn new(
        id: i64,
        dialog_id: i64,
        sender_id: i64,
        text: String,
        created_at: i64,
        random_id: i64,
        read: bool,
    ) -> Result<Self, String> {
        if text.trim().is_empty() {
            return Err("message text must not be empty".into());
        }
        Ok(Self {
            id: MessageId(id),
            dialog_id: DialogId(dialog_id),
            sender_id: UserId(sender_id),
            text,
            created_at,
            random_id,
            read,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Dialog {
    pub id: DialogId,
    pub interlocutors: Vec<UserId>,
    pub supports_voice: bool,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub status: DialogStatus,
    pub messages: Vec<Message>,
}

impl Dialog {
    pub fn new(
        id: i64,
        interlocutors: Vec<UserId>,
        supports_voice: bool,
        created_at: Option<i64>,
        updated_at: Option<i64>,
    ) -> Self {
        Self {
            id: DialogId(id),
            interlocutors,
            supports_voice,
            created_at,
            updated_at,
            status: DialogStatus::Open,
            messages: Vec::new(),
        }
    }

    pub fn try_add_message(&mut self, message: Message) -> Result<bool, String> {
        if message.dialog_id != self.id {
            return Err(format!(
                "message dialog {} does not match dialog {}",
                message.dialog_id.0, self.id.0
            ));
        }
        if self.status == DialogStatus::Closed {
            return Ok(false);
        }
        if self.messages.iter().any(|item| item.id == message.id) {
            return Ok(false);
        }
        self.updated_at = Some(message.created_at);
        self.messages.push(message);
        Ok(true)
    }

    pub fn mark_read(&mut self, up_to: MessageId) -> Vec<MessageId> {
        let mut newly_read = Vec::new();
        for message in &mut self.messages {
            if !message.read && message.id.0 <= up_to.0 {
                message.read = true;
                newly_read.push(message.id);
            }
        }
        newly_read
    }

    pub fn close(&mut self) {
        self.status = DialogStatus::Closed;
    }
}

#[cfg(test)]
mod tests {
    use super::{Dialog, DialogStatus, Message, UserId};

    fn message(id: i64, read: bool) -> Message {
        Message::new(id, 7, 5, format!("message-{id}"), id, 0, read).unwrap()
    }

    #[test]
    fn dialog_rejects_duplicates_and_wrong_dialogs() {
        let mut dialog = Dialog::new(7, vec![UserId(5)], false, None, None);
        assert!(dialog.try_add_message(message(1, false)).unwrap());
        assert!(!dialog.try_add_message(message(1, false)).unwrap());
        let wrong = Message::new(2, 8, 5, "wrong".into(), 2, 0, false).unwrap();
        assert!(dialog.try_add_message(wrong).is_err());
        assert_eq!(dialog.messages.len(), 1);
    }

    #[test]
    fn dialog_marks_only_unread_messages_through_watermark() {
        let mut dialog = Dialog::new(7, Vec::new(), false, None, None);
        dialog.try_add_message(message(1, false)).unwrap();
        dialog.try_add_message(message(2, true)).unwrap();
        dialog.try_add_message(message(3, false)).unwrap();
        assert_eq!(dialog.mark_read(super::MessageId(2)).len(), 1);
        assert!(dialog.messages[0].read);
        assert!(!dialog.messages[2].read);
    }

    #[test]
    fn closed_dialog_does_not_accept_late_messages() {
        let mut dialog = Dialog::new(7, Vec::new(), false, None, None);
        dialog.close();
        assert_eq!(dialog.status, DialogStatus::Closed);
        assert!(!dialog.try_add_message(message(1, false)).unwrap());
    }
}
