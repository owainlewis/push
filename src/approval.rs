//! Channel-neutral, durable user questions and normalized answers.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_CHOICES: usize = 9;
const MAX_PROMPT_CHARS: usize = 2_000;
const MAX_LABEL_CHARS: usize = 256;
const MAX_VALUE_CHARS: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Choice {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: String,
    pub channel: String,
    pub thread_key: String,
    pub sender_key: String,
    pub chat_key: String,
    pub target: String,
    pub prompt: String,
    pub choices: Vec<Choice>,
    pub expires_at_ms: i64,
}

impl Question {
    pub fn new(
        origin: AnswerOrigin,
        target: impl Into<String>,
        prompt: impl Into<String>,
        choices: Vec<Choice>,
        expires_at_ms: i64,
    ) -> Result<Self> {
        let question = Self {
            id: Uuid::new_v4().to_string(),
            channel: origin.channel,
            thread_key: origin.thread_key,
            sender_key: origin.sender_key,
            chat_key: origin.chat_key,
            target: target.into(),
            prompt: prompt.into(),
            choices,
            expires_at_ms,
        };
        question.validate()?;
        Ok(question)
    }

    pub fn validate(&self) -> Result<()> {
        if Uuid::parse_str(&self.id)
            .map(|id| id.to_string() != self.id)
            .unwrap_or(true)
        {
            bail!("approval correlation id must be a UUID");
        }
        if self.channel.trim().is_empty()
            || self.thread_key.trim().is_empty()
            || self.sender_key.trim().is_empty()
            || self.chat_key.trim().is_empty()
            || self.target.trim().is_empty()
            || self.prompt.trim().is_empty()
        {
            bail!("approval question fields cannot be empty");
        }
        if !(2..=MAX_CHOICES).contains(&self.choices.len()) {
            bail!("approval question must contain 2 to {MAX_CHOICES} choices");
        }
        if self.prompt.chars().count() > MAX_PROMPT_CHARS {
            bail!("approval prompt exceeds {MAX_PROMPT_CHARS} characters");
        }
        if self.choices.iter().any(|choice| {
            choice.label.trim().is_empty()
                || choice.value.trim().is_empty()
                || choice.label.chars().count() > MAX_LABEL_CHARS
                || choice.value.chars().count() > MAX_VALUE_CHARS
        }) {
            bail!("approval choice labels and values must be non-empty and bounded");
        }
        Ok(())
    }

    pub fn render_text(&self) -> String {
        let mut text = format!("{}\n", self.prompt.trim());
        for (index, choice) in self.choices.iter().enumerate() {
            text.push_str(&format!("\n{}. {}", index + 1, choice.label.trim()));
        }
        text.push_str(&format!(
            "\n\nReply with a number, or `{} <number>`. Expires automatically.",
            self.id
        ));
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedAnswer {
    pub correlation_id: String,
    pub selected_number: usize,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerAttempt {
    pub correlation_id: Option<String>,
    pub selected_number: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerOrigin {
    pub channel: String,
    pub thread_key: String,
    pub sender_key: String,
    pub chat_key: String,
}

pub fn parse_answer(text: &str) -> Option<AnswerAttempt> {
    let parts = text.split_whitespace().collect::<Vec<_>>();
    if let Some(correlation_id) = parts.first().and_then(|value| Uuid::parse_str(value).ok()) {
        let selected_number = match parts.as_slice() {
            [_, number] => number.parse().unwrap_or_default(),
            _ => 0,
        };
        return Some(AnswerAttempt {
            correlation_id: Some(correlation_id.to_string()),
            selected_number,
        });
    }
    match parts.as_slice() {
        [number] => number.parse().ok().map(|selected_number| AnswerAttempt {
            correlation_id: None,
            selected_number,
        }),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerOutcome {
    NotAnAnswer,
    Selected(NormalizedAnswer),
    Expired(String),
    Duplicate(String),
    Cancelled(String),
    Mismatched(String),
    InvalidChoice(String),
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Delivered,
    Failed,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionState {
    Pending,
    Answered,
    Consumed,
    Expired,
    Cancelled,
}

#[cfg(test)]
impl QuestionState {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "answered" => Ok(Self::Answered),
            "consumed" => Ok(Self::Consumed),
            "expired" => Ok(Self::Expired),
            "cancelled" => Ok(Self::Cancelled),
            other => bail!("invalid approval question state {other:?}"),
        }
    }
}

impl DeliveryStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_correlated_numbered_answers() {
        let id = Uuid::new_v4().to_string();
        assert_eq!(
            parse_answer(" 2 "),
            Some(AnswerAttempt {
                correlation_id: None,
                selected_number: 2,
            })
        );
        assert_eq!(
            parse_answer(&format!("{id} 1")),
            Some(AnswerAttempt {
                correlation_id: Some(id.clone()),
                selected_number: 1,
            })
        );
        assert_eq!(parse_answer("yes"), None);
        assert_eq!(
            parse_answer(&format!("{id} junk")),
            Some(AnswerAttempt {
                correlation_id: Some(id.clone()),
                selected_number: 0,
            })
        );
        assert_eq!(
            parse_answer(&format!("{} 2", id.to_uppercase())),
            Some(AnswerAttempt {
                correlation_id: Some(id),
                selected_number: 2,
            })
        );
    }

    #[test]
    fn question_validation_bounds_prompt_choices_and_correlation() {
        let origin = AnswerOrigin {
            channel: "imessage".to_string(),
            thread_key: "imessage:self:me".to_string(),
            sender_key: "me".to_string(),
            chat_key: "me".to_string(),
        };
        let choices = vec![
            Choice {
                label: "Yes".to_string(),
                value: "yes".to_string(),
            },
            Choice {
                label: "No".to_string(),
                value: "no".to_string(),
            },
        ];
        assert!(Question::new(
            origin.clone(),
            "me",
            "x".repeat(MAX_PROMPT_CHARS + 1),
            choices.clone(),
            1,
        )
        .is_err());
        let mut question = Question::new(origin, "me", "Continue?", choices, 1).unwrap();
        question.id = "not-a-uuid".to_string();
        assert!(question.validate().is_err());
    }
}
