//! Channel-neutral messaging boundary used by the gateway.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::approval::AnswerOrigin;
use crate::config::{ChannelKind, Config};
use crate::imessage::{Poller as IMessagePoller, Sender as IMessageSender};
use crate::simplex::Simplex;
use crate::telegram::Telegram;
use crate::voice::AudioClip;

pub(crate) const REPLY_MARKER: &str = "\n\n-- sent by push";

#[derive(Debug, Clone)]
pub struct InboundVoice {
    /// Channel-owned file identifier. The voice layer treats this as opaque.
    pub locator: String,
    pub file_size: Option<usize>,
    pub mime_type: String,
    pub filename: String,
    /// Channels that already have the bytes may provide them directly.
    pub data: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct RawMessage {
    pub row_id: i64,
    pub channel: &'static str,
    pub handle: String,
    pub chat_identifier: String,
    pub is_group: bool,
    pub text: String,
    pub voice: Option<InboundVoice>,
    pub is_from_me: bool,
    pub is_supported: bool,
    /// Channel-specific thread/topic id (Telegram `message_thread_id`).
    pub thread_id: Option<i64>,
}

impl RawMessage {
    pub fn event_id(&self) -> String {
        format!("{}:{}", self.channel, self.row_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundChunk {
    pub text: String,
    pub rich_markdown: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliverySemantics {
    pub send_timeout: Duration,
    pub retry_attempts: usize,
    pub retry_delay: Duration,
    pub exhausted_retry_delay: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownSemantics {
    pub worker_grace: Duration,
}

/// The static contract every gateway channel must satisfy.
///
/// Poll futures must be cancellation-safe: the gateway drops an in-progress
/// poll when shutdown is requested. Accepted messages must return a stable,
/// channel-qualified thread key and the exact target used for every reply.
/// Delivery is chunked before sending so rich-message formatting and retry
/// boundaries remain channel-owned. Implementations must also treat typing as
/// best-effort and release transport resources when a poll or send future is
/// dropped.
#[allow(async_fn_in_trait)]
trait ChannelContract {
    fn id(&self) -> &'static str;
    fn primary_target(&self, configured: &str) -> Result<String>;
    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>>;
    async fn latest_cursor(&self) -> Result<i64>;
    fn accept(&self, message: &RawMessage) -> Option<(String, String)>;
    fn reject_reason(&self, message: &RawMessage) -> &'static str;
    fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin;
    fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>>;
    fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk>;

    fn scheduled_outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        self.outbound_chunks(text, marker)
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()>;

    fn typing_refresh(&self) -> Option<Duration> {
        None
    }

    async fn send_typing(&self, _target: &str) -> Result<()> {
        Ok(())
    }

    async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip>;
    async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()>;

    fn delivery_semantics(&self) -> DeliverySemantics {
        DeliverySemantics {
            send_timeout: Duration::from_secs(30),
            retry_attempts: 3,
            retry_delay: Duration::from_millis(250),
            exhausted_retry_delay: Duration::from_secs(5),
        }
    }

    fn shutdown_semantics(&self) -> ShutdownSemantics {
        ShutdownSemantics {
            worker_grace: Duration::from_secs(30),
        }
    }

    /// Release any live resources the channel is holding (background tasks,
    /// open connections). Default no-op: iMessage (chat.db polling) and
    /// Telegram (stateless HTTP long-poll) hold nothing that needs explicit
    /// teardown. SimpleX overrides this to abort its event-pump task.
    async fn shutdown(&self) {}
}

#[derive(Clone)]
pub(crate) struct IMessageChannel {
    pub(crate) poller: IMessagePoller,
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) sender: IMessageSender,
    pub(crate) self_set: HashMap<String, String>,
    pub(crate) allow_set: HashMap<String, String>,
    pub(crate) reply_marker: String,
}

#[derive(Clone)]
pub enum Channel {
    IMessage(IMessageChannel),
    Telegram(Telegram),
    Simplex(Arc<Simplex>),
}

impl Channel {
    pub async fn new_for(cfg: &Config, kind: ChannelKind) -> Result<Self> {
        match kind {
            ChannelKind::IMessage => Ok(Self::IMessage(IMessageChannel {
                poller: IMessagePoller::new(cfg.db_path.clone()),
                sender: IMessageSender::new(),
                self_set: cfg
                    .self_handles
                    .iter()
                    .map(|value| (normalize_handle(value), thread_handle(value)))
                    .collect(),
                allow_set: cfg
                    .allow_from
                    .iter()
                    .map(|value| (normalize_handle(value), thread_handle(value)))
                    .collect(),
                reply_marker: REPLY_MARKER.to_string(),
            })),
            ChannelKind::Telegram => Ok(Self::Telegram(Telegram::new(
                cfg.telegram_token()
                    .ok_or_else(|| anyhow::anyhow!("Telegram bot token is not configured"))?,
                cfg.telegram_allow_user_ids.clone(),
                cfg.telegram_allow_chat_ids.clone(),
            ))),
            ChannelKind::Simplex => {
                let port = cfg.simplex_port.unwrap_or(5225);
                let simplex = Simplex::connect(
                    port,
                    &cfg.simplex_display_name,
                    cfg.simplex_allow_contact_ids.clone(),
                )
                .await
                .with_context(|| format!("start SimpleX channel against 127.0.0.1:{port}"))?;
                Ok(Self::Simplex(Arc::new(simplex)))
            }
        }
    }

    pub fn primary_target(&self, configured: &str) -> Result<String> {
        match self {
            Self::IMessage(channel) => ChannelContract::primary_target(channel, configured),
            Self::Telegram(channel) => ChannelContract::primary_target(channel, configured),
            Self::Simplex(channel) => ChannelContract::primary_target(channel.as_ref(), configured),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::IMessage(channel) => ChannelContract::id(channel),
            Self::Telegram(channel) => ChannelContract::id(channel),
            Self::Simplex(channel) => ChannelContract::id(channel.as_ref()),
        }
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        match self {
            Self::IMessage(channel) => ChannelContract::poll(channel, since).await,
            Self::Telegram(channel) => ChannelContract::poll(channel, since).await,
            Self::Simplex(channel) => ChannelContract::poll(channel.as_ref(), since).await,
        }
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        match self {
            Self::IMessage(channel) => ChannelContract::latest_cursor(channel).await,
            Self::Telegram(channel) => ChannelContract::latest_cursor(channel).await,
            Self::Simplex(channel) => ChannelContract::latest_cursor(channel.as_ref()).await,
        }
    }

    /// Returns `(thread_key, reply_target)` for an accepted message.
    pub fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        match self {
            Self::IMessage(channel) => ChannelContract::accept(channel, message),
            Self::Telegram(channel) => ChannelContract::accept(channel, message),
            Self::Simplex(channel) => ChannelContract::accept(channel.as_ref(), message),
        }
    }

    pub fn reject_reason(&self, message: &RawMessage) -> &'static str {
        match self {
            Self::IMessage(channel) => ChannelContract::reject_reason(channel, message),
            Self::Telegram(channel) => ChannelContract::reject_reason(channel, message),
            Self::Simplex(channel) => ChannelContract::reject_reason(channel.as_ref(), message),
        }
    }

    pub fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        match self {
            Self::IMessage(channel) => ChannelContract::approval_origin(channel, message, thread),
            Self::Telegram(channel) => ChannelContract::approval_origin(channel, message, thread),
            Self::Simplex(channel) => {
                ChannelContract::approval_origin(channel.as_ref(), message, thread)
            }
        }
    }

    pub fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        match self {
            Self::IMessage(channel) => ChannelContract::route_thread_groups(channel, thread),
            Self::Telegram(channel) => ChannelContract::route_thread_groups(channel, thread),
            Self::Simplex(channel) => ChannelContract::route_thread_groups(channel.as_ref(), thread),
        }
    }

