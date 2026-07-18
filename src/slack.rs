//! Slack Socket Mode input and Web API output.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::channel::RawMessage;

const API_BASE: &str = "https://slack.com/api";
const MAX_TEXT_CHARS: usize = 4_000;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Clone)]
pub struct Slack {
    state: Arc<State>,
    receiver: Arc<ReceiverTask>,
}

struct State {
    app_token: String,
    bot_token: String,
    allow_user_ids: HashSet<String>,
    inbox: Mutex<Inbox>,
    client: Client,
    api_base: String,
    socket: AsyncMutex<Option<Socket>>,
    identity: AsyncMutex<Option<Identity>>,
    notify: Notify,
    last_error: Mutex<Option<String>>,
}

struct ReceiverTask {
    handle: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
struct Identity {
    team_id: String,
    user_id: String,
}

struct Inbox {
    connection: Connection,
    path: String,
}

#[derive(Debug)]
struct Event {
    event_id: String,
    team_id: String,
    channel: String,
    user: String,
    text: String,
    root_ts: String,
    is_group: bool,
    is_from_me: bool,
    is_supported: bool,
}

#[derive(Deserialize)]
struct SocketEnvelope {
    #[serde(rename = "type")]
    envelope_type: String,
    envelope_id: Option<String>,
    payload: Option<Value>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiResponse {
    ok: bool,
    error: Option<String>,
    url: Option<String>,
    team_id: Option<String>,
    user_id: Option<String>,
}

impl Drop for ReceiverTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.lock().unwrap().take() {
            handle.abort();
        }
    }
}

impl Slack {
    pub fn new(
        app_token: String,
        bot_token: String,
        allow_user_ids: Vec<String>,
        state_path: &str,
    ) -> Result<Self> {
        let inbox_path = format!("{state_path}.slack-inbox.db");
        Self::with_api_base(
            app_token,
            bot_token,
            allow_user_ids,
            &inbox_path,
            API_BASE.to_string(),
        )
    }

    fn with_api_base(
        app_token: String,
        bot_token: String,
        allow_user_ids: Vec<String>,
        inbox_path: &str,
        api_base: String,
    ) -> Result<Self> {
        Ok(Self {
            state: Arc::new(State {
                app_token,
                bot_token,
                allow_user_ids: allow_user_ids
                    .into_iter()
                    .map(|value| value.trim().to_string())
                    .collect(),
                inbox: Mutex::new(Inbox::open(inbox_path)?),
                client: Client::builder()
                    .timeout(Duration::from_secs(25))
                    .build()
                    .context("build Slack HTTP client")?,
                api_base,
                socket: AsyncMutex::new(None),
                identity: AsyncMutex::new(None),
                notify: Notify::new(),
                last_error: Mutex::new(None),
            }),
            receiver: Arc::new(ReceiverTask {
                handle: Mutex::new(None),
            }),
        })
    }

    pub fn allows_user(&self, user: &str) -> bool {
        self.state.allow_user_ids.contains(user)
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.start_receiver();
        loop {
            let notified = self.state.notify.notified();
            if let Some(messages) = self.pending(since)? {
                return Ok(messages);
            }
            if let Some(error) = self.state.last_error.lock().unwrap().take() {
                bail!(error);
            }
            notified.await;
        }
    }

    pub fn latest_cursor(&self) -> Result<i64> {
        self.state.inbox.lock().unwrap().latest_cursor()
    }

    pub async fn send_message(&self, target: &str, text: &str) -> Result<()> {
        let (channel, thread_ts) = self.resolve_target(target)?;
        let mut body = json!({"channel": channel, "text": text});
        if let Some(thread_ts) = thread_ts {
            body.as_object_mut()
                .expect("Slack message payload is an object")
                .insert("thread_ts".to_string(), Value::String(thread_ts));
        }
        self.state
            .api("chat.postMessage", &self.state.bot_token, body)
            .await?;
        Ok(())
    }

    pub async fn send_status(&self, target: &str) -> Result<()> {
        let Some((channel, thread_ts)) = parse_reply_target(target) else {
            return Ok(());
        };
        self.state
            .api(
                "assistant.threads.setStatus",
                &self.state.bot_token,
                json!({
                    "channel_id": channel,
                    "thread_ts": thread_ts,
                    "status": "is working…"
                }),
            )
            .await?;
        Ok(())
    }

