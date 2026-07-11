//! Local JSONL audit log for production debugging.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::channel::RawMessage;
use crate::config::AgentBackend;

#[derive(Clone)]
pub struct AuditLog {
    path: String,
    include_content: bool,
    channel: String,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub row_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_from_me: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_group: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_new_session: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AuditContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<AuditContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditContent {
    pub chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl AuditLog {
    pub fn new(path: String, include_content: bool, channel: &str) -> Self {
        Self {
            path,
            include_content,
            channel: channel.to_string(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn record(&self, event: AuditEvent) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        if let Some(parent) = Path::new(&self.path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit log directory {}", parent.display()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open audit log {}", self.path))?;
        serde_json::to_writer(&mut file, &event).context("write audit event")?;
        use std::io::Write;
        writeln!(file).context("finish audit event")?;
        Ok(())
    }

    pub fn inbound(&self, msg: &RawMessage) -> AuditEvent {
        AuditEvent {
            ts_ms: now_ms(),
            event: "message_inbound".to_string(),
            row_id: Some(msg.row_id),
            channel: Some(msg.channel.to_string()),
            thread: None,
            backend: None,
            reason: None,
            target: None,
            handle: Some(msg.handle.clone()),
            chat_identifier: Some(msg.chat_identifier.clone()),
            is_from_me: Some(msg.is_from_me),
            is_group: Some(msg.is_group),
            is_new_session: None,
            message: Some(content(&msg.text, self.include_content)),
            reply: None,
            error: None,
        }
    }

    pub fn ignored(&self, msg: &RawMessage, reason: impl Into<String>) -> AuditEvent {
        let mut event = self.inbound(msg);
        event.event = "message_ignored".to_string();
        event.reason = Some(reason.into());
        event
    }

    pub fn accepted(&self, msg: &RawMessage, thread: &str, backend: AgentBackend) -> AuditEvent {
        let mut event = self.inbound(msg);
        event.event = "message_accepted".to_string();
        event.thread = Some(thread.to_string());
        event.backend = Some(backend.as_str().to_string());
        event
    }

    pub fn backend_started(
        &self,
        row_id: i64,
        thread: &str,
        backend: AgentBackend,
        is_new_session: bool,
    ) -> AuditEvent {
        self.base(
            "backend_run_started",
            Some(row_id),
            Some(thread),
            Some(backend),
        )
        .with_new_session(is_new_session)
    }

    pub fn backend_completed(
        &self,
        row_id: i64,
        thread: &str,
        backend: AgentBackend,
        reply: &str,
    ) -> AuditEvent {
        let mut event = self.base(
            "backend_run_completed",
            Some(row_id),
            Some(thread),
            Some(backend),
        );
        event.reply = Some(content(reply, self.include_content));
        event
    }

    pub fn failed(
        &self,
        event_name: &'static str,
        row_id: i64,
        thread: &str,
        backend: Option<AgentBackend>,
        error: impl Into<String>,
    ) -> AuditEvent {
        let mut event = self.base(event_name, Some(row_id), Some(thread), backend);
        event.error = Some(error.into());
        event
    }

    pub fn reply_sent(
        &self,
        row_id: i64,
        thread: &str,
        target: &str,
        backend: Option<AgentBackend>,
        reply: &str,
    ) -> AuditEvent {
        let mut event = self.base("reply_sent", Some(row_id), Some(thread), backend);
        event.target = Some(target.to_string());
        event.reply = Some(content(reply, self.include_content));
        event
    }

    pub fn reply_failed(
        &self,
        row_id: i64,
        thread: &str,
        target: &str,
        backend: Option<AgentBackend>,
        error: impl Into<String>,
    ) -> AuditEvent {
        let mut event = self.base("reply_failed", Some(row_id), Some(thread), backend);
        event.target = Some(target.to_string());
        event.error = Some(error.into());
        event
    }

    pub fn completed(&self, row_id: i64, reason: impl Into<String>) -> AuditEvent {
        let mut event = self.base("message_completed", Some(row_id), None, None);
        event.reason = Some(reason.into());
        event
    }
    fn base(
        &self,
        event: &'static str,
        row_id: Option<i64>,
        thread: Option<&str>,
        backend: Option<AgentBackend>,
    ) -> AuditEvent {
        AuditEvent {
            ts_ms: now_ms(),
            event: event.to_string(),
            row_id,
            channel: Some(self.channel.clone()),
            thread: thread.map(str::to_string),
            backend: backend.map(|b| b.as_str().to_string()),
            reason: None,
            target: None,
            handle: None,
            chat_identifier: None,
            is_from_me: None,
            is_group: None,
            is_new_session: None,
            message: None,
            reply: None,
            error: None,
        }
    }
}

impl AuditEvent {
    fn with_new_session(mut self, is_new_session: bool) -> Self {
        self.is_new_session = Some(is_new_session);
        self
    }
}

fn content(text: &str, include: bool) -> AuditContent {
    AuditContent {
        chars: text.chars().count(),
        text: include.then(|| text.to_string()),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_path;

    fn msg() -> RawMessage {
        RawMessage {
            row_id: 42,
            channel: "imessage",
            handle: "+15551234567".to_string(),
            chat_identifier: "+15551234567".to_string(),
            is_group: false,
            text: "secret request".to_string(),
            is_from_me: false,
            is_supported: true,
            thread_id: None,
        }
    }

    #[test]
    fn accepted_event_redacts_content_by_default() {
        let audit = AuditLog::new("audit.jsonl".to_string(), false, "imessage");

        let event = audit.accepted(&msg(), "dm:+15551234567", AgentBackend::Claude);

        assert_eq!(event.event, "message_accepted");
        assert_eq!(event.thread.as_deref(), Some("dm:+15551234567"));
        assert_eq!(event.backend.as_deref(), Some("claude"));
        assert_eq!(event.message.unwrap().text, None);
    }

    #[test]
    fn content_logging_is_opt_in() {
        let audit = AuditLog::new("audit.jsonl".to_string(), true, "imessage");

        let event = audit.ignored(&msg(), "not_allowlisted");

        assert_eq!(event.event, "message_ignored");
        assert_eq!(event.reason.as_deref(), Some("not_allowlisted"));
        assert_eq!(
            event.message.unwrap().text.as_deref(),
            Some("secret request")
        );
    }

    #[test]
    fn failed_and_completed_events_include_debug_context() {
        let audit = AuditLog::new("audit.jsonl".to_string(), false, "imessage");

        let failed = audit.failed(
            "backend_run_failed",
            42,
            "dm:+15551234567",
            Some(AgentBackend::Codex),
            "timeout",
        );
        let completed = audit.completed(42, "ignored");

        assert_eq!(failed.backend.as_deref(), Some("codex"));
        assert_eq!(failed.error.as_deref(), Some("timeout"));
        assert_eq!(completed.event, "message_completed");
        assert_eq!(completed.reason.as_deref(), Some("ignored"));
    }

    #[test]
    fn reply_failures_include_backend_context_when_known() {
        let audit = AuditLog::new("audit.jsonl".to_string(), false, "imessage");

        let failed = audit.reply_failed(
            42,
            "dm:+15551234567",
            "+15551234567",
            Some(AgentBackend::Codex),
            "send failed",
        );

        assert_eq!(failed.event, "reply_failed");
        assert_eq!(failed.backend.as_deref(), Some("codex"));
        assert_eq!(failed.target.as_deref(), Some("+15551234567"));
        assert_eq!(failed.error.as_deref(), Some("send failed"));
    }

    #[test]
    fn writes_jsonl_events() {
        let path = temp_path("audit-jsonl");
        let audit = AuditLog::new(path.to_string_lossy().to_string(), false, "imessage");

        audit.record(audit.completed(42, "completed")).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let event: AuditEvent = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(event.event, "message_completed");
        assert_eq!(event.row_id, Some(42));

        let _ = std::fs::remove_file(path);
    }
}
