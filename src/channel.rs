//! Channel-neutral messaging boundary used by the gateway.

use std::collections::HashMap;

use anyhow::{bail, Context, Result};

use crate::config::{ChannelKind, Config};
use crate::imessage::{Poller as IMessagePoller, Sender as IMessageSender};
use crate::telegram::Telegram;
use crate::voice::AudioClip;

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

#[derive(Clone)]
pub enum Channel {
    IMessage {
        poller: IMessagePoller,
        #[cfg_attr(test, allow(dead_code))]
        sender: IMessageSender,
        self_set: HashMap<String, String>,
        allow_set: HashMap<String, String>,
        reply_marker: String,
    },
    Telegram(Telegram),
}

impl Channel {
    pub fn new_for(cfg: &Config, kind: ChannelKind) -> Result<Self> {
        match kind {
            ChannelKind::IMessage => Ok(Self::IMessage {
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
                reply_marker: cfg.reply_marker.clone(),
            }),
            ChannelKind::Telegram => Ok(Self::Telegram(Telegram::new(
                cfg.telegram_token()
                    .ok_or_else(|| anyhow::anyhow!("Telegram bot token is not configured"))?,
                cfg.telegram_allow_user_ids.clone(),
                cfg.telegram_allow_chat_ids.clone(),
            ))),
        }
    }