    fn resolve_target(&self, target: &str) -> Result<(String, Option<String>)> {
        if let Some((channel, root)) = parse_reply_target(target) {
            return Ok((channel.to_string(), Some(root.to_string())));
        }
        let Some(user) = target.strip_prefix("user:") else {
            bail!("invalid Slack delivery target");
        };
        if !self.allows_user(user) {
            bail!("Slack delivery user is not allowlisted");
        }
        Ok((user.to_string(), None))
    }

    fn pending(&self, since: i64) -> Result<Option<Vec<RawMessage>>> {
        let messages = self.state.inbox.lock().unwrap().after(since)?;
        Ok((!messages.is_empty()).then_some(messages))
    }

    fn start_receiver(&self) {
        let mut handle = self.receiver.handle.lock().unwrap();
        if handle.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return;
        }
        if let Some(finished) = handle.take() {
            drop(finished);
        }
        let state = self.state.clone();
        *handle = Some(tokio::spawn(async move { receive_loop(state).await }));
    }
}

impl State {
    async fn api(&self, method: &str, token: &str, body: Value) -> Result<ApiResponse> {
        let url = format!("{}/{method}", self.api_base.trim_end_matches('/'));
        let mut attempt = 0;
        loop {
            let response = self
                .client
                .post(&url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("call Slack {method}"))?;
            if response.status() == StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                tokio::time::sleep(retry_after(response.headers())).await;
                attempt += 1;
                continue;
            }
            let status = response.status();
            let response: ApiResponse = response
                .json()
                .await
                .with_context(|| format!("decode Slack {method} response ({status})"))?;
            if !status.is_success() || !response.ok {
                bail!(
                    "Slack {method} failed: {}",
                    response.error.as_deref().unwrap_or(status.as_str())
                );
            }
            return Ok(response);
        }
    }

    async fn ensure_identity(&self) -> Result<Identity> {
        if let Some(identity) = self.identity.lock().await.clone() {
            return Ok(identity);
        }
        let response = self.api("auth.test", &self.bot_token, json!({})).await?;
        let identity = Identity {
            team_id: response
                .team_id
                .context("Slack auth.test omitted team_id")?,
            user_id: response
                .user_id
                .context("Slack auth.test omitted user_id")?,
        };
        *self.identity.lock().await = Some(identity.clone());
        Ok(identity)
    }

    async fn ensure_socket(&self) -> Result<()> {
        if self.socket.lock().await.is_some() {
            return Ok(());
        }
        self.ensure_identity().await?;
        let response = self
            .api("apps.connections.open", &self.app_token, json!({}))
            .await?;
        let url = response
            .url
            .context("Slack apps.connections.open omitted WebSocket URL")?;
        let (socket, _) = tokio_tungstenite::connect_async(&url)
            .await
            .context("connect Slack Socket Mode")?;
        *self.socket.lock().await = Some(socket);
        Ok(())
    }

    async fn receive_one(&self) -> Result<bool> {
        self.ensure_socket().await?;
        let next = {
            let mut socket = self.socket.lock().await;
            socket
                .as_mut()
                .context("Slack Socket Mode connection is unavailable")?
                .next()
                .await
        };
        match next {
            Some(Ok(Message::Text(text))) => self.handle_socket_text(&text).await,
            Some(Ok(Message::Ping(payload))) => {
                if let Some(socket) = self.socket.lock().await.as_mut() {
                    socket.send(Message::Pong(payload)).await?;
                }
                Ok(false)
            }
            Some(Ok(Message::Close(_))) | None => {
                *self.socket.lock().await = None;
                bail!("Slack Socket Mode connection closed")
            }
            Some(Ok(_)) => Ok(false),
            Some(Err(error)) => {
                *self.socket.lock().await = None;
                Err(error).context("receive Slack Socket Mode message")
            }
        }
    }

    async fn handle_socket_text(&self, text: &str) -> Result<bool> {
        let envelope: SocketEnvelope =
            serde_json::from_str(text).context("parse Slack envelope")?;
        if envelope.envelope_type == "disconnect" {
            *self.socket.lock().await = None;
            bail!(
                "Slack requested Socket Mode reconnect ({})",
                envelope.reason.as_deref().unwrap_or("unspecified")
            );
        }
        if envelope.envelope_type != "events_api" {
            return Ok(false);
        }
        let envelope_id = envelope
            .envelope_id
            .as_deref()
            .context("Slack events_api envelope omitted envelope_id")?;
        let identity = self.ensure_identity().await?;
        let inserted = if let Some(mut event) = envelope
            .payload
            .as_ref()
            .and_then(|payload| parse_event(payload, &identity))
        {
            let accepted = event.is_supported
                && !event.is_group
                && !event.is_from_me
                && self.allow_user_ids.contains(&event.user);
            if !accepted {
                event.text.clear();
            }
            self.inbox.lock().unwrap().insert(&event)?;
            true
        } else {
            false
        };
        self.ack(envelope_id).await?;
        Ok(inserted)
    }

    async fn ack(&self, envelope_id: &str) -> Result<()> {
        let ack = Message::Text(json!({"envelope_id": envelope_id}).to_string().into());
        self.socket
            .lock()
            .await
            .as_mut()
            .context("Slack Socket Mode connection closed before ACK")?
            .send(ack)
            .await
            .context("acknowledge Slack Socket Mode envelope")
    }
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds)
}

