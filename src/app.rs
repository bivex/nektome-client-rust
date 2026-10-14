use crate::{
    auth,
    persistence::Store,
    protocol::{self, SearchCriteria},
    transport::ChatTransport,
};
use serde_json::Value;
use std::{collections::HashSet, io::Write};
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

pub struct App;

struct CliState {
    current_dialog: Option<i64>,
    own_user_id: Option<i64>,
    pending_random_ids: HashSet<i64>,
    message_ids: HashSet<i64>,
    last_message_id: Option<i64>,
}

impl App {
    pub async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        println!("NektoMe Rust client — type /help for commands");
        let store = Store::new();
        let device = store.load_or_create_device()?;
        let key = auth::auth_key(&device.device_id);
        let auth_options = protocol::AuthOptions::from_env();
        let mut saved_token = store.load_token()?;
        let (notice_tx, mut notice_rx) = mpsc::channel::<String>(64);
        let mut transport: Option<ChatTransport> = None;
        let mut state = CliState {
            current_dialog: None,
            own_user_id: None,
            pending_random_ids: HashSet::new(),
            message_ids: HashSet::new(),
            last_message_id: None,
        };
        let stdin = BufReader::new(io::stdin());
        let mut lines = stdin.lines();
        loop {
            print!("> ");
            std::io::stdout().flush()?;
            tokio::select! {
                Some(raw) = notice_rx.recv() => {
                    Self::handle_notice(&raw, &store, &mut saved_token, &mut state, &transport).await?;
                    if raw == "disconnected" || raw.starts_with("transport-") {
                        transport = None;
                        state.current_dialog = None;
                        state.pending_random_ids.clear();
                        state.message_ids.clear();
                        state.last_message_id = None;
                    }
                }
                line = lines.next_line() => {
                    let Some(line) = line? else { break };
                    let command = line.trim();
                    match command {
                        "/help" => println!("/connect, /disconnect, /forget-token, /search, /stop, /refresh, /typing <on|off>, /read, /report [reason], /captcha <solution>, /leave, /send <text>, /quit"),
                        "/connect" => {
                            if let Some(client) = &transport {
                                client.shutdown();
                            }
                            transport = None;
                            match ChatTransport::connect(notice_tx.clone()).await {
                            Ok(t) => {
                                let payload = match &saved_token {
                                    Some(token) => protocol::auth_send_token_with_options(token, &key, &auth_options),
                                    None => protocol::auth_get_token_with_options(&device.device_id, &device.device_name, &key, &auth_options),
                                };
                                // Publish the transport before sending auth so a fast
                                // auth.success notice can immediately send online.track.
                                transport = Some(t);
                                let result = if let Some(client) = transport.as_ref() {
                                    client.send(protocol::socket_action(payload)).await
                                } else {
                                    Err("chat transport was lost before authentication".into())
                                };
                                match result {
                                    Ok(()) => {
                                        println!("connecting…");
                                    }
                                    Err(error) => {
                                        if let Some(client) = transport.take() {
                                            client.shutdown();
                                        }
                                        eprintln!("authentication send failed: {error}");
                                    }
                                }
                            }
                            Err(e) => eprintln!("connect failed: {e}"),
                            }
                        }
                        "/disconnect" => {
                            if let Some(t) = &transport {
                                let _ = t
                                    .send(protocol::socket_action(protocol::online_track(false)))
                                    .await;
                            }
                            if let Some(client) = &transport {
                                client.shutdown();
                            }
                            transport = None;
                            state.current_dialog = None;
                            state.pending_random_ids.clear();
                            state.message_ids.clear();
                            state.last_message_id = None;
                            println!("disconnected");
                        }
                        "/forget-token" => {
                            store.clear_token()?;
                            saved_token = None;
                            println!("saved token cleared; next connection will authenticate freshly");
                        }
                        "/search" => {
                            let criteria = SearchCriteria { my_sex:None, wish_sex:None, my_age:None, wish_age:None, include_adult:false, role_play:false, disable_voice:false };
                            let _ = Self::send(&transport, protocol::search_run(&criteria)).await?;
                        },
                        "/stop" => { let _ = Self::send(&transport, protocol::action("search.sendOut")).await?; },
                        "/refresh" => if let Some(dialog_id) = state.current_dialog {
                            let _ = Self::send(&transport, protocol::dialog_info(dialog_id)).await?;
                        } else {
                            println!("No open dialog.");
                        },
                        command if command.starts_with("/typing ") => {
                            if let Some(dialog_id) = state.current_dialog {
                                let typing = match command[8..].trim() {
                                    "on" => Some(true),
                                    "off" => Some(false),
                                    _ => None,
                                };
                                if let Some(typing) = typing {
                                    let _ = Self::send(
                                        &transport,
                                        protocol::dialog_typing(dialog_id, typing, false),
                                    )
                                    .await?;
                                } else {
                                    println!("Usage: /typing <on|off>");
                                }
                            } else {
                                println!("No open dialog.");
                            }
                        }
                        "/read" => if let (Some(dialog_id), Some(message_id)) =
                            (state.current_dialog, state.last_message_id)
                        {
                            let _ = Self::send(&transport, protocol::read_messages(dialog_id, message_id))
                                .await?;
                        } else {
                            println!("No message to acknowledge.");
                        },
                        command if command == "/report" || command.starts_with("/report ") => {
                            if let (Some(dialog_id), Some(message_id)) =
                                (state.current_dialog, state.last_message_id)
                            {
                                let reason_id = command
                                    .strip_prefix("/report")
                                    .and_then(|value| value.split_whitespace().next())
                                    .and_then(|value| value.parse().ok())
                                    .unwrap_or(1);
                                let _ = Self::send(
                                    &transport,
                                    protocol::report_dialog(dialog_id, message_id, reason_id),
                                )
                                .await?;
                            } else {
                                println!("No message to report.");
                            }
                        }
                        command if command.starts_with("/captcha ") => {
                            let solution = command[9..].trim();
                            if solution.is_empty() {
                                println!("Usage: /captcha <solution>");
                            } else {
                                let _ = Self::send(&transport, protocol::captcha_verify(solution)).await?;
                            }
                        }
                        "/leave" => {
                            if let Some(dialog_id) = state.current_dialog
                                && Self::send(&transport, protocol::leave_dialog(dialog_id)).await?
                            {
                                state.current_dialog = None;
                            }
                        },
                        "/quit" => break,
                        command if command.starts_with("/send ") => if let (Some(t), Some(dialog_id)) = (&transport, state.current_dialog) {
                            let text = command[6..].trim();
                            if text.is_empty() {
                                println!("Message must not be empty.");
                                continue;
                            }
                            let random_id = rand::random();
                            state.pending_random_ids.insert(random_id);
                            if let Err(error) = t
                                .send(protocol::socket_action(protocol::message(
                                    dialog_id, text, random_id,
                                )))
                                .await
                            {
                                state.pending_random_ids.remove(&random_id);
                                eprintln!("message send failed: {error}");
                            }
                        } else { println!("No open dialog."); },
                        _ => println!("Unknown command; type /help"),
                    }
                }
            }
        }
        if let Some(client) = &transport {
            let _ = client
                .send(protocol::socket_action(protocol::online_track(false)))
                .await;
            client.shutdown();
        }
        Ok(())
    }

    async fn send(
        transport: &Option<ChatTransport>,
        payload: Value,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(t) = transport {
            if let Err(error) = t.send(protocol::socket_action(payload)).await {
                eprintln!("Send failed: {error}");
                return Ok(false);
            }
            Ok(true)
        } else {
            println!("Not connected.");
            Ok(false)
        }
    }

    async fn handle_notice(
        raw: &str,
        store: &Store,
        saved_token: &mut Option<String>,
        state: &mut CliState,
        transport: &Option<ChatTransport>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if raw == "connected" || raw == "ready" || raw == "disconnected" {
            println!("\n[event] {raw}");
            return Ok(());
        }
        if raw.starts_with("transport-closed:") || raw.starts_with("transport-error:") {
            state.pending_random_ids.clear();
            println!("\n[event] {raw}");
            return Ok(());
        }
        let envelope: protocol::NoticeEnvelope = match serde_json::from_str(raw) {
            Ok(envelope) => envelope,
            Err(_) => {
                println!("\n[event] {raw}");
                return Ok(());
            }
        };
        let name = envelope.notice.as_deref().unwrap_or("unknown");
        let data = envelope.data.unwrap_or(Value::Null);
        match name {
            "auth.successToken" | "auth.successUser" => {
                state.own_user_id = data.get("id").and_then(Value::as_i64);
                if let Some(token) = data.pointer("/tokenInfo/authToken").and_then(Value::as_str) {
                    store.save_token(token)?;
                    *saved_token = Some(token.to_owned());
                    println!(
                        "\nAuthenticated as #{}",
                        state.own_user_id.unwrap_or_default()
                    );
                }
                if let Some(t) = transport {
                    t.send(protocol::socket_action(protocol::online_track(true)))
                        .await?;
                }
                if protocol::communication_banned(&data) {
                    println!("\nChat communication is currently banned");
                }
            }
            "dialog.opened" | "dialog.info" => {
                let closed = data.get("close").and_then(Value::as_bool) == Some(true);
                state.current_dialog = data.get("id").and_then(Value::as_i64);
                state.pending_random_ids.clear();
                state.message_ids.clear();
                state.last_message_id = None;
                let count = data
                    .get("messages")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                println!(
                    "\nDialog {} opened ({count} history messages)",
                    state.current_dialog.unwrap_or_default()
                );
                if let Some(messages) = data.get("messages").and_then(Value::as_array) {
                    for message in messages {
                        if !Self::accept_message(
                            message,
                            state.current_dialog,
                            &mut state.message_ids,
                            &mut state.last_message_id,
                        ) {
                            continue;
                        }
                        let own =
                            message.get("senderId").and_then(Value::as_i64) == state.own_user_id;
                        Self::print_message(message, own);
                    }
                }
                if closed {
                    state.current_dialog = None;
                    state.message_ids.clear();
                    state.last_message_id = None;
                    println!("Dialog is already closed");
                }
            }
            "dialog.closed" => {
                println!("\nDialog closed");
                state.current_dialog = None;
                state.pending_random_ids.clear();
                state.message_ids.clear();
                state.last_message_id = None;
            }
            "messages.new" => {
                if !Self::accept_message(
                    &data,
                    state.current_dialog,
                    &mut state.message_ids,
                    &mut state.last_message_id,
                ) {
                    return Ok(());
                }
                if state.current_dialog.is_none() {
                    state.current_dialog = data
                        .get("dialogId")
                        .and_then(Value::as_i64)
                        .filter(|dialog_id| *dialog_id != 0);
                    if state.current_dialog.is_some() {
                        println!("\nDialog opened from incoming message");
                    }
                }
                let own_by_random_id = data
                    .get("randomId")
                    .and_then(Value::as_i64)
                    .is_some_and(|random_id| state.pending_random_ids.remove(&random_id));
                let own = own_by_random_id
                    || data.get("senderId").and_then(Value::as_i64) == state.own_user_id;
                Self::print_message(&data, own);
                if let (Some(t), Some(dialog_id), Some(message_id)) =
                    (transport, state.current_dialog, state.last_message_id)
                    && !own
                {
                    t.send(protocol::socket_action(protocol::read_messages(
                        dialog_id, message_id,
                    )))
                    .await?;
                }
            }
            "search.success" => println!("\nSearching for a partner…"),
            "search.out" => println!("\nSearch stopped"),
            "dialog.typing" => {
                let typing = data.get("typing").and_then(Value::as_bool).unwrap_or(false);
                println!("\nPartner is {}typing", if typing { "" } else { "not " });
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
                println!(
                    "\nMessages read through #{}",
                    watermark.map_or_else(|| "?".into(), |id| id.to_string())
                );
            }
            "online.count" => println!(
                "\nOnline: {} in chats · {} in search",
                data.get("inChats").and_then(Value::as_i64).unwrap_or(0),
                data.get("inSearch").and_then(Value::as_i64).unwrap_or(0)
            ),
            "dialog.paid" | "purchase.changed" => println!(
                "\nAccount status changed{}",
                data.get("paidType")
                    .and_then(Value::as_i64)
                    .map_or(String::new(), |paid_type| format!(
                        " (paid type {paid_type})"
                    ))
            ),
            "socket.close" => {
                println!("\nServer closed the chat connection");
                if let Some(t) = transport {
                    let _ = t
                        .send(protocol::socket_action(protocol::online_track(false)))
                        .await;
                    t.shutdown();
                }
            }
            "captcha.verify" => {
                if let Some(solution) = data.get("solution").and_then(Value::as_str) {
                    if let Some(t) = transport {
                        t.send(protocol::socket_action(protocol::captcha_verify(solution)))
                            .await?;
                        println!("\nServer supplied a CAPTCHA solution; verifying");
                    }
                } else {
                    println!("\nCAPTCHA required; use /captcha <solution>");
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
                println!("\nError {code}: {description}{additional}");
                if code == "600"
                    || description.to_lowercase().contains("captcha")
                    || description.to_lowercase().contains("каптч")
                {
                    println!("Open the desktop UI to solve this reCAPTCHA in Chrome.");
                }
            }
            _ => println!("\n[event] {name}"),
        }
        Ok(())
    }

    fn print_message(message: &Value, own: bool) {
        let label = if own { "you" } else { "anon" };
        println!(
            "[{label}] {}",
            message.get("message").and_then(Value::as_str).unwrap_or("")
        );
    }

    fn accept_message(
        message: &Value,
        current_dialog: Option<i64>,
        message_ids: &mut HashSet<i64>,
        last_message_id: &mut Option<i64>,
    ) -> bool {
        let Some(parsed) = protocol::parse_message(message) else {
            return false;
        };
        if let (Some(current), Some(message_dialog)) = (
            current_dialog,
            (parsed.dialog_id.0 != 0).then_some(parsed.dialog_id.0),
        ) && current != message_dialog
        {
            return false;
        }
        let message_id = parsed.id.0;
        if !message_ids.insert(message_id) {
            return false;
        }
        if last_message_id.is_none_or(|last| message_id > last) {
            *last_message_id = Some(message_id);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::App;
    use serde_json::json;
    use std::collections::HashSet;

    #[test]
    fn incoming_message_with_dialog_id_is_valid_for_bootstrapping() {
        let message = json!({"id": 42, "dialogId": 7, "message": "hello"});
        let mut ids = HashSet::new();
        let mut last_id = None;

        assert!(App::accept_message(&message, None, &mut ids, &mut last_id));
        assert_eq!(
            message.get("dialogId").and_then(|value| value.as_i64()),
            Some(7)
        );
        assert_eq!(last_id, Some(42));
    }
}