    pub fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        match self {
            Self::IMessage(channel) => ChannelContract::outbound_chunks(channel, text, marker),
            Self::Telegram(channel) => ChannelContract::outbound_chunks(channel, text, marker),
            Self::Simplex(channel) => {
                ChannelContract::outbound_chunks(channel.as_ref(), text, marker)
            }
        }
    }

    pub fn scheduled_outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        match self {
            Self::IMessage(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
            Self::Simplex(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel.as_ref(), text, marker)
            }
            Self::Telegram(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
            Self::Telegram(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
            Self::Simplex(channel) => {
                ChannelContract::send_chunk(channel.as_ref(), target, chunk).await
            }
        }
    }

    pub fn typing_refresh(&self) -> Option<Duration> {
        match self {
            Self::IMessage(channel) => ChannelContract::typing_refresh(channel),
            Self::Telegram(channel) => ChannelContract::typing_refresh(channel),
            Self::Simplex(channel) => ChannelContract::typing_refresh(channel.as_ref()),
        }
    }

    pub async fn send_typing(&self, target: &str) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_typing(channel, target).await,
            Self::Telegram(channel) => ChannelContract::send_typing(channel, target).await,
            Self::Simplex(channel) => ChannelContract::send_typing(channel.as_ref(), target).await,
        }
    }

    pub async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        match self {
            Self::IMessage(channel) => ChannelContract::download_voice(channel, voice).await,
            Self::Telegram(channel) => ChannelContract::download_voice(channel, voice).await,
            Self::Simplex(channel) => ChannelContract::download_voice(channel.as_ref(), voice).await,
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_voice(channel, target, clip).await,
            Self::Telegram(channel) => ChannelContract::send_voice(channel, target, clip).await,
            Self::Simplex(channel) => {
                ChannelContract::send_voice(channel.as_ref(), target, clip).await
            }
        }
    }

    pub fn delivery_semantics(&self) -> DeliverySemantics {
        match self {
            Self::IMessage(channel) => ChannelContract::delivery_semantics(channel),
            Self::Telegram(channel) => ChannelContract::delivery_semantics(channel),
            Self::Simplex(channel) => ChannelContract::delivery_semantics(channel.as_ref()),
        }
    }

    pub fn shutdown_semantics(&self) -> ShutdownSemantics {
        match self {
            Self::IMessage(channel) => ChannelContract::shutdown_semantics(channel),
            Self::Telegram(channel) => ChannelContract::shutdown_semantics(channel),
            Self::Simplex(channel) => ChannelContract::shutdown_semantics(channel.as_ref()),
        }
    }

    pub async fn shutdown(&self) {
        match self {
            Self::IMessage(channel) => ChannelContract::shutdown(channel).await,
            Self::Telegram(channel) => ChannelContract::shutdown(channel).await,
            Self::Simplex(channel) => ChannelContract::shutdown(channel.as_ref()).await,
        }
    }
}