async fn receive_loop(state: Arc<State>) {
    loop {
        match state.receive_one().await {
            Ok(inserted) => {
                if inserted {
                    state.notify.notify_one();
                }
            }
            Err(error) => {
                *state.socket.lock().await = None;
                *state.last_error.lock().unwrap() = Some(format!("{error:#}"));
                state.notify.notify_one();
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

impl Inbox {
    fn open(path: &str) -> Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create Slack inbox directory {}", parent.display()))?;
        }
        let connection =
            Connection::open(path).with_context(|| format!("open Slack inbox {path}"))?;
        crate::util::restrict_permissions(Path::new(path), false)
            .with_context(|| format!("restrict Slack inbox permissions {path}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .context("configure Slack inbox busy timeout")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS slack_events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_id TEXT NOT NULL UNIQUE,
                team_id TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                text TEXT NOT NULL,
                root_ts TEXT NOT NULL,
                is_group INTEGER NOT NULL,
                is_from_me INTEGER NOT NULL,
                is_supported INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            connection,
            path: path.to_string(),
        })
    }

    fn insert(&mut self, event: &Event) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO slack_events (
                event_id, team_id, channel_id, user_id, text, root_ts,
                is_group, is_from_me, is_supported
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(event_id) DO NOTHING",
            params![
                event.event_id,
                event.team_id,
                event.channel,
                event.user,
                event.text,
                event.root_ts,
                event.is_group,
                event.is_from_me,
                event.is_supported,
            ],
        )?;
        self.connection
            .query_row(
                "SELECT id FROM slack_events WHERE event_id = ?1",
                [&event.event_id],
                |row| row.get(0),
            )
            .with_context(|| format!("read Slack event from {}", self.path))
    }

    fn latest_cursor(&self) -> Result<i64> {
        self.connection
            .query_row("SELECT MAX(id) FROM slack_events", [], |row| row.get(0))
            .optional()?
            .flatten()
            .map_or(Ok(0), Ok)
    }

    fn after(&self, since: i64) -> Result<Vec<RawMessage>> {
        let mut statement = self.connection.prepare(
            "SELECT id, event_id, team_id, channel_id, user_id, text, root_ts,
                    is_group, is_from_me, is_supported
             FROM slack_events WHERE id > ?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map([since], |row| {
                let team: String = row.get(2)?;
                let channel: String = row.get(3)?;
                let root: String = row.get(6)?;
                Ok(RawMessage {
                    row_id: row.get(0)?,
                    provider_event_id: Some(row.get(1)?),
                    channel: "slack",
                    handle: row.get(4)?,
                    chat_identifier: format!("{team}|{channel}|{root}"),
                    is_group: row.get(7)?,
                    text: row.get(5)?,
                    voice: None,
                    is_from_me: row.get(8)?,
                    is_supported: row.get(9)?,
                    thread_id: None,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("read pending Slack inbox events")?;
        Ok(rows)
    }
}

fn parse_event(payload: &Value, identity: &Identity) -> Option<Event> {
    if payload.get("type")?.as_str()? != "event_callback" {
        return None;
    }
    let event = payload.get("event")?;
    let event_id = payload.get("event_id")?.as_str()?.to_string();
    let team_id = payload.get("team_id")?.as_str()?.to_string();
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let channel_type = event
        .get("channel_type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let channel = event
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let user = event
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let text = event
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let ts = event.get("ts").and_then(Value::as_str).unwrap_or_default();
    let root_ts = event
        .get("thread_ts")
        .and_then(Value::as_str)
        .unwrap_or(ts)
        .to_string();
    let subtype = event.get("subtype").and_then(Value::as_str);
    let is_from_me = event.get("bot_id").is_some()
        || event.get("bot_profile").is_some()
        || subtype == Some("bot_message")
        || user == identity.user_id;
    let is_group = channel_type != "im";
    let is_supported = team_id == identity.team_id
        && event_type == "message"
        && subtype.is_none()
        && !channel.is_empty()
        && !user.is_empty()
        && !text.trim().is_empty()
        && !root_ts.is_empty();
    Some(Event {
        event_id,
        team_id,
        channel,
        user,
        text,
        root_ts,
        is_group,
        is_from_me,
        is_supported,
    })
}

pub fn split_text(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        if current.chars().count() == MAX_TEXT_CHARS {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

pub fn parse_message_target(value: &str) -> Option<(&str, &str, &str)> {
    let (team, rest) = value.split_once('|')?;
    let (channel, root) = rest.split_once('|')?;
    (!team.is_empty() && !channel.is_empty() && !root.is_empty()).then_some((team, channel, root))
}

fn parse_reply_target(value: &str) -> Option<(&str, &str)> {
    let (channel, root) = value.split_once('|')?;
    (!channel.is_empty() && !root.is_empty()).then_some((channel, root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_path;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn identity() -> Identity {
        Identity {
            team_id: "T1".to_string(),
            user_id: "UBOT".to_string(),
        }
    }

    fn payload(event: Value) -> Value {
        json!({
            "type": "event_callback",
            "team_id": "T1",
            "event_id": "Ev1",
            "event": event
        })
    }

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 2048];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            let text = String::from_utf8_lossy(&bytes);
            let Some((headers, body)) = text.split_once("\r\n\r\n") else {
                continue;
            };
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if body.len() >= length {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn write_json_response(stream: &mut TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[test]
    fn parses_only_plain_workspace_dm_messages() {
        let accepted = parse_event(
            &payload(json!({
                "type": "message", "channel_type": "im", "channel": "D1",
                "user": "U1", "text": "hello", "ts": "1.2"
            })),
            &identity(),
        )
        .unwrap();
        assert!(accepted.is_supported);
        assert!(!accepted.is_group);
        assert!(!accepted.is_from_me);

        for event in [
            json!({"type":"message","channel_type":"channel","channel":"C1","user":"U1","text":"no","ts":"1"}),
            json!({"type":"message","channel_type":"mpim","channel":"G1","user":"U1","text":"no","ts":"1"}),
            json!({"type":"message","channel_type":"im","channel":"D1","user":"UBOT","text":"no","ts":"1"}),
            json!({"type":"message","channel_type":"im","channel":"D1","user":"U1","text":"no","ts":"1","subtype":"bot_message","bot_id":"B1"}),
            json!({"type":"message","channel_type":"im","channel":"D1","user":"U1","text":"no","ts":"1","subtype":"message_changed"}),
        ] {
            let parsed = parse_event(&payload(event), &identity()).unwrap();
            assert!(parsed.is_group || parsed.is_from_me || !parsed.is_supported);
        }
    }

    #[test]
    fn inbox_deduplicates_event_ids_and_recovers_rows() {
        let path = temp_path("slack-inbox");
        let mut inbox = Inbox::open(path.to_str().unwrap()).unwrap();
        let event = parse_event(
            &payload(json!({
                "type": "message", "channel_type": "im", "channel": "D1",
                "user": "U1", "text": "hello", "ts": "1.2"
            })),
            &identity(),
        )
        .unwrap();
        assert_eq!(inbox.insert(&event).unwrap(), 1);
        assert_eq!(inbox.insert(&event).unwrap(), 1);
        drop(inbox);

        let inbox = Inbox::open(path.to_str().unwrap()).unwrap();
        let rows = inbox.after(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id(), "slack:Ev1");
        assert_eq!(inbox.latest_cursor().unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn chunks_unicode_without_splitting_characters() {
        let text = "🦀".repeat(MAX_TEXT_CHARS + 1);
        let chunks = split_text(&text);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].chars().count(), MAX_TEXT_CHARS);
        assert_eq!(chunks[1], "🦀");
    }

    #[tokio::test]
    async fn socket_mode_persists_before_ack_and_deduplicates_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let envelope = json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": payload(json!({
                "type": "message", "channel_type": "im", "channel": "D1",
                "user": "U1", "text": "hello", "ts": "1.2"
            }))
        })
        .to_string();
        let retry = envelope.replace("env-1", "env-2");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::Text(envelope.into())).await.unwrap();
            let first = socket.next().await.unwrap().unwrap().into_text().unwrap();
            socket.send(Message::Text(retry.into())).await.unwrap();
            let second = socket.next().await.unwrap().unwrap().into_text().unwrap();
            (first, second)
        });

        let path = temp_path("slack-socket-inbox");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-test".to_string(),
            vec!["U1".to_string()],
            path.to_str().unwrap(),
            "http://unused".to_string(),
        )
        .unwrap();
        *slack.state.identity.lock().await = Some(identity());
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        *slack.state.socket.lock().await = Some(socket);

        let first = slack.poll(0).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].event_id(), "slack:Ev1");
        assert!(slack.poll(1).await.is_err());
        let (first_ack, second_ack) = server.await.unwrap();
        assert_eq!(first_ack, r#"{"envelope_id":"env-1"}"#);
        assert_eq!(second_ack, r#"{"envelope_id":"env-2"}"#);
        assert_eq!(
            slack.state.inbox.lock().unwrap().latest_cursor().unwrap(),
            1
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn receiver_acks_next_envelope_while_gateway_processing_is_stalled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let first = json!({
            "type": "events_api", "envelope_id": "env-1",
            "payload": payload(json!({
                "type": "message", "channel_type": "im", "channel": "D1",
                "user": "U1", "text": "first", "ts": "1.1"
            }))
        })
        .to_string();
        let second = json!({
            "type": "events_api", "envelope_id": "env-2",
            "payload": {
                "type": "event_callback", "team_id": "T1", "event_id": "Ev2",
                "event": {
                    "type": "message", "channel_type": "im", "channel": "D1",
                    "user": "U1", "text": "second", "ts": "1.2"
                }
            }
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.send(Message::Text(first.into())).await.unwrap();
            let _ = socket.next().await.unwrap().unwrap();
            let sent = tokio::time::Instant::now();
            socket.send(Message::Text(second.into())).await.unwrap();
            let ack = socket.next().await.unwrap().unwrap().into_text().unwrap();
            (sent.elapsed(), ack)
        });

        let path = temp_path("slack-independent-receiver");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-test".to_string(),
            vec!["U1".to_string()],
            path.to_str().unwrap(),
            "http://unused".to_string(),
        )
        .unwrap();
        *slack.state.identity.lock().await = Some(identity());
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        *slack.state.socket.lock().await = Some(socket);

        let rows = slack.poll(0).await.unwrap();
        assert_eq!(rows.len(), 1);
        tokio::time::sleep(Duration::from_millis(3_200)).await;
        let later = slack.poll(1).await.unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].event_id(), "slack:Ev2");
        let (ack_delay, ack) = server.await.unwrap();
        assert!(ack_delay < Duration::from_secs(1));
        assert_eq!(ack, r#"{"envelope_id":"env-2"}"#);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn production_lifecycle_authenticates_reconnects_and_redacts_rejections() {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = http.local_addr().unwrap();
        let websocket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let websocket_address = websocket.local_addr().unwrap();

        let http_server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for body in [
                r#"{"ok":true,"team_id":"T1","user_id":"UBOT"}"#.to_string(),
                format!(r#"{{"ok":true,"url":"ws://{websocket_address}"}}"#),
                format!(r#"{{"ok":true,"url":"ws://{websocket_address}"}}"#),
            ] {
                let (mut stream, _) = http.accept().await.unwrap();
                requests.push(read_http_request(&mut stream).await);
                write_json_response(&mut stream, &body).await;
            }
            requests
        });

        let websocket_server = tokio::spawn(async move {
            let (first, _) = websocket.accept().await.unwrap();
            let mut first = tokio_tungstenite::accept_async(first).await.unwrap();
            first
                .send(Message::Text(
                    json!({"type":"disconnect","reason":"refresh_requested"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            drop(first);

            let (second, _) = websocket.accept().await.unwrap();
            let mut second = tokio_tungstenite::accept_async(second).await.unwrap();
            let events = [
                ("env-unauthorized", "EvU", "U2", "im", None),
                ("env-group", "EvG", "U1", "mpim", None),
                ("env-bot", "EvB", "U1", "im", Some("bot_message")),
                ("env-valid", "EvV", "U1", "im", None),
            ];
            let mut acknowledgements = Vec::new();
            for (envelope_id, event_id, user, channel_type, subtype) in events {
                let mut event = json!({
                    "type":"message", "channel_type":channel_type, "channel":"D1",
                    "user":user, "text":"message", "ts":"1.2"
                });
                if let Some(subtype) = subtype {
                    event["subtype"] = Value::String(subtype.to_string());
                    event["bot_id"] = Value::String("B1".to_string());
                }
                let envelope = json!({
                    "type":"events_api", "envelope_id":envelope_id,
                    "payload": {
                        "type":"event_callback", "team_id":"T1", "event_id":event_id,
                        "event":event
                    }
                });
                second
                    .send(Message::Text(envelope.to_string().into()))
                    .await
                    .unwrap();
                acknowledgements.push(
                    second
                        .next()
                        .await
                        .unwrap()
                        .unwrap()
                        .into_text()
                        .unwrap()
                        .to_string(),
                );
            }
            acknowledgements
        });

        let path = temp_path("slack-production-lifecycle");
        let slack = Slack::with_api_base(
            "xapp-secret".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            path.to_str().unwrap(),
            format!("http://{http_address}"),
        )
        .unwrap();

        let rows = loop {
            match slack.poll(0).await {
                Ok(rows) if rows.len() == 4 => break rows,
                Ok(_) => tokio::task::yield_now().await,
                Err(_) => continue,
            }
        };
        assert_eq!(rows.len(), 4);
        let channel = crate::channel::Channel::Slack(slack.clone());
        for row in &rows[..3] {
            assert!(row.text.is_empty());
            assert!(channel.accept(row).is_none());
        }
        assert_eq!(rows[3].event_id(), "slack:EvV");
        assert_eq!(rows[3].handle, "U1");
        assert_eq!(rows[3].text, "message");
        assert!(channel.accept(&rows[3]).is_some());

        let requests = http_server.await.unwrap();
        assert!(requests[0].starts_with("POST /auth.test HTTP/1.1"));
        assert!(requests[0].contains("authorization: Bearer xoxb-secret"));
        assert!(requests[1].starts_with("POST /apps.connections.open HTTP/1.1"));
        assert!(requests[1].contains("authorization: Bearer xapp-secret"));
        assert!(requests[2].starts_with("POST /apps.connections.open HTTP/1.1"));
        let acknowledgements = websocket_server.await.unwrap();
        assert_eq!(acknowledgements.len(), 4);
        assert!(acknowledgements[0].contains("env-unauthorized"));
        assert!(acknowledgements[1].contains("env-group"));
        assert!(acknowledgements[2].contains("env-bot"));
        assert!(acknowledgements[3].contains("env-valid"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn web_api_posts_reply_and_progress_to_originating_thread() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 2048];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&bytes);
                    let Some((headers, body)) = text.split_once("\r\n\r\n") else {
                        continue;
                    };
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
                requests.push(String::from_utf8(bytes).unwrap());
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}",
                    )
                    .await
                    .unwrap();
            }
            requests
        });

        let path = temp_path("slack-http-inbox");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            path.to_str().unwrap(),
            format!("http://{address}"),
        )
        .unwrap();
        slack.send_status("D1|1.2").await.unwrap();
        slack.send_message("D1|1.2", "reply").await.unwrap();

        let requests = server.await.unwrap();
        assert!(requests[0].starts_with("POST /assistant.threads.setStatus HTTP/1.1"));
        assert!(requests[0].contains("authorization: Bearer xoxb-secret"));
        assert!(
            requests[0].contains(r#"{"channel_id":"D1","status":"is working…","thread_ts":"1.2"}"#)
        );
        assert!(requests[1].starts_with("POST /chat.postMessage HTTP/1.1"));
        assert!(requests[1].contains(r#"{"channel":"D1","text":"reply","thread_ts":"1.2"}"#));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retry_after_preserves_delays_above_the_previous_gateway_timeout() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("31"),
        );

        assert_eq!(retry_after(&headers), Duration::from_secs(31));
    }

    #[tokio::test]
    async fn web_api_waits_for_retry_after_before_retrying_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for response in [
                "HTTP/1.1 429 Too Many Requests\r\nretry-after: 1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"ok\":true}",
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).await.unwrap();
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let path = temp_path("slack-rate-limit-inbox");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            path.to_str().unwrap(),
            format!("http://{address}"),
        )
        .unwrap();
        let started = tokio::time::Instant::now();

        slack.send_message("D1|1.2", "reply").await.unwrap();

        assert!(started.elapsed() >= Duration::from_secs(1));
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn receiver_notification_is_retained_until_poll_can_wait() {
        let notify = Notify::new();
        notify.notify_one();

        tokio::time::timeout(Duration::from_millis(50), notify.notified())
            .await
            .expect("notify_one stores a permit for the next waiter");
    }
}
