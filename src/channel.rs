//! Channel-neutral messaging boundary used by the gateway.

use std::collections::HashMap;

use anyhow::Result;

use crate::config::{ChannelKind, Config};
use crate::imessage::{Poller as IMessagePoller, Sender as IMessageSender};
use crate::telegram::Telegram;

#[derive(Debug, Clone)]
pub struct RawMessage {
    pub row_id: i64,
    pub channel: &'static str,
    pub handle: String,
    pub chat_identifier: String,
    pub is_group: bool,
    pub text: String,
    pub is_from_me: bool,
    pub is_supported: bool,
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
    pub fn new(cfg: &Config) -> Result<Self> {
        match cfg.channel_kind()? {
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
                        is_from_me: message.is_from_me,
                        is_supported: true,
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
        if !message.is_supported || message.is_group || message.text.trim().is_empty() {
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
            Self::Telegram(telegram) => telegram.is_allowed(message).then(|| {
                (
                    format!("telegram:dm:{}", message.chat_identifier),
                    message.chat_identifier.clone(),
                )
            }),
        }
    }

    pub fn reject_reason(&self, message: &RawMessage) -> &'static str {
        if !message.is_supported {
            "unsupported_update"
        } else if message.is_group {
            "group_chat"
        } else if message.text.trim().is_empty() {
            "empty_text"
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

    pub fn outbound_chunks(&self, text: &str, marker: &str) -> Vec<String> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let output = format!("{text}{marker}");
        match self {
            Self::IMessage { .. } => vec![output],
            Self::Telegram(_) => crate::telegram::split_text(&output),
        }
    }

    #[cfg_attr(test, allow(dead_code))]
    pub async fn send_chunk(&self, target: &str, text: &str) -> Result<()> {
        match self {
            Self::IMessage { sender, .. } => sender.send(target, text).await,
            Self::Telegram(telegram) => telegram.send(target, text).await,
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
            is_from_me: false,
            is_supported: true,
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
    fn telegram_marker_and_reply_never_exceed_limit() {
        let chunks = telegram().outbound_chunks(
            &"x".repeat(crate::telegram::TEXT_LIMIT),
            "\n\n-- sent by push",
        );

        assert_eq!(chunks.len(), 2);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.encode_utf16().count() <= crate::telegram::TEXT_LIMIT));
        assert_eq!(
            chunks.concat(),
            format!(
                "{}\n\n-- sent by push",
                "x".repeat(crate::telegram::TEXT_LIMIT)
            )
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
            channel.outbound_chunks("hello", "\n\n-- sent by push"),
            ["hello\n\n-- sent by push"]
        );
    }
}