impl ChannelContract for IMessageChannel {
    fn id(&self) -> &'static str {
        "imessage"
    }

    fn primary_target(&self, configured: &str) -> Result<String> {
        if configured.trim().is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        let normalized = normalize_handle(configured);
        self.self_set
            .get(&normalized)
            .or_else(|| self.allow_set.get(&normalized))
            .cloned()
            .with_context(|| {
                format!(
                    "iMessage primary target {configured:?} is not in imessage.self_handles or imessage.allow_from"
                )
            })
    }

    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        let poller = self.poller.clone();
        let messages = tokio::task::spawn_blocking(move || poller.poll(since)).await??;
        Ok(messages
            .into_iter()
            .map(|message| RawMessage {
                row_id: message.row_id,
                channel: self.id(),
                handle: message.handle,
                chat_identifier: message.chat_identifier,
                is_group: message.is_group,
                text: message.text,
                voice: None,
                is_from_me: message.is_from_me,
                is_supported: true,
                thread_id: None,
            })
            .collect())
    }

    async fn latest_cursor(&self) -> Result<i64> {
        let poller = self.poller.clone();
        tokio::task::spawn_blocking(move || poller.max_row_id()).await?
    }

    fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        if common_reject_reason(message).is_some()
            || (!self.reply_marker.is_empty() && message.text.contains(&self.reply_marker))
        {
            return None;
        }
        let chat = normalize_handle(&message.chat_identifier);
        let handle = normalize_handle(&message.handle);
        if let Some(value) = self.self_set.get(&chat) {
            return Some((
                format!("imessage:self:{value}"),
                message.chat_identifier.clone(),
            ));
        }
        if !message.is_from_me {
            if let Some(value) = self.allow_set.get(&handle) {
                return Some((format!("imessage:dm:{value}"), message.handle.clone()));
            }
        }
        None
    }

    fn reject_reason(&self, message: &RawMessage) -> &'static str {
        common_reject_reason(message).unwrap_or_else(|| {
            if !self.reply_marker.is_empty() && message.text.contains(&self.reply_marker) {
                "reply_marker"
            } else if message.is_from_me {
                "from_me_to_other"
            } else {
                "not_allowlisted"
            }
        })
    }

    fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        let chat_key = normalize_handle(&message.chat_identifier);
        let sender_key = if thread.starts_with("imessage:self:") {
            chat_key.clone()
        } else {
            normalize_handle(&message.handle)
        };
        AnswerOrigin {
            channel: self.id().to_string(),
            thread_key: thread.to_string(),
            sender_key,
            chat_key,
        }
    }

    fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        let mut aliases = vec![thread.to_string()];
        if let Some(legacy) = thread.strip_prefix("imessage:") {
            aliases.push(legacy.to_string());
        }
        vec![aliases]
    }

    fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        if text.trim().is_empty() {
            Vec::new()
        } else {
            vec![OutboundChunk {
                text: format!("{text}{marker}"),
                rich_markdown: false,
            }]
        }
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        self.sender.send(target, &chunk.text).await
    }

    async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        if let Some(bytes) = &voice.data {
            return inline_audio(voice, bytes);
        }
        bail!("iMessage voice messages are not supported yet")
    }

    async fn send_voice(&self, _target: &str, _clip: &AudioClip) -> Result<()> {
        bail!("iMessage voice replies are not supported yet")
    }
}

