//! Telegram Bot API client using outbound-only long polling.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::channel::{InboundVoice, RawMessage};
use crate::voice::{AudioClip, MAX_AUDIO_BYTES};

pub const TEXT_LIMIT: usize = 4096;
pub const RICH_TEXT_LIMIT: usize = 32768;
const LONG_POLL_SECONDS: u64 = 25;
const HTTP_TIMEOUT_SECONDS: u64 = LONG_POLL_SECONDS + 10;

struct TransportResponse {
    status: u16,
    body: Value,
}

type TransportFuture<'a> = Pin<Box<dyn Future<Output = Result<TransportResponse>> + Send + 'a>>;
type BytesFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;

trait Transport: Send + Sync {
    fn post<'a>(&'a self, token: &'a str, method: &'static str, body: Value)
        -> TransportFuture<'a>;

    fn download<'a>(&'a self, _token: &'a str, _file_path: &'a str) -> BytesFuture<'a> {
        Box::pin(async { bail!("Telegram file download is not available") })
    }

    #[cfg_attr(test, allow(dead_code))]
    fn post_voice<'a>(
        &'a self,
        _token: &'a str,
        _payload: Value,
        _clip: &'a AudioClip,
    ) -> TransportFuture<'a> {
        Box::pin(async { bail!("Telegram voice upload is not available") })
    }
}

struct ReqwestTransport {
    client: reqwest::Client,
}

impl Transport for ReqwestTransport {
    fn post<'a>(
        &'a self,
        token: &'a str,
        method: &'static str,
        body: Value,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/bot{token}/{method}");
            let response = self
                .client
                .post(url)
                .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
                .json(&body)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Telegram {method} request failed"))?;
            let status = response.status().as_u16();
            let body = response.json().await.map_err(|_| {
                anyhow::anyhow!("Telegram {method} returned HTTP {status} with invalid JSON")
            })?;
            Ok(TransportResponse { status, body })
        })
    }

    fn download<'a>(&'a self, token: &'a str, file_path: &'a str) -> BytesFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
            let mut response = self
                .client
                .get(url)
                .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Telegram voice download failed"))?;
            let status = response.status();
            if !status.is_success() {
                bail!("Telegram voice download returned HTTP {}", status.as_u16());
            }
            if response
                .content_length()
                .is_some_and(|size| size > MAX_AUDIO_BYTES as u64)
            {
                bail!("Telegram voice message exceeds the 20 MB limit");
            }
            let mut bytes = Vec::with_capacity(
                response
                    .content_length()
                    .unwrap_or_default()
                    .min(MAX_AUDIO_BYTES as u64) as usize,
            );
            while let Some(chunk) = response
                .chunk()
                .await
                .context("read Telegram voice message")?
            {
                if bytes.len().saturating_add(chunk.len()) > MAX_AUDIO_BYTES {
                    bail!("Telegram voice message exceeds the 20 MB limit");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(bytes)
        })
    }

    fn post_voice<'a>(
        &'a self,
        token: &'a str,
        payload: Value,
        clip: &'a AudioClip,
    ) -> TransportFuture<'a> {
        Box::pin(async move {
            let url = format!("https://api.telegram.org/bot{token}/sendVoice");
            let mut form = reqwest::multipart::Form::new();
            let object = payload
                .as_object()
                .context("Telegram voice payload must be an object")?;
            for (key, value) in object {
                let value = value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string());
                form = form.text(key.clone(), value);
            }
            let part = reqwest::multipart::Part::bytes(clip.bytes.clone())
                .file_name(clip.filename.clone())
                .mime_str(&clip.mime_type)
                .context("prepare Telegram voice reply")?;
            let response = self
                .client
                .post(url)
                .timeout(Duration::from_secs(HTTP_TIMEOUT_SECONDS))
                .multipart(form.part("voice", part))
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("Telegram sendVoice request failed"))?;
            let status = response.status().as_u16();
            let body = response.json().await.with_context(|| {
                format!("Telegram sendVoice returned HTTP {status} with invalid JSON")
            })?;
            Ok(TransportResponse { status, body })
        })
    }
}

