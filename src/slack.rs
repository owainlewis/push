//! Slack Socket Mode input and Web API output.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode, Url};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::channel::{InboundImage, RawMessage};
use crate::image::{DownloadedImage, MAX_IMAGE_BYTES};

const API_BASE: &str = "https://slack.com/api";
pub(crate) const MAX_TEXT_CHARS: usize = 4_000;

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
    files: Vec<SlackFile>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
struct SlackFile {
    id: String,
    size: Option<usize>,
    mimetype: Option<String>,
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
    file: Option<ApiFile>,
}

#[derive(Deserialize)]
struct ApiFile {
    id: String,
    size: Option<usize>,
    mimetype: Option<String>,
    url_private: Option<String>,
    url_private_download: Option<String>,
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
        inbox_path: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::with_api_base(
            app_token,
            bot_token,
            allow_user_ids,
            inbox_path.as_ref(),
            API_BASE.to_string(),
        )
    }

    pub(crate) fn with_api_base(
        app_token: String,
        bot_token: String,
        allow_user_ids: Vec<String>,
        inbox_path: impl AsRef<Path>,
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
                    .redirect(reqwest::redirect::Policy::none())
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

    pub async fn download_image(&self, image: &InboundImage) -> Result<DownloadedImage> {
        if let Some(bytes) = &image.data {
            return Ok(DownloadedImage {
                bytes: bytes.clone(),
            });
        }
        let response = self
            .state
            .api(
                "files.info",
                &self.state.bot_token,
                json!({"file": image.locator}),
            )
            .await?;
        let file = response.file.context("Slack files.info omitted file")?;
        if file.id != image.locator {
            bail!("Slack files.info returned a different file");
        }
        if !matches!(
            file.mimetype.as_deref(),
            Some("image/jpeg" | "image/png" | "image/webp")
        ) {
            bail!("Slack file is not a supported JPEG, PNG, or WebP image");
        }
        if file.size.is_some_and(|size| size > MAX_IMAGE_BYTES) {
            bail!("Slack image exceeds the 6 MiB limit");
        }
        let private_url = file
            .url_private_download
            .or(file.url_private)
            .context("Slack files.info omitted a private download URL")?;
        let mut private_url = validated_private_url(&self.state.api_base, &private_url)?;
        let mut redirects = 0;
        let mut response = loop {
            let response = self
                .state
                .client
                .get(private_url.clone())
                .bearer_auth(&self.state.bot_token)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("download Slack image failed"))?;
            if !response.status().is_redirection() {
                break response;
            }
            if redirects == 3 {
                bail!("Slack image download exceeded the redirect limit");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .context("Slack image redirect omitted a destination")?
                .to_str()
                .map_err(|_| anyhow::anyhow!("Slack image redirect was invalid"))?;
            let next = private_url
                .join(location)
                .map_err(|_| anyhow::anyhow!("Slack image redirect was invalid"))?;
            private_url = validated_private_url(&self.state.api_base, next.as_str())?;
            redirects += 1;
        };
        if !response.status().is_success() {
            bail!(
                "Slack image download failed with status {}",
                response.status()
            );
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
        {
            bail!("Slack image exceeds the 6 MiB limit");
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("read Slack image download failed"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_IMAGE_BYTES {
                bail!("Slack image exceeds the 6 MiB limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(DownloadedImage { bytes })
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
                event.files.clear();
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

fn validated_private_url(api_base: &str, value: &str) -> Result<Url> {
    let url = Url::parse(value).context("Slack returned an invalid private download URL")?;
    let base = Url::parse(api_base).context("invalid Slack API base URL")?;
    let host = url
        .host_str()
        .context("Slack private download URL omitted a host")?;
    let is_production = base.host_str() == Some("slack.com") && base.scheme() == "https";
    let allowed = if is_production {
        url.scheme() == "https"
            && (host == "slack.com"
                || host.ends_with(".slack.com")
                || host == "slack-files.com"
                || host.ends_with(".slack-files.com"))
    } else {
        url.scheme() == base.scheme()
            && url.host_str() == base.host_str()
            && url.port_or_known_default() == base.port_or_known_default()
    };
    if !allowed {
        bail!("Slack returned a private download URL outside its trusted origin");
    }
    Ok(url)
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
    fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create Slack inbox directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open Slack inbox {}", path.display()))?;
        crate::util::restrict_permissions(path, false)
            .with_context(|| format!("restrict Slack inbox permissions {}", path.display()))?;
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
                is_supported INTEGER NOT NULL,
                files_json TEXT NOT NULL DEFAULT '[]'
            );",
        )?;
        let has_files_json = connection
            .prepare("PRAGMA table_info(slack_events)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .any(|column| column == "files_json");
        if !has_files_json {
            connection.execute(
                "ALTER TABLE slack_events ADD COLUMN files_json TEXT NOT NULL DEFAULT '[]'",
                [],
            )?;
        }
        Ok(Self {
            connection,
            path: path.to_string_lossy().to_string(),
        })
    }

    fn insert(&mut self, event: &Event) -> Result<i64> {
        let files_json =
            serde_json::to_string(&event.files).context("encode Slack file metadata")?;
        self.connection.execute(
            "INSERT INTO slack_events (
                event_id, team_id, channel_id, user_id, text, root_ts,
                is_group, is_from_me, is_supported, files_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
                files_json,
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
                    is_group, is_from_me, is_supported, files_json
             FROM slack_events WHERE id > ?1 ORDER BY id",
        )?;
        let rows = statement
            .query_map([since], |row| {
                let team: String = row.get(2)?;
                let channel: String = row.get(3)?;
                let root: String = row.get(6)?;
                let files_json: String = row.get(10)?;
                let files: Vec<SlackFile> = serde_json::from_str(&files_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        10,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(RawMessage {
                    row_id: row.get(0)?,
                    provider_event_id: Some(row.get(1)?),
                    channel: "slack",
                    handle: row.get(4)?,
                    chat_identifier: format!("{team}|{channel}|{root}"),
                    is_group: row.get(7)?,
                    text: row.get(5)?,
                    voice: None,
                    images: files
                        .into_iter()
                        .map(|file| InboundImage {
                            locator: file.id,
                            file_size: file.size,
                            mime_type: file.mimetype,
                            data: None,
                        })
                        .collect(),
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
    let files = event
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let id = file.get("id")?.as_str()?.to_string();
            (!id.is_empty()).then(|| SlackFile {
                id,
                size: file
                    .get("size")
                    .and_then(Value::as_u64)
                    .and_then(|size| usize::try_from(size).ok()),
                mimetype: file
                    .get("mimetype")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect::<Vec<_>>();
    let is_from_me = event.get("bot_id").is_some()
        || event.get("bot_profile").is_some()
        || subtype == Some("bot_message")
        || user == identity.user_id;
    let is_group = channel_type != "im";
    let is_supported = team_id == identity.team_id
        && event_type == "message"
        && (subtype.is_none() || subtype == Some("file_share"))
        && !channel.is_empty()
        && !user.is_empty()
        && (!text.trim().is_empty() || !files.is_empty())
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
        files,
    })
}

pub fn split_text(text: &str) -> Vec<String> {
    use std::borrow::Cow;
    use std::collections::VecDeque;

    #[derive(Clone, Default)]
    struct Formatting {
        fenced_code: bool,
        inline_code: bool,
        styles: Vec<char>,
        quote_prefix: String,
    }

    impl Formatting {
        fn apply(&mut self, token: &str) {
            if token == "```" && !self.inline_code {
                self.fenced_code = !self.fenced_code;
            } else if token == "`" && !self.fenced_code {
                self.inline_code = !self.inline_code;
            } else if token.len() == 1 && !self.fenced_code && !self.inline_code {
                let style = token.chars().next().unwrap();
                if self.styles.last() == Some(&style) {
                    self.styles.pop();
                } else {
                    self.styles.push(style);
                }
            }
        }

        fn closing(&self) -> String {
            if self.fenced_code {
                "```".to_string()
            } else if self.inline_code {
                "`".to_string()
            } else {
                self.styles.iter().rev().collect()
            }
        }

        fn opening(&self) -> String {
            let mut opening = self.quote_prefix.clone();
            if self.fenced_code {
                opening.push_str("```");
            } else if self.inline_code {
                opening.push('`');
            } else {
                opening.extend(&self.styles);
            }
            opening
        }
    }

    fn readable_link(token: &str) -> String {
        let inner = token
            .strip_prefix('<')
            .and_then(|value| value.strip_suffix('>'))
            .unwrap_or(token);
        inner
            .split_once('|')
            .map(|(url, label)| format!("{label} ({url})"))
            .unwrap_or_else(|| inner.to_string())
    }

    fn strip_semantic_markers(text: &str) -> String {
        let marker = crate::markdown::SLACK_FORMAT_MARKER.to_string();
        ["```", "`", "*", "_", "~"]
            .into_iter()
            .fold(text.to_string(), |value, delimiter| {
                value.replace(&format!("{marker}{delimiter}"), delimiter)
            })
    }

    let mut tokens = VecDeque::new();
    let mut offset = 0;
    let mut line_start = true;
    while offset < text.len() {
        let rest = &text[offset..];
        if line_start && rest.starts_with("> ") {
            let mut prefix_len = 0;
            while rest[prefix_len..].starts_with("> ") {
                prefix_len += 2;
            }
            let prefix = if prefix_len >= MAX_TEXT_CHARS {
                "> "
            } else {
                &rest[..prefix_len]
            };
            tokens.push_back(prefix.to_string());
            offset += prefix_len;
            line_start = false;
            continue;
        }
        if rest.starts_with(crate::markdown::SLACK_FORMAT_MARKER) {
            let marker_len = crate::markdown::SLACK_FORMAT_MARKER.len_utf8();
            let marked = &rest[marker_len..];
            let delimiter = if marked.starts_with("```") {
                Some("```")
            } else {
                marked
                    .get(..1)
                    .filter(|value| matches!(*value, "`" | "*" | "_" | "~"))
            };
            if let Some(delimiter) = delimiter {
                tokens.push_back(format!(
                    "{}{delimiter}",
                    crate::markdown::SLACK_FORMAT_MARKER
                ));
                offset += marker_len + delimiter.len();
            } else {
                tokens.push_back(crate::markdown::SLACK_FORMAT_MARKER.to_string());
                offset += marker_len;
            };
            line_start = false;
            continue;
        }
        if rest.starts_with('<') {
            if let Some(end) = rest.find('>') {
                tokens.push_back(rest[..=end].to_string());
                offset += end + 1;
                line_start = false;
                continue;
            }
        }
        if let Some(entity) = ["&amp;", "&lt;", "&gt;"]
            .into_iter()
            .find(|entity| rest.starts_with(entity))
        {
            tokens.push_back(entity.to_string());
            offset += entity.len();
            line_start = false;
            continue;
        }
        let character = rest.chars().next().unwrap();
        let character_len = character.len_utf8();
        tokens.push_back(character.to_string());
        offset += character_len;
        line_start = character == '\n';
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0;
    let mut current_has_content = false;
    let mut formatting = Formatting::default();
    while let Some(token) = tokens.pop_front() {
        let delimiter = token
            .strip_prefix(crate::markdown::SLACK_FORMAT_MARKER)
            .filter(|value| matches!(*value, "```" | "`" | "*" | "_" | "~"));
        let rendered = if let Some(delimiter) = delimiter {
            Cow::Borrowed(delimiter)
        } else if token.starts_with('<') && token.contains(crate::markdown::SLACK_FORMAT_MARKER) {
            Cow::Owned(strip_semantic_markers(&token))
        } else {
            Cow::Borrowed(token.as_str())
        };
        let is_delimiter = delimiter.is_some();
        let is_quote_prefix = rendered.starts_with("> ")
            && rendered
                .as_bytes()
                .chunks_exact(2)
                .all(|pair| pair == b"> ");
        let mut after = formatting.clone();
        if is_quote_prefix {
            after.quote_prefix = rendered.to_string();
        } else if rendered == "\n" {
            after.quote_prefix.clear();
        } else if is_delimiter {
            after.apply(&rendered);
        }
        let closing = after.closing();
        let token_chars = rendered.chars().count();
        if current_chars + token_chars + closing.chars().count() > MAX_TEXT_CHARS {
            if !current_has_content && rendered.starts_with('<') && rendered.ends_with('>') {
                for character in readable_link(&rendered).chars().rev() {
                    tokens.push_front(character.to_string());
                }
                continue;
            }
            if !current_has_content {
                if formatting.quote_prefix.chars().count() > 2 {
                    formatting.quote_prefix = "> ".to_string();
                } else {
                    formatting = Formatting::default();
                }
                current = formatting.opening();
                current_chars = current.chars().count();
                tokens.push_front(token);
                continue;
            }
            current.push_str(&formatting.closing());
            chunks.push(std::mem::take(&mut current));
            let opening = formatting.opening();
            current_chars = opening.chars().count();
            current.push_str(&opening);
            current_has_content = false;
            tokens.push_front(token);
            continue;
        }
        current.push_str(&rendered);
        current_chars += token_chars;
        current_has_content |= !is_delimiter && !is_quote_prefix;
        formatting = after;
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
    fn parses_text_and_file_share_workspace_dm_messages() {
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
        assert!(accepted.files.is_empty());

        let image_only = parse_event(
            &payload(json!({
                "type": "message", "subtype": "file_share", "channel_type": "im",
                "channel": "D1", "user": "U1", "text": "", "ts": "1.3",
                "files": [{
                    "id": "F1", "size": 12, "mimetype": "image/png",
                    "url_private": "https://files.slack.com/private-secret"
                }]
            })),
            &identity(),
        )
        .unwrap();
        assert!(image_only.is_supported);
        assert_eq!(
            image_only.files,
            vec![SlackFile {
                id: "F1".to_string(),
                size: Some(12),
                mimetype: Some("image/png".to_string()),
            }]
        );

        let text_and_images = parse_event(
            &payload(json!({
                "type": "message", "channel_type": "im", "channel": "D1",
                "user": "U1", "text": "compare", "ts": "1.4",
                "files": [
                    {"id": "F1", "size": 12, "mimetype": "image/png"},
                    {"id": "F2", "size": 20, "mimetype": "image/jpeg"}
                ]
            })),
            &identity(),
        )
        .unwrap();
        assert!(text_and_images.is_supported);
        assert_eq!(text_and_images.files.len(), 2);

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
                "user": "U1", "text": "hello", "ts": "1.2",
                "files": [{
                    "id": "F1", "size": 12, "mimetype": "image/png",
                    "url_private_download": "https://files.slack.com/private-secret"
                }]
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
        assert_eq!(rows[0].images.len(), 1);
        assert_eq!(rows[0].images[0].locator, "F1");
        assert_eq!(rows[0].images[0].file_size, Some(12));
        assert_eq!(rows[0].images[0].mime_type.as_deref(), Some("image/png"));
        assert!(rows[0].images[0].data.is_none());
        let files_json: String = inbox
            .connection
            .query_row("SELECT files_json FROM slack_events", [], |row| row.get(0))
            .unwrap();
        assert!(!files_json.contains("private"));
        assert_eq!(inbox.latest_cursor().unwrap(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn inbox_migrates_legacy_rows_without_losing_queued_events() {
        let path = temp_path("slack-legacy-inbox");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE slack_events (
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
                );
                INSERT INTO slack_events (
                    event_id, team_id, channel_id, user_id, text, root_ts,
                    is_group, is_from_me, is_supported
                ) VALUES ('EvLegacy', 'T1', 'D1', 'U1', 'queued', '1.2', 0, 0, 1);",
            )
            .unwrap();
        drop(connection);

        let inbox = Inbox::open(&path).unwrap();
        let rows = inbox.after(0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].event_id(), "slack:EvLegacy");
        assert_eq!(rows[0].text, "queued");
        assert!(rows[0].images.is_empty());
        let columns = inbox
            .connection
            .prepare("PRAGMA table_info(slack_events)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "files_json"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn downloads_private_images_with_the_bot_token_after_metadata_resolution() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let image = b"\x89PNG\r\n\x1a\nbody";
        let server = tokio::spawn(async move {
            let (mut info_stream, _) = listener.accept().await.unwrap();
            let info_request = read_http_request(&mut info_stream).await;
            write_json_response(
                &mut info_stream,
                &format!(
                    r#"{{"ok":true,"file":{{"id":"F1","size":{},"mimetype":"image/png","url_private_download":"http://{address}/private/F1"}}}}"#,
                    image.len()
                ),
            )
            .await;

            let (mut redirect_stream, _) = listener.accept().await.unwrap();
            let redirect_request = read_http_request(&mut redirect_stream).await;
            let redirect = format!(
                "HTTP/1.1 302 Found\r\nlocation: http://{address}/files-origin/F1\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
            );
            redirect_stream
                .write_all(redirect.as_bytes())
                .await
                .unwrap();

            let (mut download_stream, _) = listener.accept().await.unwrap();
            let download_request = read_http_request(&mut download_stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                image.len()
            );
            download_stream
                .write_all(response.as_bytes())
                .await
                .unwrap();
            download_stream.write_all(image).await.unwrap();
            (info_request, redirect_request, download_request)
        });
        let path = temp_path("slack-image-download");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            &path,
            format!("http://{address}"),
        )
        .unwrap();

        let downloaded = slack
            .download_image(&InboundImage {
                locator: "F1".to_string(),
                file_size: Some(image.len()),
                mime_type: Some("image/png".to_string()),
                data: None,
            })
            .await
            .unwrap();

        assert_eq!(downloaded.bytes, image);
        let (info_request, redirect_request, download_request) = server.await.unwrap();
        assert!(info_request.starts_with("POST /files.info HTTP/1.1"));
        assert!(info_request.contains("authorization: Bearer xoxb-secret"));
        assert!(info_request.contains(r#"{"file":"F1"}"#));
        assert!(redirect_request.starts_with("GET /private/F1 HTTP/1.1"));
        assert!(redirect_request.contains("authorization: Bearer xoxb-secret"));
        assert!(download_request.starts_with("GET /files-origin/F1 HTTP/1.1"));
        assert!(download_request.contains("authorization: Bearer xoxb-secret"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn rejects_untrusted_private_urls_without_exposing_urls_or_tokens() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut stream).await;
            write_json_response(
                &mut stream,
                r#"{"ok":true,"file":{"id":"F1","size":12,"mimetype":"image/png","url_private_download":"https://evil.example/private-secret"}}"#,
            )
            .await;
        });
        let path = temp_path("slack-untrusted-image");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            &path,
            format!("http://{address}"),
        )
        .unwrap();

        let error = slack
            .download_image(&InboundImage {
                locator: "F1".to_string(),
                file_size: Some(12),
                mime_type: Some("image/png".to_string()),
                data: None,
            })
            .await
            .unwrap_err();

        let detail = format!("{error:#}");
        assert!(!detail.contains("evil.example"));
        assert!(!detail.contains("private-secret"));
        assert!(!detail.contains("xoxb-secret"));
        server.await.unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn rejects_untrusted_redirects_before_forwarding_the_bot_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut info_stream, _) = listener.accept().await.unwrap();
            let _ = read_http_request(&mut info_stream).await;
            write_json_response(
                &mut info_stream,
                &format!(
                    r#"{{"ok":true,"file":{{"id":"F1","size":12,"mimetype":"image/png","url_private_download":"http://{address}/private/F1"}}}}"#
                ),
            )
            .await;

            let (mut redirect_stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut redirect_stream).await;
            assert!(request.contains("authorization: Bearer xoxb-secret"));
            redirect_stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nlocation: https://evil.example/private-secret\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let path = temp_path("slack-untrusted-redirect");
        let slack = Slack::with_api_base(
            "xapp-test".to_string(),
            "xoxb-secret".to_string(),
            vec!["U1".to_string()],
            &path,
            format!("http://{address}"),
        )
        .unwrap();

        let error = slack
            .download_image(&InboundImage {
                locator: "F1".to_string(),
                file_size: Some(12),
                mime_type: Some("image/png".to_string()),
                data: None,
            })
            .await
            .unwrap_err();

        let detail = format!("{error:#}");
        assert!(!detail.contains("evil.example"));
        assert!(!detail.contains("private-secret"));
        assert!(!detail.contains("xoxb-secret"));
        server.await.unwrap();
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

    #[test]
    fn chunks_slack_escape_entities_atomically() {
        for entity in ["&amp;", "&lt;", "&gt;"] {
            let text = format!("{}{entity}", "x".repeat(MAX_TEXT_CHARS - 1));
            let chunks = split_text(&text);

            assert_eq!(
                chunks,
                vec!["x".repeat(MAX_TEXT_CHARS - 1), entity.to_string()]
            );
            assert!(chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        }
    }

    #[test]
    fn chunks_long_blockquotes_with_a_prefix_in_every_message() {
        let markdown = format!("> {}", "x".repeat(MAX_TEXT_CHARS));
        let text = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let chunks = split_text(&text);

        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.starts_with("> ")));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.matches('x').count())
                .sum::<usize>(),
            MAX_TEXT_CHARS
        );

        let nested = format!("> > {}", "x".repeat(MAX_TEXT_CHARS));
        let chunks = split_text(&nested);
        assert!(chunks.iter().all(|chunk| chunk.starts_with("> > ")));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
    }

    #[test]
    fn oversized_quote_prefixes_degrade_without_stalling() {
        for depth in [MAX_TEXT_CHARS / 2, MAX_TEXT_CHARS / 2 + 1] {
            let text = format!("{}content", "> ".repeat(depth));
            let chunks = split_text(&text);

            assert_eq!(chunks, vec!["> content"]);
            assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
            assert!(chunks
                .iter()
                .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        }
    }

    #[test]
    fn chunks_are_independently_valid_mrkdwn() {
        let markdown = format!("**{}**", "x".repeat(MAX_TEXT_CHARS));
        let text = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let chunks = split_text(&text);

        assert_eq!(chunks.len(), 2);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.starts_with('*') && chunk.ends_with('*')));

        let markdown = format!("```\n{}\n```", "x".repeat(MAX_TEXT_CHARS));
        let code = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let chunks = split_text(&code);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.starts_with("```") && chunk.ends_with("```")));
    }

    #[test]
    fn oversized_links_degrade_to_safe_bounded_text() {
        let link = format!("<https://example.com/{}|label>", "x".repeat(MAX_TEXT_CHARS));
        let chunks = split_text(&link);

        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert!(!chunks.iter().any(|chunk| chunk.contains("<https://")));
        assert!(chunks.concat().starts_with("label (https://example.com/"));

        let markdown = format!("**[label](https://example.com/{})**", "x".repeat(3_980));
        let bold_link = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let chunks = split_text(&bold_link);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert!(chunks
            .iter()
            .all(|chunk| chunk.starts_with('*') && chunk.ends_with('*')));
    }

    #[test]
    fn literal_markers_do_not_change_chunk_formatting_state() {
        let markdown = format!(r"\*literal {}", "x".repeat(MAX_TEXT_CHARS));
        let formatted = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let clean = crate::markdown::to_slack_mrkdwn(&markdown);
        let chunks = split_text(&formatted);

        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert_eq!(chunks.concat(), clean);
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| chunk.matches('*').count())
                .sum::<usize>(),
            1
        );
        assert!(chunks.concat().contains("*\u{200B}literal"));

        let markdown = format!(
            "A{}*B {}",
            crate::markdown::SLACK_FORMAT_MARKER,
            "x".repeat(MAX_TEXT_CHARS)
        );
        let formatted = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let clean = crate::markdown::to_slack_mrkdwn(&markdown);
        let chunks = split_text(&formatted);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert_eq!(chunks.concat(), clean);
        assert!(clean.contains(crate::markdown::SLACK_FORMAT_MARKER));

        let markdown = format!(
            "[A{}*B](https://example.com) {}",
            crate::markdown::SLACK_FORMAT_MARKER,
            "x".repeat(MAX_TEXT_CHARS)
        );
        let formatted = crate::markdown::to_slack_mrkdwn_for_chunking(&markdown);
        let clean = crate::markdown::to_slack_mrkdwn(&markdown);
        let chunks = split_text(&formatted);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_TEXT_CHARS));
        assert_eq!(chunks.concat(), clean);
        assert!(clean.contains(crate::markdown::SLACK_FORMAT_MARKER));
    }

    #[tokio::test]
    async fn socket_mode_persists_before_ack_and_deduplicates_retries() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let envelope = json!({
            "type": "events_api",
            "envelope_id": "env-1",
            "payload": payload(json!({
                "type": "message", "subtype": "file_share", "channel_type": "im",
                "channel": "D1", "user": "U1", "text": "", "ts": "1.2",
                "files": [{
                    "id": "F1", "size": 12, "mimetype": "image/png",
                    "url_private_download": "https://files.slack.com/private-secret"
                }]
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
        assert_eq!(first[0].images.len(), 1);
        assert_eq!(first[0].images[0].locator, "F1");
        assert_eq!(first[0].images[0].file_size, Some(12));
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
                if envelope_id == "env-unauthorized" {
                    event["files"] = json!([{
                        "id": "FSECRET", "size": 12, "mimetype": "image/png",
                        "url_private_download": "https://files.slack.com/private-secret"
                    }]);
                }
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
            assert!(row.images.is_empty());
            assert!(channel.accept(row).is_none());
        }
        assert_eq!(rows[3].event_id(), "slack:EvV");
        assert_eq!(rows[3].handle, "U1");
        assert_eq!(rows[3].text, "message");
        assert!(channel.accept(&rows[3]).is_some());
        let database = std::fs::read(&path).unwrap();
        assert!(!database.windows(7).any(|window| window == b"FSECRET"));
        assert!(!database
            .windows(14)
            .any(|window| window == b"private-secret"));

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