impl ChannelContract for Telegram {
    fn id(&self) -> &'static str {
        "telegram"
    }

    fn primary_target(&self, configured: &str) -> Result<String> {
        if configured.trim().is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        let (chat, topic) = configured
            .trim()
            .split_once(':')
            .map_or((configured.trim(), None), |(chat, topic)| {
                (chat, Some(topic))
            });
        let chat_id = chat
            .parse::<i64>()
            .with_context(|| format!("invalid Telegram primary chat id {chat:?}"))?;
        if !self.allows_target(chat_id) {
            bail!(
                "Telegram primary chat id {chat_id} is not in telegram.allow_user_ids or telegram.allow_chat_ids"
            );
        }
        match topic {
            None => Ok(chat_id.to_string()),
            Some(value) => {
                if value.contains(':') {
                    bail!("invalid Telegram primary target {configured:?}");
                }
                let topic_id = value
                    .parse::<i64>()
                    .with_context(|| format!("invalid Telegram primary topic id {value:?}"))?;
                if topic_id <= 0 {
                    bail!("Telegram primary topic id must be positive");
                }
                Ok(format!("{chat_id}:{topic_id}"))
            }
        }
    }

    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.poll(since).await
    }

    async fn latest_cursor(&self) -> Result<i64> {
        self.latest_cursor().await
    }

    fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        if common_reject_reason(message).is_some() || !self.is_allowed(message) {
            return None;
        }
        Some(match message.thread_id {
            Some(topic) => (
                format!("telegram:dm:{}:topic:{topic}", message.chat_identifier),
                format!("{}:{topic}", message.chat_identifier),
            ),
            None => (
                format!("telegram:dm:{}", message.chat_identifier),
                message.chat_identifier.clone(),
            ),
        })
    }

    fn reject_reason(&self, message: &RawMessage) -> &'static str {
        common_reject_reason(message).unwrap_or("not_allowlisted")
    }

    fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        AnswerOrigin {
            channel: self.id().to_string(),
            thread_key: thread.to_string(),
            sender_key: message.handle.clone(),
            chat_key: message.chat_identifier.clone(),
        }
    }

    fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        let mut groups = vec![vec![thread.to_string()]];
        if let Some((parent, _)) = thread.rsplit_once(":topic:") {
            groups.push(vec![parent.to_string()]);
        }
        groups
    }

    fn outbound_chunks(&self, text: &str, _marker: &str) -> Vec<OutboundChunk> {
        crate::telegram::split_text(text)
            .into_iter()
            .map(|text| OutboundChunk {
                text,
                rich_markdown: true,
            })
            .collect()
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        if chunk.rich_markdown {
            self.send_rich(target, &chunk.text).await
        } else {
            self.send_plain(target, &chunk.text).await
        }
    }

    fn typing_refresh(&self) -> Option<Duration> {
        Some(Duration::from_secs(4))
    }

    async fn send_typing(&self, target: &str) -> Result<()> {
        self.send_typing(target).await
    }

    async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        if let Some(bytes) = &voice.data {
            return inline_audio(voice, bytes);
        }
        self.download_voice(voice).await
    }

    async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()> {
        self.send_voice(target, clip).await
    }
}