#[derive(Clone)]
pub struct Telegram {
    token: Arc<str>,
    allow_user_ids: Arc<HashSet<i64>>,
    allow_chat_ids: Arc<HashSet<i64>>,
    transport: Arc<dyn Transport>,
}

impl Telegram {
    pub fn new(token: String, allow_user_ids: Vec<i64>, allow_chat_ids: Vec<i64>) -> Self {
        Self {
            token: Arc::from(token),
            allow_user_ids: Arc::new(allow_user_ids.into_iter().collect()),
            allow_chat_ids: Arc::new(allow_chat_ids.into_iter().collect()),
            transport: Arc::new(ReqwestTransport {
                client: reqwest::Client::new(),
            }),
        }
    }

    #[cfg(test)]
    fn with_transport(
        token: String,
        allow_user_ids: Vec<i64>,
        allow_chat_ids: Vec<i64>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            token: Arc::from(token),
            allow_user_ids: Arc::new(allow_user_ids.into_iter().collect()),
            allow_chat_ids: Arc::new(allow_chat_ids.into_iter().collect()),
            transport,
        }
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.get_updates(since.saturating_add(1), LONG_POLL_SECONDS)
            .await
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        Ok(self
            .get_updates(-1, 0)
            .await?
            .into_iter()
            .map(|message| message.row_id)
            .max()
            .unwrap_or_default())
    }

    async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<RawMessage>> {
        let transport_response = self
            .transport
            .post(
                &self.token,
                "getUpdates",
                json!({
                    "offset": offset,
                    "timeout": timeout,
                    "allowed_updates": ["message"]
                }),
            )
            .await?;
        let response: ApiResponse<Vec<Update>> = serde_json::from_value(transport_response.body)
            .map_err(|_| anyhow::anyhow!("Telegram getUpdates returned an invalid response"))?;
        if !response.ok {
            bail!(
                "Telegram getUpdates returned HTTP {}",
                transport_response.status
            );
        }
        Ok(response
            .result
            .unwrap_or_default()
            .into_iter()
            .map(Update::into_raw)
            .collect())
    }

    pub fn is_allowed(&self, message: &RawMessage) -> bool {
        message
            .handle
            .parse::<i64>()
            .ok()
            .is_some_and(|id| self.allow_user_ids.contains(&id))
            || message
                .chat_identifier
                .parse::<i64>()
                .ok()
                .is_some_and(|id| self.allow_chat_ids.contains(&id))
    }

    pub fn allows_target(&self, chat_id: i64) -> bool {
        self.allow_user_ids.contains(&chat_id) || self.allow_chat_ids.contains(&chat_id)
    }

    pub async fn send_rich(&self, target: &str, text: &str) -> Result<()> {
        let mut payload = target_payload(target);
        payload["rich_message"] = json!({"markdown": text});
        let transport_response = self
            .post_with_topic_fallback("sendRichMessage", payload)
            .await?;
        let response: ApiResponse<Value> = serde_json::from_value(transport_response.body)
            .map_err(|_| {
                anyhow::anyhow!("Telegram sendRichMessage returned an invalid response")
            })?;
        if !response.ok {
            if transport_response.status == 400 {
                return self.send_plain_chunks(target, text).await;
            }
            bail!(
                "Telegram sendRichMessage returned HTTP {}",
                transport_response.status
            );
        }
        Ok(())
    }

    async fn send_plain_chunks(&self, target: &str, text: &str) -> Result<()> {
        for chunk in split_text(text) {
            self.send_plain(target, &chunk).await?;
        }
        Ok(())
    }

    pub async fn send_plain(&self, target: &str, text: &str) -> Result<()> {
        let mut payload = target_payload(target);
        payload["text"] = json!(text);
        let transport_response = self
            .post_with_topic_fallback("sendMessage", payload)
            .await?;
        let response: ApiResponse<Value> = serde_json::from_value(transport_response.body)
            .map_err(|_| anyhow::anyhow!("Telegram sendMessage returned an invalid response"))?;
        if !response.ok {
            bail!(
                "Telegram sendMessage returned HTTP {}",
                transport_response.status
            );
        }
        Ok(())
    }

    pub async fn send_typing(&self, target: &str) -> Result<()> {
        let mut payload = target_payload(target);
        payload["action"] = json!("typing");
        let transport_response = self
            .post_with_topic_fallback("sendChatAction", payload)
            .await?;
        let response: ApiResponse<Value> = serde_json::from_value(transport_response.body)
            .map_err(|_| anyhow::anyhow!("Telegram sendChatAction returned an invalid response"))?;
        if !response.ok {
            bail!(
                "Telegram sendChatAction returned HTTP {}",
                transport_response.status
            );
        }
        Ok(())
    }

    pub async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        if voice.file_size.is_some_and(|size| size > MAX_AUDIO_BYTES) {
            bail!("Telegram voice message exceeds the 20 MB limit");
        }
        let transport_response = self
            .transport
            .post(&self.token, "getFile", json!({"file_id": voice.locator}))
            .await?;
        let response: ApiResponse<TelegramFile> =
            serde_json::from_value(transport_response.body)
                .map_err(|_| anyhow::anyhow!("Telegram getFile returned an invalid response"))?;
        if !response.ok {
            bail!(
                "Telegram getFile returned HTTP {}",
                transport_response.status
            );
        }
        let file_path = response
            .result
            .context("Telegram getFile omitted the file path")?
            .file_path;
        let bytes = self.transport.download(&self.token, &file_path).await?;
        Ok(AudioClip {
            bytes,
            filename: voice.filename.clone(),
            mime_type: voice.mime_type.clone(),
        })
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()> {
        let payload = target_payload(target);
        let transport_response = self.post_voice_with_topic_fallback(payload, clip).await?;
        let response: ApiResponse<Value> = serde_json::from_value(transport_response.body)
            .map_err(|_| anyhow::anyhow!("Telegram sendVoice returned an invalid response"))?;
        if !response.ok {
            bail!(
                "Telegram sendVoice returned HTTP {}",
                transport_response.status
            );
        }
        Ok(())
    }

    #[cfg_attr(test, allow(dead_code))]
    async fn post_voice_with_topic_fallback(
        &self,
        mut payload: Value,
        clip: &AudioClip,
    ) -> Result<TransportResponse> {
        let response = self
            .transport
            .post_voice(&self.token, payload.clone(), clip)
            .await?;
        let thread_missing = response.status == 400
            && response
                .body
                .get("description")
                .and_then(Value::as_str)
                .is_some_and(|description| description.contains("message thread not found"));
        if !thread_missing || payload.get("message_thread_id").is_none() {
            return Ok(response);
        }
        if let Some(object) = payload.as_object_mut() {
            object.remove("message_thread_id");
        }
        self.transport.post_voice(&self.token, payload, clip).await
    }

    /// Posts the payload, retrying once without `message_thread_id` when
    /// Telegram rejects a private-chat topic send with "message thread not
    /// found", for example when a topic is stale or unavailable. The retry
    /// lands the reply in the main chat view instead of losing it.
    async fn post_with_topic_fallback(
        &self,
        method: &'static str,
        mut payload: Value,
    ) -> Result<TransportResponse> {
        let response = self
            .transport
            .post(&self.token, method, payload.clone())
            .await?;
        let ok = response
            .body
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let thread_missing = response
            .body
            .get("description")
            .and_then(Value::as_str)
            .is_some_and(|d| d.contains("message thread not found"));
        if ok
            || response.status != 400
            || !thread_missing
            || payload.get("message_thread_id").is_none()
        {
            return Ok(response);
        }
        if let Some(obj) = payload.as_object_mut() {
            obj.remove("message_thread_id");
        }
        self.transport.post(&self.token, method, payload).await
    }
}