    pub fn primary_target(&self, configured: &str) -> Result<String> {
        if configured.trim().is_empty() {
            bail!("primary delivery target cannot be empty");
        }
        match self {
            Self::IMessage {
                self_set,
                allow_set,
                ..
            } => {
                let normalized = normalize_handle(configured);
                self_set
                    .get(&normalized)
                    .or_else(|| allow_set.get(&normalized))
                    .cloned()
                    .with_context(|| {
                        format!(
                            "iMessage primary target {configured:?} is not in imessage.self_handles or imessage.allow_from"
                        )
                    })
            }
            Self::Telegram(telegram) => {
                let (chat, topic) = configured
                    .trim()
                    .split_once(':')
                    .map_or((configured.trim(), None), |(chat, topic)| {
                        (chat, Some(topic))
                    });
                let chat_id = chat
                    .parse::<i64>()
                    .with_context(|| format!("invalid Telegram primary chat id {chat:?}"))?;
                if !telegram.allows_target(chat_id) {
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
                        let topic_id = value.parse::<i64>().with_context(|| {
                            format!("invalid Telegram primary topic id {value:?}")
                        })?;
                        if topic_id <= 0 {
                            bail!("Telegram primary topic id must be positive");
                        }
                        Ok(format!("{chat_id}:{topic_id}"))
                    }
                }
            }
        }
    }

    pub fn id(&self) -> &'static str {
        match self {
            Self::IMessage { .. } => "imessage",
            Self::Telegram(_) => "telegram",
        }
    }

    pub async fn poll(&self, since: i64) -> Result<Vec<RawMessage>> {
        match self {
            Self::IMessage { poller, .. } => {
                let poller = poller.clone();
                let messages = tokio::task::spawn_blocking(move || poller.poll(since)).await??;
                Ok(messages
                    .into_iter()
                    .map(|message| RawMessage {
                        row_id: message.row_id,
                        channel: "imessage",
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
            Self::Telegram(telegram) => telegram.poll(since).await,
        }
    }

    pub async fn latest_cursor(&self) -> Result<i64> {
        match self {
            Self::IMessage { poller, .. } => {
                let poller = poller.clone();
                Ok(tokio::task::spawn_blocking(move || poller.max_row_id()).await??)
            }
            Self::Telegram(telegram) => telegram.latest_cursor().await,
        }
    }

    /// Returns `(thread_key, reply_target)` for an accepted message.
    pub fn accept(&self, message: &RawMessage) -> Option<(String, String)> {
        if !message.is_supported
            || message.is_group
            || (message.text.trim().is_empty() && message.voice.is_none())
        {
            return None;
        }
        match self {
            Self::IMessage {
                self_set,
                allow_set,
                reply_marker,
                ..
            } => {
                if !reply_marker.is_empty() && message.text.contains(reply_marker) {
                    return None;
                }
                let chat = normalize_handle(&message.chat_identifier);
                let handle = normalize_handle(&message.handle);
                if let Some(value) = self_set.get(&chat) {
                    return Some((
                        format!("imessage:self:{value}"),
                        message.chat_identifier.clone(),
                    ));
                }
                if !message.is_from_me {
                    if let Some(value) = allow_set.get(&handle) {
                        return Some((format!("imessage:dm:{value}"), message.handle.clone()));
                    }
                }
                None
            }
            Self::Telegram(telegram) => {
                telegram
                    .is_allowed(message)
                    .then(|| match message.thread_id {
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
        }
    }

    pub fn reject_reason(&self, message: &RawMessage) -> &'static str {
        if !message.is_supported {
            "unsupported_update"
        } else if message.is_group {
            "group_chat"
        } else if message.text.trim().is_empty() && message.voice.is_none() {
            "empty_message"
        } else {
            match self {
                Self::IMessage { reply_marker, .. }
                    if !reply_marker.is_empty() && message.text.contains(reply_marker) =>
                {
                    "reply_marker"
                }
                Self::IMessage { .. } if message.is_from_me => "from_me_to_other",
                _ => "not_allowlisted",
            }
        }
    }

    pub fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<OutboundChunk> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        match self {
            Self::IMessage { .. } => vec![OutboundChunk {
                text: format!("{text}{marker}"),
                rich_markdown: false,
            }],
            Self::Telegram(_) if text.chars().count() <= crate::telegram::RICH_TEXT_LIMIT => {
                vec![OutboundChunk {
                    text: text.to_string(),
                    rich_markdown: true,
                }]
            }
            Self::Telegram(_) => crate::telegram::split_text(text)
                .into_iter()
                .map(|text| OutboundChunk {
                    text,
                    rich_markdown: false,
                })
                .collect(),
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_chunk(&self, target: &str, chunk: &OutboundChunk) -> Result<()> {
        match self {
            Self::IMessage { sender, .. } => sender.send(target, &chunk.text).await,
            Self::Telegram(telegram) if chunk.rich_markdown => {
                telegram.send_rich(target, &chunk.text).await
            }
            Self::Telegram(telegram) => telegram.send_plain(target, &chunk.text).await,
        }
    }

    pub fn supports_typing(&self) -> bool {
        matches!(self, Self::Telegram(_))
    }

    pub async fn send_typing(&self, target: &str) -> Result<()> {
        match self {
            Self::IMessage { .. } => Ok(()),
            Self::Telegram(telegram) => telegram.send_typing(target).await,
        }
    }

    pub async fn download_voice(&self, voice: &InboundVoice) -> Result<AudioClip> {
        if let Some(bytes) = &voice.data {
            return Ok(AudioClip {
                bytes: bytes.clone(),
                filename: voice.filename.clone(),
                mime_type: voice.mime_type.clone(),
            });
        }
        match self {
            Self::Telegram(telegram) => telegram.download_voice(voice).await,
            Self::IMessage { .. } => bail!("iMessage voice messages are not supported yet"),
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_voice(&self, target: &str, clip: &AudioClip) -> Result<()> {
        match self {
            Self::Telegram(telegram) => telegram.send_voice(target, clip).await,
            Self::IMessage { .. } => bail!("iMessage voice replies are not supported yet"),
        }
    }
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

    fn telegram() -> Channel {
        Channel::Telegram(Telegram::new("secret".to_string(), vec![7], vec![9]))
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
    fn telegram_accepts_allowlisted_private_user_or_chat() {
        assert!(telegram().supports_typing());
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

        let long =
            telegram().outbound_chunks(&"x".repeat(crate::telegram::RICH_TEXT_LIMIT + 1), marker);
        assert!(long.iter().all(|chunk| !chunk.rich_markdown));
        assert!(long
            .iter()
            .all(|chunk| { chunk.text.encode_utf16().count() <= crate::telegram::TEXT_LIMIT }));
    }

    #[test]
    fn telegram_keeps_markdown_structures_whole_or_falls_back_to_plain_chunks() {
        let structured = format!(
            "{}\n```rust\nlet value = 1;\n```\n[link](https://example.com)\n| a | b |\n| - | - |\n| 1 | 2 |",
            "x".repeat(crate::telegram::TEXT_LIMIT)
        );
        let rich = telegram().outbound_chunks(&structured, "ignored");

        assert_eq!(rich.len(), 1);
        assert!(rich[0].rich_markdown);
        assert_eq!(rich[0].text, structured);

        let oversized = format!(
            "{}\n```rust\nlet value = 1;\n```\n[link](https://example.com)\n| a | b |\n| - | - |",
            "x".repeat(crate::telegram::RICH_TEXT_LIMIT)
        );
        let plain = telegram().outbound_chunks(&oversized, "ignored");

        assert!(plain.len() > 1);
        assert!(plain.iter().all(|chunk| !chunk.rich_markdown));
        assert_eq!(
            plain
                .into_iter()
                .map(|chunk| chunk.text)
                .collect::<String>(),
            oversized
        );
    }

    #[test]
    fn imessage_outbound_reply_remains_one_unsplit_message() {
        let channel = Channel::IMessage {
            poller: IMessagePoller::new("fake".to_string()),
            sender: IMessageSender::new(),
            self_set: HashMap::new(),
            allow_set: HashMap::new(),
            reply_marker: String::new(),
        };

        assert_eq!(
            channel.outbound_chunks("hello", "\n\n-- sent by push")[0].text,
            "hello\n\n-- sent by push"
        );
    }
}