impl ChannelContract for Simplex {
    fn id(&self) -> &'static str {
        "simplex"
    }

    fn primary_target(&self, configured: &str) -> Result<String> {
        if configured.trim().is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        let contact_id = configured
            .trim()
            .parse::<i64>()
            .with_context(|| format!("invalid SimpleX primary contact id {configured:?}"))?;
        if !self.is_allowed(contact_id) {
            bail!("SimpleX primary contact id {contact_id} is not in simplex.allow_contact_ids");
        }
        Ok(contact_id.to_string())
    }

    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        let mut rx = self.inbound.lock().await;
        let mut out = Vec::new();
        loop {
            match rx.try_recv() {
                Ok(message) => {
                    if message.row_id > since {
                        out.push(message);
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    bail!("SimpleX event pump task ended; connection to the daemon was lost")
                }
            }
        }
        Ok(out)
    }

    async fn latest_cursor(&self) -> Result<i64> {
        Ok(self.next_row_id.load(std::sync::atomic::Ordering::SeqCst))
    }

    fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        simplex_accept(|id| self.is_allowed(id), message)
    }

    fn reject_reason(&self, message: &RawMessage) -> &'static str {
        common_reject_reason(message).unwrap_or("not_allowlisted")
    }

    fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        AnswerOrigin {
            channel: self.id().to_string(),
            thread_key: thread.to_string(),
            sender_key: message.handle.clone(),
            chat_key: message.chat_identifier.clone(),
        }
    }

    fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        // SimpleX has no topic/thread-within-chat concept comparable to
        // Telegram's forum topics, so there is only ever one alias group.
        vec![vec![thread.to_string()]]
    }

    fn outbound_chunks(&self, text: &str, _marker: &str) -> Vec<OutboundChunk> {
        simplex_outbound_chunks(text)
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        let contact_id: i64 = target
            .parse()
            .with_context(|| format!("invalid SimpleX send target {target:?}"))?;
        let chat_id = simploxide_client::prelude::ContactId::from_raw(contact_id);
        self.bot
            .send_msg(chat_id, chunk.text.clone())
            .await
            .with_context(|| format!("send SimpleX message to contact {contact_id}"))?;
        Ok(())
    }

    async fn download_voice(&self, _voice: &InboundVoice) -> Result<AudioClip> {
        bail!("SimpleX voice messages are not supported yet")
    }

    async fn send_voice(&self, _target: &str, _clip: &AudioClip) -> Result<()> {
        bail!("SimpleX voice replies are not supported yet")
    }

    fn shutdown_semantics(&self) -> ShutdownSemantics {
        ShutdownSemantics {
            worker_grace: Duration::from_secs(30),
        }
    }

    async fn shutdown(&self) {
        self.pump.abort();
    }
}

/// Pure `accept()` logic for the SimpleX channel, extracted so it's testable
/// without a live `Simplex` (which requires a real daemon connection to
/// construct at all — see contract §D's note on this). The trait impl calls
/// this with `|id| self.is_allowed(id)`; tests call it with a plain closure
/// over a `HashSet`.
fn simplex_accept(
    is_allowed: impl Fn(i64) -> bool,
    message: &RawMessage,
) -> Option<(String, String)> {
    if common_reject_reason(message).is_some() {
        return None;
    }
    let contact_id: i64 = message.handle.parse().ok()?;
    if !is_allowed(contact_id) {
        return None;
    }
    Some((
        format!("simplex:dm:{contact_id}"),
        message.chat_identifier.clone(),
    ))
}