/// Builds the send payload for a reply target. `Channel::accept` encodes
/// topic targets as `"{chat_id}:{topic_id}"`; plain targets are the chat id.
fn target_payload(target: &str) -> Value {
    let (chat, topic) = split_target(target);
    let mut payload = json!({"chat_id": chat});
    if let Some(topic) = topic {
        payload["message_thread_id"] = json!(topic);
    }
    payload
}

fn split_target(target: &str) -> (&str, Option<i64>) {
    match target.split_once(':') {
        Some((chat, topic)) => match topic.parse::<i64>() {
            Ok(topic) => (chat, Some(topic)),
            Err(_) => (target, None),
        },
        None => (target, None),
    }
}

#[derive(Deserialize)]
struct ApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
}

#[derive(Deserialize)]
struct Update {
    update_id: i64,
    #[serde(default)]
    message: Option<TelegramMessage>,
}

impl Update {
    fn into_raw(self) -> RawMessage {
        let Some(message) = self.message else {
            return RawMessage {
                row_id: self.update_id,
                channel: "telegram",
                handle: String::new(),
                chat_identifier: String::new(),
                is_group: false,
                text: String::new(),
                voice: None,
                is_from_me: false,
                is_supported: false,
                thread_id: None,
            };
        };
        RawMessage {
            row_id: self.update_id,
            channel: "telegram",
            handle: message
                .from
                .map(|sender| sender.id.to_string())
                .unwrap_or_default(),
            chat_identifier: message.chat.id.to_string(),
            is_group: message.chat.kind != "private",
            text: message.text.unwrap_or_default(),
            voice: message.voice.map(|voice| InboundVoice {
                locator: voice.file_id,
                file_size: voice.file_size,
                mime_type: voice.mime_type.unwrap_or_else(|| "audio/ogg".to_string()),
                filename: "voice.ogg".to_string(),
                data: None,
            }),
            is_from_me: false,
            is_supported: true,
            thread_id: message.message_thread_id,
        }
    }
}

