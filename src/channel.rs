//! Channel-neutral messaging boundary used by the gateway.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::approval::AnswerOrigin;
use crate::config::{ChannelKind, Config};
use crate::imessage::{Poller as IMessagePoller, Sender as IMessageSender};
use crate::missive::Missive;
use crate::slack::{parse_message_target, Slack};
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
    /// Provider-stable identifier used for durable deduplication when it is
    /// different from the channel's monotonic local cursor.
    pub provider_event_id: Option<String>,
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
        self.provider_event_id.as_ref().map_or_else(
            || format!("{}:{}", self.channel, self.row_id),
            |id| format!("{}:{id}", self.channel),
        )
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
    Slack(Slack),
    Missive(Missive),
}

impl Channel {
    pub fn new_for(cfg: &Config, kind: ChannelKind) -> Result<Self> {
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
            ChannelKind::Slack => Ok(Self::Slack(Slack::new(
                cfg.slack_app_token()
                    .ok_or_else(|| anyhow::anyhow!("Slack app token is not configured"))?,
                cfg.slack_bot_token()
                    .ok_or_else(|| anyhow::anyhow!("Slack bot token is not configured"))?,
                cfg.slack_allow_user_ids.clone(),
                &cfg.state_path,
            )?)),
            ChannelKind::Missive => Ok(Self::Missive(Missive::new(
                cfg.missive_token()
                    .ok_or_else(|| anyhow::anyhow!("Missive API token is not configured"))?,
                cfg.missive_conversation_ids.clone(),
                cfg.missive_allow_user_ids.clone(),
                &cfg.state_path,
            )?)),
        }
    }

    pub fn primary_target(&self, configured: &str) -> Result<String> {
        match self {
            Self::IMessage(channel) => ChannelContract::primary_target(channel, configured),
            Self::Telegram(channel) => ChannelContract::primary_target(channel, configured),
            Self::Slack(channel) => ChannelContract::primary_target(channel, configured),
            Self::Missive(channel) => ChannelContract::primary_target(channel, configured),
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::IMessage(channel) => ChannelContract::id(channel),
            Self::Telegram(channel) => ChannelContract::id(channel),
            Self::Slack(channel) => ChannelContract::id(channel),
            Self::Missive(channel) => ChannelContract::id(channel),
        }
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        match self {
            Self::IMessage(channel) => ChannelContract::poll(channel, since).await,
            Self::Telegram(channel) => ChannelContract::poll(channel, since).await,
            Self::Slack(channel) => ChannelContract::poll(channel, since).await,
            Self::Missive(channel) => ChannelContract::poll(channel, since).await,
        }
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        match self {
            Self::IMessage(channel) => ChannelContract::latest_cursor(channel).await,
            Self::Telegram(channel) => ChannelContract::latest_cursor(channel).await,
            Self::Slack(channel) => ChannelContract::latest_cursor(channel).await,
            Self::Missive(channel) => ChannelContract::latest_cursor(channel).await,
        }
    }

    /// Returns `(thread_key, reply_target)` for an accepted message.
    pub fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        match self {
            Self::IMessage(channel) => ChannelContract::accept(channel, message),
            Self::Telegram(channel) => ChannelContract::accept(channel, message),
            Self::Slack(channel) => ChannelContract::accept(channel, message),
            Self::Missive(channel) => ChannelContract::accept(channel, message),
        }
    }

    pub fn reject_reason(&self, message: &RawMessage) -> &'static str {
        match self {
            Self::IMessage(channel) => ChannelContract::reject_reason(channel, message),
            Self::Telegram(channel) => ChannelContract::reject_reason(channel, message),
            Self::Slack(channel) => ChannelContract::reject_reason(channel, message),
            Self::Missive(channel) => ChannelContract::reject_reason(channel, message),
        }
    }

    pub fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        match self {
            Self::IMessage(channel) => ChannelContract::approval_origin(channel, message, thread),
            Self::Telegram(channel) => ChannelContract::approval_origin(channel, message, thread),
            Self::Slack(channel) => ChannelContract::approval_origin(channel, message, thread),
            Self::Missive(channel) => ChannelContract::approval_origin(channel, message, thread),
        }
    }

    pub fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        match self {
            Self::IMessage(channel) => ChannelContract::route_thread_groups(channel, thread),
            Self::Telegram(channel) => ChannelContract::route_thread_groups(channel, thread),
            Self::Slack(channel) => ChannelContract::route_thread_groups(channel, thread),
            Self::Missive(channel) => ChannelContract::route_thread_groups(channel, thread),
        }
    }

    pub fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        match self {
            Self::IMessage(channel) => ChannelContract::outbound_chunks(channel, text, marker),
            Self::Telegram(channel) => ChannelContract::outbound_chunks(channel, text, marker),
            Self::Slack(channel) => ChannelContract::outbound_chunks(channel, text, marker),
            Self::Missive(channel) => ChannelContract::outbound_chunks(channel, text, marker),
        }
    }

    pub fn scheduled_outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        match self {
            Self::IMessage(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
            Self::Telegram(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
            Self::Slack(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
            Self::Missive(channel) => {
                ChannelContract::scheduled_outbound_chunks(channel, text, marker)
            }
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
            Self::Telegram(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
            Self::Slack(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
            Self::Missive(channel) => ChannelContract::send_chunk(channel, target, chunk).await,
        }
    }

    pub fn typing_refresh(&self) -> Option<Duration> {
        match self {
            Self::IMessage(channel) => ChannelContract::typing_refresh(channel),
            Self::Telegram(channel) => ChannelContract::typing_refresh(channel),
            Self::Slack(channel) => ChannelContract::typing_refresh(channel),
            Self::Missive(channel) => ChannelContract::typing_refresh(channel),
        }
    }

    pub async fn send_typing(&self, target: &str) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_typing(channel, target).await,
            Self::Telegram(channel) => ChannelContract::send_typing(channel, target).await,
            Self::Slack(channel) => ChannelContract::send_typing(channel, target).await,
            Self::Missive(channel) => ChannelContract::send_typing(channel, target).await,
        }
    }

    pub async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        match self {
            Self::IMessage(channel) => ChannelContract::download_voice(channel, voice).await,
            Self::Telegram(channel) => ChannelContract::download_voice(channel, voice).await,
            Self::Slack(channel) => ChannelContract::download_voice(channel, voice).await,
            Self::Missive(channel) => ChannelContract::download_voice(channel, voice).await,
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()> {
        match self {
            Self::IMessage(channel) => ChannelContract::send_voice(channel, target, clip).await,
            Self::Telegram(channel) => ChannelContract::send_voice(channel, target, clip).await,
            Self::Slack(channel) => ChannelContract::send_voice(channel, target, clip).await,
            Self::Missive(channel) => ChannelContract::send_voice(channel, target, clip).await,
        }
    }

    pub fn delivery_semantics(&self) -> DeliverySemantics {
        match self {
            Self::IMessage(channel) => ChannelContract::delivery_semantics(channel),
            Self::Telegram(channel) => ChannelContract::delivery_semantics(channel),
            Self::Slack(channel) => ChannelContract::delivery_semantics(channel),
            Self::Missive(channel) => ChannelContract::delivery_semantics(channel),
        }
    }

    pub fn shutdown_semantics(&self) -> ShutdownSemantics {
        match self {
            Self::IMessage(channel) => ChannelContract::shutdown_semantics(channel),
            Self::Telegram(channel) => ChannelContract::shutdown_semantics(channel),
            Self::Slack(channel) => ChannelContract::shutdown_semantics(channel),
            Self::Missive(channel) => ChannelContract::shutdown_semantics(channel),
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
                provider_event_id: None,
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

impl ChannelContract for Slack {
    fn id(&self) -> &'static str {
        "slack"
    }

    fn primary_target(&self, configured: &str) -> Result<String> {
        let user = configured.trim();
        if user.is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        if !self.allows_user(user) {
            bail!("Slack primary user {user:?} is not in slack.allow_user_ids");
        }
        Ok(format!("user:{user}"))
    }

    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.poll(since).await
    }

    async fn latest_cursor(&self) -> Result<i64> {
        self.latest_cursor()
    }

    fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        if message.is_from_me
            || common_reject_reason(message).is_some()
            || !self.allows_user(&message.handle)
        {
            return None;
        }
        let (team, channel, root) = parse_message_target(&message.chat_identifier)?;
        Some((
            format!("slack:dm:{team}:{channel}"),
            format!("{channel}|{root}"),
        ))
    }

    fn reject_reason(&self, message: &RawMessage) -> &'static str {
        if message.is_from_me {
            "bot_message"
        } else if !self.allows_user(&message.handle) {
            "not_allowlisted"
        } else {
            common_reject_reason(message).unwrap_or("unsupported_update")
        }
    }

    fn approval_origin(&self, message: &RawMessage, thread: &str) -> AnswerOrigin {
        let chat_key = parse_message_target(&message.chat_identifier)
            .map(|(_, channel, _)| channel)
            .unwrap_or(&message.chat_identifier);
        AnswerOrigin {
            channel: self.id().to_string(),
            thread_key: thread.to_string(),
            sender_key: message.handle.clone(),
            chat_key: chat_key.to_string(),
        }
    }

    fn route_thread_groups(&self, thread: &str) -> Vec<Vec<String>> {
        vec![vec![thread.to_string()]]
    }

    fn outbound_chunks(&self, text: &str, _marker: &str) -> Vec<OutboundChunk> {
        crate::slack::split_text(text)
            .into_iter()
            .map(|text| OutboundChunk {
                text,
                rich_markdown: true,
            })
            .collect()
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        self.send_message(target, &chunk.text).await
    }

    fn typing_refresh(&self) -> Option<Duration> {
        Some(Duration::from_secs(60))
    }

    async fn send_typing(&self, target: &str) -> Result<()> {
        self.send_status(target).await
    }

    async fn download_voice(&self, _voice: &InboundVoice) -> Result<AudioClip> {
        bail!("Slack voice messages are not supported")
    }

    async fn send_voice(&self, _target: &str, _clip: &AudioClip) -> Result<()> {
        bail!("Slack voice replies are not supported")
    }

    fn delivery_semantics(&self) -> DeliverySemantics {
        DeliverySemantics {
            // Slack's HTTP client bounds each request. A zero gateway timeout
            // lets a server-directed Retry-After sleep finish without an
            // early generic retry.
            send_timeout: Duration::ZERO,
            retry_attempts: 3,
            retry_delay: Duration::from_secs(1),
            exhausted_retry_delay: Duration::from_secs(5),
        }
    }
}

impl ChannelContract for Missive {
    fn id(&self) -> &'static str {
        "missive"
    }

    fn primary_target(&self, configured: &str) -> Result<String> {
        let conversation = configured.trim();
        if conversation.is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        if !self.allows_conversation(conversation) {
            bail!(
                "Missive primary conversation {conversation:?} is not in missive.conversation_ids"
            );
        }
        Ok(conversation.to_string())
    }

    async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        self.poll(since).await
    }

    async fn latest_cursor(&self) -> Result<i64> {
        self.latest_cursor().await
    }

    fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        if message.is_from_me
            || common_reject_reason(message).is_some()
            || !self.allows_user(&message.handle)
            || !self.allows_conversation(&message.chat_identifier)
        {
            return None;
        }
        Some((
            format!("missive:conversation:{}", message.chat_identifier),
            message.chat_identifier.clone(),
        ))
    }

    fn reject_reason(&self, message: &RawMessage) -> &'static str {
        if message.is_from_me {
            "bot_message"
        } else if !self.allows_user(&message.handle)
            || !self.allows_conversation(&message.chat_identifier)
        {
            "not_allowlisted"
        } else {
            common_reject_reason(message).unwrap_or("unsupported_update")
        }
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
        vec![vec![thread.to_string()]]
    }

    fn outbound_chunks(&self, text: &str, _marker: &str) -> Vec<OutboundChunk> {
        crate::missive::split_text(text)
            .into_iter()
            .map(|text| OutboundChunk {
                text,
                rich_markdown: true,
            })
            .collect()
    }

    async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        self.send_message(target, &chunk.text).await
    }

    async fn download_voice(&self, _voice: &InboundVoice) -> Result<AudioClip> {
        bail!("Missive voice messages are not supported")
    }

    async fn send_voice(&self, _target: &str, _clip: &AudioClip) -> Result<()> {
        bail!("Missive voice replies are not supported")
    }

    fn delivery_semantics(&self) -> DeliverySemantics {
        DeliverySemantics {
            send_timeout: Duration::ZERO,
            retry_attempts: 3,
            retry_delay: Duration::from_secs(1),
            exhausted_retry_delay: Duration::from_secs(5),
        }
    }
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

    fn slack() -> Channel {
        let state = std::env::temp_dir()
            .join(format!("push-channel-slack-{}.json", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .to_string();
        Channel::Slack(
            Slack::new(
                "xapp-test".to_string(),
                "xoxb-test".to_string(),
                vec!["U1".to_string()],
                &state,
            )
            .unwrap(),
        )
    }

    fn missive() -> Channel {
        let state = std::env::temp_dir()
            .join(format!(
                "push-channel-missive-{}.json",
                uuid::Uuid::new_v4()
            ))
            .to_string_lossy()
            .to_string();
        Channel::Missive(
            Missive::new(
                "missive-test".to_string(),
                vec!["conversation-1".to_string()],
                vec!["user-1".to_string()],
                &state,
            )
            .unwrap(),
        )
    }

    fn imessage_message(chat: &str, handle: &str, is_from_me: bool) -> RawMessage {
        RawMessage {
            row_id: 1,
            provider_event_id: None,
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

    fn telegram_message(user: i64, chat: i64, is_group: bool) -> RawMessage {
        RawMessage {
            row_id: 1,
            provider_event_id: None,
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
    fn built_in_channels_implement_the_static_contract() {
        assert_contract::<IMessageChannel>();
        assert_contract::<Telegram>();
        assert_contract::<Slack>();
        assert_contract::<Missive>();
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
    fn slack_contract_keeps_workspace_dm_identity_and_origin_thread_target() {
        let channel = slack();
        let message = RawMessage {
            row_id: 1,
            provider_event_id: Some("Ev1".to_string()),
            channel: "slack",
            handle: "U1".to_string(),
            chat_identifier: "T1|D1|123.45".to_string(),
            is_group: false,
            text: "hello".to_string(),
            voice: None,
            is_from_me: false,
            is_supported: true,
            thread_id: None,
        };

        let (thread, target) = channel.accept(&message).unwrap();
        assert_eq!(thread, "slack:dm:T1:D1");
        assert_eq!(target, "D1|123.45");
        assert_eq!(message.event_id(), "slack:Ev1");
        assert_eq!(channel.delivery_semantics().send_timeout, Duration::ZERO);
        assert_eq!(
            channel.approval_origin(&message, &thread),
            AnswerOrigin {
                channel: "slack".to_string(),
                thread_key: thread,
                sender_key: "U1".to_string(),
                chat_key: "D1".to_string(),
            }
        );

        let mut unauthorized = message.clone();
        unauthorized.handle = "U2".to_string();
        assert_eq!(channel.accept(&unauthorized), None);
        assert_eq!(channel.reject_reason(&unauthorized), "not_allowlisted");
    }

    #[test]
    fn missive_contract_keeps_conversation_identity_and_allowlists() {
        let channel = missive();
        let message = RawMessage {
            row_id: 1,
            provider_event_id: Some("comment-1".to_string()),
            channel: "missive",
            handle: "user-1".to_string(),
            chat_identifier: "conversation-1".to_string(),
            is_group: false,
            text: "hello".to_string(),
            voice: None,
            is_from_me: false,
            is_supported: true,
            thread_id: None,
        };

        let (thread, target) = channel.accept(&message).unwrap();
        assert_eq!(thread, "missive:conversation:conversation-1");
        assert_eq!(target, "conversation-1");
        assert_eq!(message.event_id(), "missive:comment-1");
        assert_eq!(channel.primary_target("conversation-1").unwrap(), target);
        assert_eq!(channel.delivery_semantics().send_timeout, Duration::ZERO);
        assert_eq!(
            channel.approval_origin(&message, &thread),
            AnswerOrigin {
                channel: "missive".to_string(),
                thread_key: thread,
                sender_key: "user-1".to_string(),
                chat_key: "conversation-1".to_string(),
            }
        );

        let mut unauthorized = message;
        unauthorized.handle = "user-2".to_string();
        assert_eq!(channel.accept(&unauthorized), None);
        assert_eq!(channel.reject_reason(&unauthorized), "not_allowlisted");
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
}