/// Pure `outbound_chunks()` logic for the SimpleX channel, extracted for the
/// same testability reason as `simplex_accept` above.
fn simplex_outbound_chunks(text: &str) -> Vec<OutboundChunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    // SimpleX's practical message-size limit and rich-text support are
    // unconfirmed (see contract Open Questions #3/#4) — conservatively
    // reuse Telegram's 4096 UTF-16 boundary rather than inventing an
    // unverified SimpleX-specific number, and send plain text only.
    crate::telegram::split_text(text)
        .into_iter()
        .map(|text| OutboundChunk {
            text,
            rich_markdown: false,
        })
        .collect()
}

fn common_reject_reason(message: &RawMessage) -> Option<&'static str> {
    if !message.is_supported {
        Some("unsupported_update")
    } else if message.is_group {
        Some("group_chat")
    } else if message.text.trim().is_empty() && message.voice.is_none() {
        Some("empty_message")
    } else {
        None
    }
}

fn inline_audio(voice: &InboundVoice, bytes: &[u8]) -> Result<AudioClip> {
    Ok(AudioClip {
        bytes: bytes.to_vec(),
        filename: voice.filename.clone(),
        mime_type: voice.mime_type.clone(),
    })
}

pub(crate) fn normalize_handle(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.contains('@') {
        return trimmed.to_ascii_lowercase();
    }
    let digits: String = trimmed.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() {
        trimmed.to_ascii_lowercase()
    } else {
        digits
    }
}