#[derive(Deserialize)]
struct TelegramMessage {
    #[serde(default)]
    from: Option<User>,
    chat: Chat,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    voice: Option<TelegramVoice>,
    #[serde(default)]
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TelegramVoice {
    file_id: String,
    #[serde(default)]
    file_size: Option<usize>,
    #[serde(default)]
    mime_type: Option<String>,
}

#[derive(Default, Deserialize)]
struct TelegramFile {
    file_path: String,
}

#[derive(Deserialize)]
struct User {
    id: i64,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

pub fn split_text(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for character in text.chars() {
        let character_len = character.len_utf16();
        if current_len + character_len > TEXT_LIMIT && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push(character);
        current_len += character_len;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeTransport {
        calls: Mutex<Vec<(String, Value)>>,
        responses: Mutex<VecDeque<TransportResponse>>,
        downloads: Mutex<VecDeque<Vec<u8>>>,
        download_paths: Mutex<Vec<String>>,
        voice_calls: Mutex<Vec<(Value, AudioClip)>>,
    }

    impl FakeTransport {
        fn with_responses(responses: Vec<Value>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|body| TransportResponse { status: 200, body })
                        .collect(),
                ),
                downloads: Mutex::new(VecDeque::new()),
                download_paths: Mutex::new(Vec::new()),
                voice_calls: Mutex::new(Vec::new()),
            }
        }

