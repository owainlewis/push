//! Local JSONL audit log for production debugging.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::channel::RawMessage;
use crate::config::AgentBackend;

#[derive(Clone)]
pub struct AuditLog {
    path: PathBuf,
    include_content: bool,
    channel: String,
    lock: Arc<Mutex<()>>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEvent {
    pub ts_ms: u64,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
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
    pub rehydrated_messages: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<AuditContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<AuditContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditContent {
    pub chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl AuditLog {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(path: impl Into<PathBuf>, include_content: bool, channel: &str) -> Self {
        Self::with_lock(path, include_content, channel, Arc::new(Mutex::new(())))
    }

    pub(crate) fn with_lock(
        path: impl Into<PathBuf>,
        include_content: bool,
        channel: &str,
        lock: Arc<Mutex<()>>,
    ) -> Self {
        Self {
            path: path.into(),
            include_content,
            channel: channel.to_string(),
            lock,
        }
    }

    pub fn record(&self, event: AuditEvent) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let path = self.path.as_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit log directory {}", parent.display()))?;
        }
        let mut encoded = serde_json::to_vec(&event).context("encode audit event")?;
        encoded.push(b'\n');
        let mut options = std::fs::OpenOptions::new();
        options.create(true).read(true).write(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("open audit log {}", self.path.display()))?;
        crate::util::restrict_permissions(path, false)
            .with_context(|| format!("restrict audit log permissions {}", self.path.display()))?;
        repair_incomplete_tail(&mut file).context("repair incomplete audit event")?;
        file.write_all(&encoded).context("write audit event")?;
        file.sync_data().context("sync audit event")?;
        Ok(())
    }

    pub(crate) fn flush_schedule_reviews(&self, ledger: &mut crate::jobs::Ledger) -> Result<usize> {
        let mut written = 0usize;
        loop {
            let events = ledger.pending_schedule_audit_events(100)?;
            if events.is_empty() {
                return Ok(written);
            }
            for event in events {
                self.record(self.schedule_review(&event))?;
                ledger.mark_schedule_audit_logged(event.id, crate::util::now_ms())?;
                written += 1;
            }
        }
    }

    pub fn inbound(&self, msg: &RawMessage) -> AuditEvent {
        AuditEvent {
            ts_ms: now_ms(),
            event: "message_inbound".to_string(),
            event_id: None,
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
            rehydrated_messages: None,
            message: Some(content(&msg.text, self.include_content)),
            reply: None,
            error: None,
            job_name: None,
            revision: None,
            actor: None,
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
        rehydrated_messages: usize,
    ) -> AuditEvent {
        self.base(
            "backend_run_started",
            Some(row_id),
            Some(thread),
            Some(backend),
        )
        .with_new_session(is_new_session)
        .with_rehydrated_messages(rehydrated_messages)
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

    pub fn approval(
        &self,
        event_name: &'static str,
        row_id: i64,
        thread: &str,
        reason: impl Into<String>,
    ) -> AuditEvent {
        let mut event = self.base(event_name, Some(row_id), Some(thread), None);
        event.reason = Some(reason.into());
        event
    }

    pub fn schedule_review(&self, event: &crate::jobs::ScheduleReviewEvent) -> AuditEvent {
        let mut audit = self.base(
            match event.event.as_str() {
                "proposed" => "schedule_review_proposed",
                "approved" => "schedule_review_approved",
                "rejected" => "schedule_review_rejected",
                "invalidated" => "schedule_review_invalidated",
                "activated" => "schedule_review_activated",
                _ => "schedule_review_unknown",
            },
            None,
            None,
            None,
        );
        audit.ts_ms = u64::try_from(event.created_at_ms).unwrap_or_default();
        audit.event_id = Some(format!("job_schedule_event:{}", event.audit_event_id));
        audit.job_name = Some(event.job_name.clone());
        audit.revision = Some(event.content_hash.clone());
        audit.reason = event.reason.clone();
        audit.actor = event.actor.clone();
        audit
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
            event_id: None,
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
            rehydrated_messages: None,
            message: None,
            reply: None,
            error: None,
            job_name: None,
            revision: None,
            actor: None,
        }
    }
}

impl AuditEvent {
    fn with_new_session(mut self, is_new_session: bool) -> Self {
        self.is_new_session = Some(is_new_session);
        self
    }

    fn with_rehydrated_messages(mut self, count: usize) -> Self {
        self.rehydrated_messages = Some(count);
        self
    }
}

fn repair_incomplete_tail(file: &mut std::fs::File) -> std::io::Result<()> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }

    let mut end = len;
    let mut buffer = [0u8; 8 * 1024];
    while end > 0 {
        let start = end.saturating_sub(buffer.len() as u64);
        let count = usize::try_from(end - start).unwrap_or(buffer.len());
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buffer[..count])?;
        if let Some(index) = buffer[..count].iter().rposition(|byte| *byte == b'\n') {
            file.set_len(start + index as u64 + 1)?;
            return Ok(());
        }
        end = start;
    }
    file.set_len(0)?;
    Ok(())
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
            provider_event_id: None,
            channel: "imessage",
            handle: "+15551234567".to_string(),
            chat_identifier: "+15551234567".to_string(),
            is_group: false,
            text: "secret request".to_string(),
            voice: None,
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
    fn schedule_review_events_keep_job_and_revision_context() {
        let audit = AuditLog::new("audit.jsonl".to_string(), false, "scheduler");
        let event = audit.schedule_review(&crate::jobs::ScheduleReviewEvent {
            id: 17,
            audit_event_id: "9ec38fe9-a6c8-4e74-b2cf-d6b8a943188a".to_string(),
            event: "invalidated".to_string(),
            job_name: "daily-review".to_string(),
            content_hash: "abc123".to_string(),
            review_id: "review-id".to_string(),
            actor: Some("scheduler".to_string()),
            reason: Some("job revision changed".to_string()),
            created_at_ms: 1234,
        });

        assert_eq!(event.event, "schedule_review_invalidated");
        assert_eq!(
            event.event_id.as_deref(),
            Some("job_schedule_event:9ec38fe9-a6c8-4e74-b2cf-d6b8a943188a")
        );
        assert_eq!(event.ts_ms, 1234);
        assert_eq!(event.job_name.as_deref(), Some("daily-review"));
        assert_eq!(event.revision.as_deref(), Some("abc123"));
        assert_eq!(event.actor.as_deref(), Some("scheduler"));
        assert_eq!(event.reason.as_deref(), Some("job revision changed"));
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

    #[cfg(unix)]
    #[test]
    fn audit_file_is_private_and_existing_permissions_are_repaired() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("audit-permissions");
        let audit = AuditLog::new(path.to_string_lossy().to_string(), false, "imessage");
        audit.record(audit.completed(1, "created")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        audit.record(audit.completed(2, "repaired")).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let _ = std::fs::remove_file(path);
    }
}