pub(crate) fn thread_handle(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.contains('@') {
        return trimmed.to_ascii_lowercase();
    }
    let mut output = String::new();
    if trimmed.starts_with('+') {
        output.push('+');
    }
    output.push_str(&normalize_handle(trimmed));
    if output == "+" {
        trimmed.to_ascii_lowercase()
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_contract<T: ChannelContract>() {}

    fn imessage() -> Channel {
        Channel::IMessage(IMessageChannel {
            poller: IMessagePoller::new("fake".to_string()),
            sender: IMessageSender::new(),
            self_set: [("me@icloud.com".to_string(), "me@icloud.com".to_string())]
                .into_iter()
                .collect(),
            allow_set: [("15551234567".to_string(), "+15551234567".to_string())]
                .into_iter()
                .collect(),
            reply_marker: REPLY_MARKER.to_string(),
        })
    }

    fn telegram() -> Channel {
        Channel::Telegram(Telegram::new("secret".to_string(), vec![7], vec![9]))
    }

    fn imessage_message(chat: &str, handle: &str, is_from_me: bool) -> RawMessage {
        RawMessage {
            row_id: 1,
            channel: "imessage",
            handle: handle.to_string(),
            chat_identifier: chat.to_string(),
            is_group: false,
            text: "hello".to_string(),
            voice: None,
            is_from_me,
            is_supported: true,
            thread_id: None,
        }
    }

    fn simplex_message(contact_id: i64, is_group: bool, text: &str) -> RawMessage {
        RawMessage {
            row_id: 1,
            channel: "simplex",
            handle: contact_id.to_string(),
            chat_identifier: contact_id.to_string(),
            is_group,
            text: text.to_string(),
            voice: None,
            is_from_me: false,
            is_supported: true,
            thread_id: None,
        }
    }

    fn telegram_message(user: i64, chat: i64, is_group: bool) -> RawMessage {
        RawMessage {
            row_id: 1,
            channel: "telegram",
            handle: user.to_string(),
            chat_identifier: chat.to_string(),
            is_group,
            text: "hello".to_string(),
            voice: None,
            is_from_me: false,
            is_supported: true,
            thread_id: None,
        }
    }

    #[test]
    fn all_three_channels_implement_the_static_contract() {
        assert_contract::<IMessageChannel>();
        assert_contract::<Telegram>();
        assert_contract::<Simplex>();
    }

    #[test]
    fn imessage_contract_keeps_stable_identity_and_delivery_routing() {
        let channel = imessage();
        let message = imessage_message("+1 (555) 123-4567", "+1 (555) 123-4567", false);

        let accepted = channel.accept(&message).unwrap();
        assert_eq!(channel.accept(&message), Some(accepted.clone()));
        assert_eq!(accepted.0, "imessage:dm:+15551234567");
        assert_eq!(accepted.1, "+1 (555) 123-4567");
        assert_eq!(
            channel.route_thread_groups(&accepted.0),
            [vec![
                "imessage:dm:+15551234567".to_string(),
                "dm:+15551234567".to_string(),
            ]]
        );
        assert_eq!(
            channel.approval_origin(&message, &accepted.0),
            AnswerOrigin {
                channel: "imessage".to_string(),
                thread_key: accepted.0,
                sender_key: "15551234567".to_string(),
                chat_key: "15551234567".to_string(),
            }
        );
        assert_eq!(
            channel.outbound_chunks("reply", REPLY_MARKER),
            [OutboundChunk {
                text: format!("reply{REPLY_MARKER}"),
                rich_markdown: false,
            }]
        );
    }

    #[test]
    fn telegram_contract_keeps_topic_identity_and_delivery_routing() {
        let channel = telegram();
        let mut message = telegram_message(7, 7, false);
        message.thread_id = Some(99);

        let accepted = channel.accept(&message).unwrap();
        assert_eq!(channel.accept(&message), Some(accepted.clone()));
        assert_eq!(accepted.0, "telegram:dm:7:topic:99");
        assert_eq!(accepted.1, "7:99");
        assert_eq!(
            channel.route_thread_groups(&accepted.0),
            [
                vec!["telegram:dm:7:topic:99".to_string()],
                vec!["telegram:dm:7".to_string()],
            ]
        );
        assert_eq!(
            channel.approval_origin(&message, &accepted.0),
            AnswerOrigin {
                channel: "telegram".to_string(),
                thread_key: accepted.0,
                sender_key: "7".to_string(),
                chat_key: "7".to_string(),
            }
        );
        assert_eq!(
            channel.outbound_chunks("reply", REPLY_MARKER),
            [OutboundChunk {
                text: "reply".to_string(),
                rich_markdown: true,
            }]
        );
    }

    #[test]
    fn telegram_accepts_allowlisted_private_user_or_chat() {
        assert_eq!(telegram().typing_refresh(), Some(Duration::from_secs(4)));
        assert_eq!(
            telegram().accept(&telegram_message(7, 7, false)),
            Some(("telegram:dm:7".to_string(), "7".to_string()))
        );
        assert_eq!(
            telegram().accept(&telegram_message(8, 9, false)),
            Some(("telegram:dm:9".to_string(), "9".to_string()))
        );
    }

    #[test]
    fn telegram_topic_message_gets_its_own_thread_key_and_target() {
        let mut message = telegram_message(7, 7, false);
        message.thread_id = Some(99);
        assert_eq!(
            telegram().accept(&message),
            Some(("telegram:dm:7:topic:99".to_string(), "7:99".to_string()))
        );
    }

    #[test]
    fn telegram_rejects_unallowlisted_and_group_messages() {
        let channel = telegram();
        assert_eq!(channel.accept(&telegram_message(8, 8, false)), None);
        assert_eq!(channel.accept(&telegram_message(7, -10, true)), None);
        assert_eq!(
            channel.reject_reason(&telegram_message(7, -10, true)),
            "group_chat"
        );
    }

    #[test]
    fn telegram_omits_imessage_marker_and_reply_never_exceeds_limit() {
        let marker = "\n\n-- sent by push";
        let chunks = telegram().outbound_chunks(&"x".repeat(crate::telegram::TEXT_LIMIT), marker);

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].rich_markdown);
        assert!(chunks.iter().all(|chunk| !chunk.text.contains(marker)));
        assert_eq!(chunks[0].text, "x".repeat(crate::telegram::TEXT_LIMIT));

        let long = telegram().outbound_chunks(&"x".repeat(crate::telegram::TEXT_LIMIT + 1), marker);
        assert_eq!(long.len(), 2);
        assert!(long.iter().all(|chunk| chunk.rich_markdown));
        assert!(long
            .iter()
            .all(|chunk| { chunk.text.encode_utf16().count() <= crate::telegram::TEXT_LIMIT }));
    }

    #[test]
    fn scheduled_telegram_output_uses_persistable_plain_chunk_boundaries() {
        let text = "x".repeat(crate::telegram::TEXT_LIMIT + 1);

        let chunks = telegram().scheduled_outbound_chunks(&text, "ignored");

        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.rich_markdown));
        assert_eq!(
            chunks
                .into_iter()
                .map(|chunk| chunk.text)
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn telegram_keeps_each_rich_send_inside_a_durable_chunk_boundary() {
        let structured = format!(
            "{}\n```rust\nlet value = 1;\n```\n[link](https://example.com)\n| a | b |\n| - | - |\n| 1 | 2 |",
            "x".repeat(crate::telegram::TEXT_LIMIT)
        );
        let rich = telegram().outbound_chunks(&structured, "ignored");

        assert_eq!(rich.len(), 2);
        assert!(rich.iter().all(|chunk| chunk.rich_markdown));
        assert_eq!(
            rich.into_iter().map(|chunk| chunk.text).collect::<String>(),
            structured
        );
    }

    #[test]
    fn imessage_outbound_reply_remains_one_unsplit_message() {
        let channel = imessage();

        assert_eq!(
            channel.outbound_chunks("hello", "\n\n-- sent by push")[0].text,
            "hello\n\n-- sent by push"
        );
    }

    // SimpleX contract tests (§D). `Simplex` requires a live daemon
    // connection to construct at all (no mock `Bot` exists in
    // `simploxide-client`), so these exercise `simplex_accept` and
    // `common_reject_reason` directly rather than through a constructed
    // `Channel::Simplex(...)` the way the iMessage/Telegram tests above do.

    #[test]
    fn simplex_accepts_allowlisted_contact_and_builds_dm_thread_key() {
        let allow = [7i64].into_iter().collect::<std::collections::HashSet<_>>();
        let message = simplex_message(7, false, "hello");

        let accepted = simplex_accept(|id| allow.contains(&id), &message);

        assert_eq!(
            accepted,
            Some(("simplex:dm:7".to_string(), "7".to_string()))
        );
    }

    #[test]
    fn simplex_rejects_unallowlisted_contact() {
        let allow = std::collections::HashSet::<i64>::new();
        let message = simplex_message(7, false, "hello");

        assert_eq!(simplex_accept(|id| allow.contains(&id), &message), None);
        assert_eq!(common_reject_reason(&message), None); // not a common-reject case; the allowlist gate is what rejects it
    }

    #[test]
    fn simplex_rejects_group_messages() {
        let message = simplex_message(7, true, "hello");

        assert_eq!(
            simplex_accept(|_| true, &message),
            None,
            "group messages must be rejected even for an allowlisted contact id"
        );
        assert_eq!(common_reject_reason(&message), Some("group_chat"));
    }

    #[test]
    fn simplex_rejects_empty_text() {
        let message = simplex_message(7, false, "   ");

        assert_eq!(simplex_accept(|_| true, &message), None);
        assert_eq!(common_reject_reason(&message), Some("empty_message"));
    }

    #[test]
    fn simplex_outbound_chunks_never_contain_the_imessage_reply_marker() {
        let marker = "\n\n-- sent by push";
        // `outbound_chunks`'s `marker` parameter is ignored for SimpleX
        // entirely (see the trait impl above) — asserting that here via the
        // pure free function, without needing a live `Simplex` at all.
        let chunks = simplex_outbound_chunks("reply");
        assert!(chunks.iter().all(|chunk| !chunk.text.contains(marker)));
        assert_eq!(chunks[0].text, "reply");
        assert!(!chunks[0].rich_markdown);
    }

    #[test]
    fn simplex_outbound_chunks_empty_text_produces_no_chunks() {
        assert!(simplex_outbound_chunks("   ").is_empty());
    }
}