        fn with_status_responses(responses: Vec<(u16, Value)>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(
                    responses
                        .into_iter()
                        .map(|(status, body)| TransportResponse { status, body })
                        .collect(),
                ),
                downloads: Mutex::new(VecDeque::new()),
                download_paths: Mutex::new(Vec::new()),
                voice_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl Transport for FakeTransport {
        fn post<'a>(
            &'a self,
            _token: &'a str,
            method: &'static str,
            body: Value,
        ) -> TransportFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((method.to_string(), body));
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("missing fake response"))
            })
        }

        fn download<'a>(&'a self, _token: &'a str, file_path: &'a str) -> BytesFuture<'a> {
            Box::pin(async move {
                self.download_paths
                    .lock()
                    .unwrap()
                    .push(file_path.to_string());
                self.downloads
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("missing fake download"))
            })
        }

        fn post_voice<'a>(
            &'a self,
            _token: &'a str,
            payload: Value,
            clip: &'a AudioClip,
        ) -> TransportFuture<'a> {
            Box::pin(async move {
                self.voice_calls
                    .lock()
                    .unwrap()
                    .push((payload, clip.clone()));
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| anyhow::anyhow!("missing fake response"))
            })
        }
    }

    fn updates() -> Value {
        json!({
            "ok": true,
            "result": [
                {
                    "update_id": 101,
                    "message": {
                        "from": {"id": 7},
                        "chat": {"id": 7, "type": "private"},
                        "text": "hello"
                    }
                },
                {
                    "update_id": 102,
                    "message": {
                        "from": {"id": 8},
                        "chat": {"id": -10, "type": "group"},
                        "text": "group"
                    }
                },
                {"update_id": 103, "edited_message": {}},
                {
                    "update_id": 104,
                    "message": {
                        "from": {"id": 7},
                        "chat": {"id": 7, "type": "private"},
                        "text": "in a topic",
                        "message_thread_id": 99,
                        "is_topic_message": true
                    }
                }
            ]
        })
    }

    #[tokio::test]
    async fn parses_private_group_and_unsupported_updates() {
        let fake = Arc::new(FakeTransport::with_responses(vec![updates()]));
        let telegram = Telegram::with_transport("secret".to_string(), vec![7], vec![], fake);

        let messages = telegram.poll(100).await.unwrap();

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].row_id, 101);
        assert_eq!(messages[0].handle, "7");
        assert_eq!(messages[0].chat_identifier, "7");
        assert!(!messages[0].is_group);
        assert_eq!(messages[0].thread_id, None);
        assert!(messages[1].is_group);
        assert!(!messages[2].is_supported);
        assert_eq!(messages[3].thread_id, Some(99));
        assert!(!messages[3].is_group);
    }

    #[tokio::test]
    async fn poll_uses_next_update_offset_and_long_poll_timeout() {
        let fake = Arc::new(FakeTransport::with_responses(vec![json!({
            "ok": true,
            "result": []
        })]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        telegram.poll(41).await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "getUpdates");
        assert_eq!(calls[0].1["offset"], 42);
        assert_eq!(calls[0].1["timeout"], LONG_POLL_SECONDS);
    }

    #[tokio::test]
    async fn first_run_cursor_discards_pending_updates_with_negative_offset() {
        let fake = Arc::new(FakeTransport::with_responses(vec![updates()]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        let cursor = telegram.latest_cursor().await.unwrap();

        assert_eq!(cursor, 104);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].1["offset"], -1);
        assert_eq!(calls[0].1["timeout"], 0);
    }

    #[test]
    fn allowlist_accepts_user_or_chat_id() {
        let telegram = Telegram::new("secret".to_string(), vec![7], vec![9]);
        let mut message = Update {
            update_id: 1,
            message: Some(TelegramMessage {
                from: Some(User { id: 7 }),
                chat: Chat {
                    id: 7,
                    kind: "private".to_string(),
                },
                text: Some("hi".to_string()),
                voice: None,
                message_thread_id: None,
            }),
        }
        .into_raw();
        assert!(telegram.is_allowed(&message));
        message.handle = "8".to_string();
        message.chat_identifier = "9".to_string();
        assert!(telegram.is_allowed(&message));
        message.chat_identifier = "10".to_string();
        assert!(!telegram.is_allowed(&message));
    }

    #[test]
    fn parses_voice_attachment_without_downloading_it() {
        let message = Update {
            update_id: 2,
            message: Some(TelegramMessage {
                from: Some(User { id: 7 }),
                chat: Chat {
                    id: 7,
                    kind: "private".to_string(),
                },
                text: None,
                voice: Some(TelegramVoice {
                    file_id: "file-123".to_string(),
                    file_size: Some(42),
                    mime_type: Some("audio/ogg".to_string()),
                }),
                message_thread_id: None,
            }),
        }
        .into_raw();

        assert!(message.text.is_empty());
        let voice = message.voice.unwrap();
        assert_eq!(voice.locator, "file-123");
        assert_eq!(voice.file_size, Some(42));
        assert!(voice.data.is_none());
    }

    #[tokio::test]
    async fn downloads_voice_by_file_id_and_returns_generic_audio() {
        let fake = Arc::new(FakeTransport::with_responses(vec![json!({
            "ok": true,
            "result": {"file_path": "voice/file.oga"}
        })]));
        fake.downloads.lock().unwrap().push_back(vec![1, 2, 3]);
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());
        let voice = InboundVoice {
            locator: "file-123".to_string(),
            file_size: Some(3),
            mime_type: "audio/ogg".to_string(),
            filename: "voice.ogg".to_string(),
            data: None,
        };

        let clip = telegram.download_voice(&voice).await.unwrap();

        assert_eq!(clip.bytes, vec![1, 2, 3]);
        assert_eq!(fake.calls.lock().unwrap()[0].0, "getFile");
        assert_eq!(fake.calls.lock().unwrap()[0].1["file_id"], "file-123");
        assert_eq!(
            fake.download_paths.lock().unwrap().as_slice(),
            ["voice/file.oga"]
        );
    }

    #[tokio::test]
    async fn uploads_opus_voice_to_the_exact_topic() {
        let fake = Arc::new(FakeTransport::with_responses(vec![json!({
            "ok": true,
            "result": {}
        })]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());
        let clip = AudioClip {
            bytes: vec![4, 5, 6],
            filename: "reply.opus".to_string(),
            mime_type: "audio/ogg".to_string(),
        };

        telegram.send_voice("7:99", &clip).await.unwrap();

        let calls = fake.voice_calls.lock().unwrap();
        assert_eq!(calls[0].0["chat_id"], "7");
        assert_eq!(calls[0].0["message_thread_id"], 99);
        assert_eq!(calls[0].1, clip);
    }

    #[test]
    fn splits_exact_over_limit_multi_chunk_and_unicode_text() {
        assert_eq!(split_text(&"a".repeat(TEXT_LIMIT)).len(), 1);
        let over = split_text(&format!("{}é", "a".repeat(TEXT_LIMIT)));
        assert_eq!(over.len(), 2);
        assert_eq!(over[1], "é");
        let multi = split_text(&"x".repeat(TEXT_LIMIT * 2 + 1));
        assert_eq!(
            multi
                .iter()
                .map(|chunk| chunk.encode_utf16().count())
                .collect::<Vec<_>>(),
            vec![TEXT_LIMIT, TEXT_LIMIT, 1]
        );
        let emoji = split_text(&"😀".repeat(TEXT_LIMIT / 2 + 1));
        assert_eq!(emoji.len(), 2);
        assert!(emoji
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= TEXT_LIMIT));
        assert!(split_text("").is_empty());
    }

    #[tokio::test]
    async fn send_path_posts_rich_markdown_without_token_in_payload() {
        let fake = Arc::new(FakeTransport::with_responses(vec![json!({
            "ok": true,
            "result": {}
        })]));
        let telegram =
            Telegram::with_transport("do-not-log".to_string(), vec![7], vec![], fake.clone());

        telegram.send_rich("7", "reply").await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [(
                "sendRichMessage".to_string(),
                json!({
                    "chat_id": "7",
                    "rich_message": {"markdown": "reply"}
                })
            )]
        );
        assert!(!calls[0].1.to_string().contains("do-not-log"));
    }

    #[tokio::test]
    async fn rejected_rich_markdown_falls_back_to_plain_chunks() {
        let fake = Arc::new(FakeTransport::with_status_responses(vec![
            (400, json!({"ok": false, "error_code": 400})),
            (200, json!({"ok": true, "result": {}})),
            (200, json!({"ok": true, "result": {}})),
        ]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());
        let text = format!("{}é", "x".repeat(TEXT_LIMIT));

        telegram.send_rich("7", &text).await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "sendRichMessage");
        assert_eq!(calls[1].0, "sendMessage");
        assert_eq!(calls[2].0, "sendMessage");
        assert_eq!(
            calls[1..]
                .iter()
                .map(|(_, body)| body["text"].as_str().unwrap())
                .collect::<String>(),
            text
        );
    }

    #[tokio::test]
    async fn topic_target_sends_message_thread_id() {
        let fake = Arc::new(FakeTransport::with_responses(vec![
            json!({"ok": true, "result": {}}),
            json!({"ok": true, "result": {}}),
            json!({"ok": true, "result": true}),
        ]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        telegram.send_plain("7:99", "reply").await.unwrap();
        telegram.send_rich("7:99", "reply").await.unwrap();
        telegram.send_typing("7:99").await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            json!({"chat_id": "7", "message_thread_id": 99, "text": "reply"})
        );
        assert_eq!(
            calls[1].1,
            json!({
                "chat_id": "7",
                "message_thread_id": 99,
                "rich_message": {"markdown": "reply"}
            })
        );
        assert_eq!(
            calls[2].1,
            json!({"chat_id": "7", "message_thread_id": 99, "action": "typing"})
        );
    }

    #[tokio::test]
    async fn topic_send_retries_without_thread_id_on_thread_not_found() {
        let fake = Arc::new(FakeTransport::with_status_responses(vec![
            (
                400,
                json!({
                    "ok": false,
                    "error_code": 400,
                    "description": "Bad Request: message thread not found"
                }),
            ),
            (200, json!({"ok": true, "result": {}})),
        ]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        telegram.send_plain("7:99", "reply").await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [
                (
                    "sendMessage".to_string(),
                    json!({"chat_id": "7", "message_thread_id": 99, "text": "reply"})
                ),
                (
                    "sendMessage".to_string(),
                    json!({"chat_id": "7", "text": "reply"})
                ),
            ]
        );
    }

    #[tokio::test]
    async fn topic_send_does_not_retry_other_400s_and_rich_still_falls_back() {
        let fake = Arc::new(FakeTransport::with_status_responses(vec![(
            400,
            json!({"ok": false, "error_code": 400, "description": "Bad Request: chat not found"}),
        )]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        let error = telegram.send_plain("7:99", "reply").await.unwrap_err();

        assert!(error.to_string().contains("HTTP 400"));
        assert_eq!(fake.calls.lock().unwrap().len(), 1);

        let fake = Arc::new(FakeTransport::with_status_responses(vec![
            (400, json!({"ok": false, "error_code": 400})),
            (200, json!({"ok": true, "result": {}})),
        ]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());

        telegram.send_rich("7:99", "reply").await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0].0, "sendRichMessage");
        assert_eq!(
            calls[1].1,
            json!({"chat_id": "7", "message_thread_id": 99, "text": "reply"})
        );
    }

    #[tokio::test]
    async fn poll_preserves_http_conflict_status() {
        let fake = Arc::new(FakeTransport::with_status_responses(vec![(
            409,
            json!({"ok": false, "error_code": 409}),
        )]));
        let telegram = Telegram::with_transport("secret".to_string(), vec![7], vec![], fake);

        let error = telegram.poll(0).await.unwrap_err();

        assert!(error.to_string().contains("HTTP 409"));
    }

    #[tokio::test]
    async fn typing_path_posts_chat_action_without_token_in_payload() {
        let fake = Arc::new(FakeTransport::with_responses(vec![json!({
            "ok": true,
            "result": true
        })]));
        let telegram =
            Telegram::with_transport("do-not-log".to_string(), vec![7], vec![], fake.clone());

        telegram.send_typing("7").await.unwrap();

        let calls = fake.calls.lock().unwrap();
        assert_eq!(
            calls.as_slice(),
            [(
                "sendChatAction".to_string(),
                json!({"chat_id": "7", "action": "typing"})
            )]
        );
        assert!(!calls[0].1.to_string().contains("do-not-log"));
    }

    #[tokio::test]
    async fn long_reply_send_path_preserves_chunk_order_and_limits() {
        let fake = Arc::new(FakeTransport::with_responses(vec![
            json!({"ok": true, "result": {}}),
            json!({"ok": true, "result": {}}),
        ]));
        let telegram =
            Telegram::with_transport("secret".to_string(), vec![7], vec![], fake.clone());
        let text = format!("{}é", "a".repeat(TEXT_LIMIT));

        for chunk in split_text(&text) {
            telegram.send_plain("7", &chunk).await.unwrap();
        }

        let calls = fake.calls.lock().unwrap();
        let sent: Vec<String> = calls
            .iter()
            .map(|(_, body)| body["text"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(sent, vec!["a".repeat(TEXT_LIMIT), "é".to_string()]);
        assert!(sent
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= TEXT_LIMIT));
    }
}
