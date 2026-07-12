//! Canonical SQLite conversation history owned by the gateway.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

#[cfg(test)]
use crate::approval::QuestionState;
use crate::approval::{
    parse_answer, AnswerOrigin, AnswerOutcome, DeliveryStatus as ApprovalDeliveryStatus,
    NormalizedAnswer, Question,
};

const SCHEMA_VERSION: i64 = 5;
const MAX_HISTORY_READ_BYTES: usize = 8 * 1024;
const READ_TRUNCATED: &str = "\n[truncated by push while reading history]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundOrigin {
    Backend,
    Gateway,
}

impl OutboundOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Backend => "backend",
            Self::Gateway => "gateway",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    Pending,
    Delivered,
    Failed,
}

impl DeliveryStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Delivered => "delivered",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "delivered" => Ok(Self::Delivered),
            "failed" => Ok(Self::Failed),
            other => bail!("invalid delivery status {other:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMessage {
    pub id: i64,
    pub content: String,
    pub status: DeliveryStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationRole {
    User,
    Assistant,
}

impl ConversationRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationMessage {
    pub role: ConversationRole,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftProposal {
    pub question_id: String,
    pub name: String,
    pub path: String,
    pub snapshot_hash: String,
    pub contents: String,
    pub proposed_by: String,
    pub approved_by: Option<String>,
    pub status: String,
}

pub struct History {
    path: PathBuf,
    conn: Connection,
}

impl History {
    pub fn open(path: &str) -> Result<Self> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("open conversation database {}", path.display()))?;
        crate::util::restrict_permissions(&path, false)
            .with_context(|| format!("restrict database permissions {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("configure conversation database busy timeout")?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("enable conversation database foreign keys")?;
        migrate(&conn).context("migrate conversation database")?;
        Ok(Self { path, conn })
    }

    pub fn record_inbound(
        &mut self,
        channel: &str,
        thread_key: &str,
        channel_event_id: &str,
        content: &str,
    ) -> Result<i64> {
        let database_path = self.path.display().to_string();
        let tx = self
            .conn
            .transaction()
            .with_context(|| format!("begin inbound transaction in {database_path}"))?;
        let conversation_id = conversation(&tx, channel, thread_key)?;
        tx.execute(
            "INSERT INTO messages (
                conversation_id, direction, origin, content, channel_event_id,
                generation_status, delivery_status
             ) VALUES (?1, 'inbound', 'channel', ?2, ?3, 'received', 'not_applicable')
             ON CONFLICT(channel_event_id) DO NOTHING",
            params![conversation_id, content, channel_event_id],
        )
        .with_context(|| format!("insert inbound message into {database_path}"))?;
        let id = tx
            .query_row(
                "SELECT id FROM messages WHERE channel_event_id = ?1",
                [channel_event_id],
                |row| row.get(0),
            )
            .with_context(|| format!("read canonical inbound message from {database_path}"))?;
        tx.commit()
            .with_context(|| format!("commit inbound message to {database_path}"))?;
        Ok(id)
    }

    pub fn record_outbound(
        &mut self,
        inbound_id: i64,
        origin: OutboundOrigin,
        backend: Option<&str>,
        content: &str,
    ) -> Result<OutboundMessage> {
        let database_path = self.path.display().to_string();
        let tx = self
            .conn
            .transaction()
            .with_context(|| format!("begin outbound transaction in {database_path}"))?;
        tx.execute(
            "INSERT INTO messages (
                conversation_id, direction, origin, content, backend,
                in_reply_to_id, generation_status, delivery_status
             )
             SELECT conversation_id, 'outbound', ?2, ?3, ?4, id, 'completed', 'pending'
             FROM messages WHERE id = ?1 AND direction = 'inbound'
             ON CONFLICT(in_reply_to_id) DO NOTHING",
            params![inbound_id, origin.as_str(), content, backend],
        )
        .with_context(|| format!("insert outbound message into {database_path}"))?;
        let message = outbound_for_query(&tx, inbound_id)?
            .with_context(|| format!("inbound message {inbound_id} does not exist"))?;
        tx.commit()
            .with_context(|| format!("commit outbound message to {database_path}"))?;
        Ok(message)
    }

    pub fn outbound_for(&self, inbound_id: i64) -> Result<Option<OutboundMessage>> {
        outbound_for_query(&self.conn, inbound_id).with_context(|| {
            format!(
                "read outbound for inbound {inbound_id} from {}",
                self.path.display()
            )
        })
    }

    pub fn mark_delivery(&mut self, message_id: i64, status: DeliveryStatus) -> Result<()> {
        if status == DeliveryStatus::Pending {
            bail!("cannot reset outbound delivery to pending");
        }
        let changed = self
            .conn
            .execute(
                "UPDATE messages
                 SET delivery_status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
                 WHERE id = ?1 AND direction = 'outbound'",
                params![message_id, status.as_str()],
            )
            .with_context(|| {
                format!("update outbound delivery status in {}", self.path.display())
            })?;
        if changed != 1 {
            bail!("outbound message {message_id} does not exist");
        }
        Ok(())
    }

    pub fn recent_messages_before(
        &self,
        channel: &str,
        thread_key: &str,
        before_message_id: i64,
        limit: usize,
    ) -> Result<Vec<ConversationMessage>> {
        let mut statement = self.conn.prepare(
            "SELECT CAST(m.direction AS BLOB),
                    substr(CAST(m.content AS BLOB), 1, ?5),
                    length(CAST(m.content AS BLOB)) > ?5
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             WHERE c.channel = ?1
               AND c.thread_key = ?2
               AND (
                   (m.direction = 'inbound' AND m.id < ?3)
                   OR
                   (m.direction = 'outbound'
                    AND m.in_reply_to_id < ?3
                    AND m.delivery_status = 'delivered')
               )
             ORDER BY COALESCE(m.in_reply_to_id, m.id) DESC,
                      CASE m.direction WHEN 'outbound' THEN 1 ELSE 0 END DESC
             LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                channel,
                thread_key,
                before_message_id,
                limit as i64,
                MAX_HISTORY_READ_BYTES as i64
            ],
            |row| {
                let direction: Vec<u8> = row.get(0)?;
                let content: Vec<u8> = row.get(1)?;
                let truncated: bool = row.get(2)?;
                Ok((direction, content, truncated))
            },
        )?;

        let mut messages = Vec::new();
        for row in rows {
            let (direction, content, truncated) = row?;
            let role = match direction.as_slice() {
                b"inbound" => ConversationRole::User,
                b"outbound" => ConversationRole::Assistant,
                _ => continue,
            };
            let mut content = String::from_utf8_lossy(&content).into_owned();
            if truncated {
                content.push_str(READ_TRUNCATED);
            }
            messages.push(ConversationMessage { role, content });
        }
        messages.reverse();
        Ok(messages)
    }

    /// Test-only seeding for the inbound answer-resolution flow; production
    /// questions are created through `create_draft_question`.
    #[cfg(test)]
    pub fn create_question(&mut self, question: &Question, now_ms: i64) -> Result<()> {
        question.validate()?;
        if question.expires_at_ms <= now_ms {
            bail!("approval question expiry must be in the future");
        }
        let choices = serde_json::to_string(&question.choices)?;
        self.conn.execute(
            "INSERT INTO approval_questions (
                id, channel, thread_key, sender_key, chat_key, target,
                prompt, choices_json, expires_at_ms, status, delivery_status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', 'pending')",
            params![
                question.id,
                question.channel,
                question.thread_key,
                question.sender_key,
                question.chat_key,
                question.target,
                question.prompt,
                choices,
                question.expires_at_ms,
            ],
        )?;
        Ok(())
    }

    pub fn create_draft_question(
        &mut self,
        question: &Question,
        proposal: &DraftProposal,
        now_ms: i64,
    ) -> Result<bool> {
        question.validate()?;
        if proposal.question_id != question.id {
            bail!("draft proposal question id does not match approval question");
        }
        if question.expires_at_ms <= now_ms {
            bail!("approval question expiry must be in the future");
        }
        let choices = serde_json::to_string(&question.choices)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO approval_questions (
                id, channel, thread_key, sender_key, chat_key, target,
                prompt, choices_json, expires_at_ms, status, delivery_status
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', 'pending')",
            params![
                question.id,
                question.channel,
                question.thread_key,
                question.sender_key,
                question.chat_key,
                question.target,
                question.prompt,
                choices,
                question.expires_at_ms,
            ],
        )?;
        let inserted = tx.execute(
            "INSERT INTO job_draft_proposals (
                question_id, name, path, snapshot_hash, contents, proposed_by,
                proposed_channel, proposed_thread, proposed_sender, proposed_chat,
                status, proposed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'pending', ?11)
             ON CONFLICT(path, snapshot_hash) DO NOTHING",
            params![
                proposal.question_id,
                proposal.name,
                proposal.path,
                proposal.snapshot_hash,
                proposal.contents,
                proposal.proposed_by,
                question.channel,
                question.thread_key,
                question.sender_key,
                question.chat_key,
                now_ms,
            ],
        )?;
        if inserted == 0 {
            tx.rollback()?;
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    pub fn draft_proposal(&self, question_id: &str) -> Result<Option<DraftProposal>> {
        self.conn
            .query_row(
                "SELECT question_id, name, path, snapshot_hash, contents, proposed_by, approved_by, status
                 FROM job_draft_proposals WHERE question_id = ?1",
                [question_id],
                map_draft_proposal,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn draft_revision(
        &self,
        path: &Path,
        snapshot_hash: &str,
    ) -> Result<Option<DraftProposal>> {
        self.conn
            .query_row(
                "SELECT question_id, name, path, snapshot_hash, contents, proposed_by,
                        approved_by, status
                 FROM job_draft_proposals WHERE path = ?1 AND snapshot_hash = ?2",
                params![path.to_string_lossy(), snapshot_hash],
                map_draft_proposal,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn pending_draft_question(
        &self,
        question_id: &str,
        now_ms: i64,
    ) -> Result<Option<Question>> {
        self.conn
            .query_row(
                "SELECT channel, thread_key, sender_key, chat_key, target, prompt,
                        choices_json, expires_at_ms
                 FROM approval_questions
                 WHERE id = ?1 AND status = 'pending' AND expires_at_ms > ?2
                   AND delivery_status IN ('pending', 'failed')",
                params![question_id, now_ms],
                |row| {
                    let choices: String = row.get(6)?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        choices,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    channel,
                    thread_key,
                    sender_key,
                    chat_key,
                    target,
                    prompt,
                    choices,
                    expires_at_ms,
                )| {
                    Ok(Question {
                        id: question_id.to_string(),
                        channel,
                        thread_key,
                        sender_key,
                        chat_key,
                        target,
                        prompt,
                        choices: serde_json::from_str(&choices)?,
                        expires_at_ms,
                    })
                },
            )
            .transpose()
    }

    pub fn draft_answer(&self, question_id: &str) -> Result<Option<NormalizedAnswer>> {
        let row = self
            .conn
            .query_row(
                "SELECT choices_json, answer_index FROM approval_questions
                 WHERE id = ?1 AND status = 'answered'",
                [question_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((choices, selected_number)) = row else {
            return Ok(None);
        };
        let choices: Vec<crate::approval::Choice> = serde_json::from_str(&choices)?;
        let choice = choices
            .get(selected_number.saturating_sub(1) as usize)
            .context("stored draft approval answer index is invalid")?;
        Ok(Some(NormalizedAnswer {
            correlation_id: question_id.to_string(),
            selected_number: selected_number as usize,
            value: choice.value.clone(),
        }))
    }

    pub fn finish_draft_decision(
        &mut self,
        question_id: &str,
        status: &str,
        approved_by: Option<&str>,
        error: Option<&str>,
        now_ms: i64,
    ) -> Result<()> {
        if !matches!(status, "installed" | "rejected" | "invalidated" | "failed") {
            bail!("invalid draft proposal terminal status {status:?}");
        }
        let tx = self.conn.transaction()?;
        let changed = tx.execute(
            "UPDATE job_draft_proposals
             SET status = ?2, approved_by = ?3, decision_at_ms = ?4, error = ?5
             WHERE question_id = ?1 AND status = 'pending'",
            params![question_id, status, approved_by, now_ms, error],
        )?;
        if changed != 1 {
            bail!("pending draft proposal {question_id:?} does not exist");
        }
        tx.execute(
            "UPDATE approval_questions
             SET status = 'consumed', consumed_at_ms = ?2, updated_at_ms = ?2
             WHERE id = ?1 AND status = 'answered'",
            params![question_id, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn mark_question_delivery(
        &mut self,
        id: &str,
        status: ApprovalDeliveryStatus,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE approval_questions
             SET delivery_status = ?2, updated_at_ms = unixepoch('subsec') * 1000
             WHERE id = ?1",
            params![id, status.as_str()],
        )?;
        if changed != 1 {
            bail!("approval question {id:?} does not exist");
        }
        Ok(())
    }

    pub fn answer_question(
        &mut self,
        origin: &AnswerOrigin,
        text: &str,
        now_ms: i64,
    ) -> Result<AnswerOutcome> {
        let Some(attempt) = parse_answer(text) else {
            return Ok(AnswerOutcome::NotAnAnswer);
        };
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE approval_questions SET status = 'expired', updated_at_ms = ?1
             WHERE status = 'pending' AND expires_at_ms <= ?1",
            [now_ms],
        )?;
        let id = if let Some(id) = attempt.correlation_id {
            id
        } else {
            let mut statement = tx.prepare(
                "SELECT id FROM approval_questions
                 WHERE channel = ?1 AND thread_key = ?2
                   AND sender_key = ?3 AND chat_key = ?4
                   AND status = 'pending'
                 ORDER BY created_at_ms, id",
            )?;
            let ids = statement
                .query_map(
                    params![
                        origin.channel,
                        origin.thread_key,
                        origin.sender_key,
                        origin.chat_key
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            match ids.as_slice() {
                [] => {
                    let recent = tx
                        .query_row(
                            "SELECT id FROM approval_questions
                             WHERE channel = ?1 AND thread_key = ?2
                               AND sender_key = ?3 AND chat_key = ?4
                               AND (
                                   (status IN ('answered', 'consumed', 'cancelled')
                                    AND expires_at_ms >= ?5)
                                   OR
                                   (status = 'expired' AND expires_at_ms >= ?5 - 86400000)
                               )
                             ORDER BY created_at_ms DESC, id DESC LIMIT 1",
                            params![
                                origin.channel,
                                origin.thread_key,
                                origin.sender_key,
                                origin.chat_key,
                                now_ms
                            ],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()?;
                    let Some(id) = recent else {
                        return Ok(AnswerOutcome::NotAnAnswer);
                    };
                    id
                }
                [id] => id.clone(),
                _ => return Ok(AnswerOutcome::Ambiguous),
            }
        };

        let row = tx
            .query_row(
                "SELECT channel, thread_key, sender_key, chat_key, choices_json,
                        expires_at_ms, status
                 FROM approval_questions WHERE id = ?1",
                [&id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, String>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((channel, thread, sender, chat, choices, expires_at, status)) = row else {
            return Ok(AnswerOutcome::Mismatched(id));
        };
        if channel != origin.channel
            || thread != origin.thread_key
            || sender != origin.sender_key
            || chat != origin.chat_key
        {
            return Ok(AnswerOutcome::Mismatched(id));
        }
        if status == "answered" || status == "consumed" {
            return Ok(AnswerOutcome::Duplicate(id));
        }
        if status == "cancelled" {
            return Ok(AnswerOutcome::Cancelled(id));
        }
        if status == "expired" || expires_at <= now_ms {
            tx.execute(
                "UPDATE approval_questions SET status = 'expired', updated_at_ms = ?2
                 WHERE id = ?1 AND status = 'pending'",
                params![id, now_ms],
            )?;
            tx.commit()?;
            return Ok(AnswerOutcome::Expired(id));
        }
        let choices: Vec<crate::approval::Choice> = serde_json::from_str(&choices)?;
        let Some(choice) = attempt
            .selected_number
            .checked_sub(1)
            .and_then(|index| choices.get(index))
        else {
            return Ok(AnswerOutcome::InvalidChoice(id));
        };
        tx.execute(
            "UPDATE approval_questions
             SET status = 'answered', answer_index = ?2, answered_at_ms = ?3,
                 updated_at_ms = ?3
             WHERE id = ?1 AND status = 'pending'",
            params![id, attempt.selected_number as i64, now_ms],
        )?;
        tx.commit()?;
        Ok(AnswerOutcome::Selected(NormalizedAnswer {
            correlation_id: id,
            selected_number: attempt.selected_number,
            value: choice.value.clone(),
        }))
    }

    #[cfg(test)]
    pub fn take_answer(&mut self, id: &str, now_ms: i64) -> Result<Option<NormalizedAnswer>> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE approval_questions SET status = 'expired', updated_at_ms = ?2
             WHERE id = ?1 AND status = 'pending' AND expires_at_ms <= ?2",
            params![id, now_ms],
        )?;
        let row = tx
            .query_row(
                "SELECT choices_json, answer_index FROM approval_questions
                 WHERE id = ?1 AND status = 'answered'",
                [id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((choices, selected_number)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        let choices: Vec<crate::approval::Choice> = serde_json::from_str(&choices)?;
        let choice = choices
            .get(selected_number.saturating_sub(1) as usize)
            .context("stored approval answer index is invalid")?;
        tx.execute(
            "UPDATE approval_questions
             SET status = 'consumed', consumed_at_ms = ?2, updated_at_ms = ?2
             WHERE id = ?1 AND status = 'answered'",
            params![id, now_ms],
        )?;
        tx.commit()?;
        Ok(Some(NormalizedAnswer {
            correlation_id: id.to_string(),
            selected_number: selected_number as usize,
            value: choice.value.clone(),
        }))
    }

    #[cfg(test)]
    pub fn question_state(&mut self, id: &str, now_ms: i64) -> Result<Option<QuestionState>> {
        self.conn.execute(
            "UPDATE approval_questions SET status = 'expired', updated_at_ms = ?2
             WHERE id = ?1 AND status = 'pending' AND expires_at_ms <= ?2",
            params![id, now_ms],
        )?;
        self.conn
            .query_row(
                "SELECT status FROM approval_questions WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|status| QuestionState::parse(&status))
            .transpose()
    }

    #[cfg(test)]
    pub fn cancel_question(&mut self, id: &str, now_ms: i64) -> Result<bool> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "UPDATE approval_questions SET status = 'expired', updated_at_ms = ?2
             WHERE id = ?1 AND status = 'pending' AND expires_at_ms <= ?2",
            params![id, now_ms],
        )?;
        let cancelled = tx.execute(
            "UPDATE approval_questions
             SET status = 'cancelled', updated_at_ms = ?2
             WHERE id = ?1 AND status = 'pending' AND expires_at_ms > ?2",
            params![id, now_ms],
        )? == 1;
        tx.commit()?;
        Ok(cancelled)
    }

    #[cfg(test)]
    pub fn execute_batch_for_test(&self, sql: &str) {
        self.conn.execute_batch(sql).unwrap();
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE;")?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        bail!(
            "conversation database schema {version} is newer than supported version {SCHEMA_VERSION}"
        );
    }
    if version == 0 {
        conn.execute_batch(
            "CREATE TABLE conversations (
                 id INTEGER PRIMARY KEY,
                 channel TEXT NOT NULL,
                 thread_key TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 UNIQUE(channel, thread_key)
             );
             CREATE TABLE messages (
                 id INTEGER PRIMARY KEY,
                 conversation_id INTEGER NOT NULL REFERENCES conversations(id),
                 direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound')),
                 origin TEXT NOT NULL CHECK(origin IN ('channel', 'backend', 'gateway')),
                 content TEXT NOT NULL,
                 backend TEXT,
                 channel_event_id TEXT UNIQUE,
                 in_reply_to_id INTEGER UNIQUE REFERENCES messages(id),
                 generation_status TEXT NOT NULL,
                 delivery_status TEXT NOT NULL,
                 created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 CHECK (
                     (direction = 'inbound'
                      AND origin = 'channel'
                      AND channel_event_id IS NOT NULL
                      AND in_reply_to_id IS NULL
                      AND generation_status = 'received'
                      AND delivery_status = 'not_applicable')
                     OR
                     (direction = 'outbound'
                      AND origin IN ('backend', 'gateway')
                      AND channel_event_id IS NULL
                      AND in_reply_to_id IS NOT NULL
                      AND generation_status = 'completed'
                      AND delivery_status IN ('pending', 'delivered', 'failed'))
                 )
             );
             CREATE INDEX messages_conversation_id_idx
                 ON messages(conversation_id, id);
             PRAGMA user_version = 1;",
        )?;
    }
    if version <= 1 {
        conn.execute_batch(
            "CREATE TABLE approval_questions (
                 id TEXT PRIMARY KEY,
                 channel TEXT NOT NULL,
                 thread_key TEXT NOT NULL,
                 sender_key TEXT NOT NULL,
                 chat_key TEXT NOT NULL,
                 target TEXT NOT NULL,
                 prompt TEXT NOT NULL,
                 choices_json TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 status TEXT NOT NULL CHECK(status IN (
                     'pending', 'answered', 'consumed', 'expired', 'cancelled'
                 )),
                 delivery_status TEXT NOT NULL CHECK(delivery_status IN (
                     'pending', 'delivered', 'failed'
                 )),
                 answer_index INTEGER,
                 answered_at_ms INTEGER,
                 consumed_at_ms INTEGER,
                 created_at_ms INTEGER NOT NULL DEFAULT (
                     CAST(strftime('%s', 'now') AS INTEGER) * 1000
                 ),
                 updated_at_ms INTEGER NOT NULL DEFAULT (
                     CAST(strftime('%s', 'now') AS INTEGER) * 1000
                 )
             );
             CREATE INDEX approval_questions_origin_idx ON approval_questions (
                 channel, thread_key, sender_key, chat_key, status, expires_at_ms
             );
             PRAGMA user_version = 2;",
        )?;
    }
    if version <= 2 {
        conn.execute_batch(
            "CREATE TABLE job_runs (
                 id TEXT PRIMARY KEY,
                 job_name TEXT NOT NULL,
                 snapshot_hash TEXT NOT NULL,
                 trigger_kind TEXT NOT NULL,
                 trigger_id TEXT,
                 owner_kind TEXT NOT NULL,
                 scheduled_at_ms INTEGER,
                 queued_at_ms INTEGER NOT NULL,
                 started_at_ms INTEGER,
                 finished_at_ms INTEGER,
                 backend TEXT NOT NULL,
                 permission_profile TEXT NOT NULL,
                 timeout_ms INTEGER NOT NULL,
                 workdir TEXT NOT NULL,
                 state TEXT NOT NULL CHECK(state IN (
                     'queued', 'running', 'succeeded', 'failed', 'timed_out',
                     'skipped_overlap', 'cancelled'
                 )),
                 result TEXT,
                 error TEXT,
                 delivery_state TEXT NOT NULL DEFAULT 'not_requested' CHECK(
                     delivery_state IN ('not_requested', 'pending', 'delivered', 'failed')
                 ),
                 delivery_attempts INTEGER NOT NULL DEFAULT 0,
                 delivery_last_attempt_ms INTEGER,
                 delivery_error TEXT
             );
             CREATE INDEX job_runs_name_time_idx
                 ON job_runs(job_name, queued_at_ms DESC);
             CREATE UNIQUE INDEX job_runs_one_active_per_job
                 ON job_runs(job_name) WHERE state IN ('queued', 'running');
             PRAGMA user_version = 3;",
        )?;
    }
    if version <= 3 {
        conn.execute_batch(
            "ALTER TABLE job_runs ADD COLUMN delivery_channel TEXT;
             ALTER TABLE job_runs ADD COLUMN delivery_target TEXT;
             CREATE UNIQUE INDEX job_runs_scheduled_occurrence
                 ON job_runs(job_name, trigger_id, scheduled_at_ms)
                 WHERE trigger_kind = 'cron';
             PRAGMA user_version = 4;",
        )?;
    }
    if version <= 4 {
        conn.execute_batch(
            "CREATE TABLE job_draft_proposals (
                 question_id TEXT PRIMARY KEY REFERENCES approval_questions(id),
                 name TEXT NOT NULL,
                 path TEXT NOT NULL,
                 snapshot_hash TEXT NOT NULL,
                 contents TEXT NOT NULL,
                 proposed_by TEXT NOT NULL,
                 proposed_channel TEXT NOT NULL,
                 proposed_thread TEXT NOT NULL,
                 proposed_sender TEXT NOT NULL,
                 proposed_chat TEXT NOT NULL,
                 approved_by TEXT,
                 status TEXT NOT NULL CHECK(status IN (
                     'pending', 'installed', 'rejected', 'invalidated', 'failed'
                 )),
                 proposed_at_ms INTEGER NOT NULL,
                 decision_at_ms INTEGER,
                 error TEXT,
                 UNIQUE(path, snapshot_hash)
             );
             CREATE INDEX job_draft_proposals_status_idx
                 ON job_draft_proposals(status, proposed_at_ms);
             PRAGMA user_version = 5;",
        )?;
    }
    conn.execute_batch("COMMIT;")?;
    Ok(())
}

fn map_draft_proposal(row: &rusqlite::Row<'_>) -> rusqlite::Result<DraftProposal> {
    Ok(DraftProposal {
        question_id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        snapshot_hash: row.get(3)?,
        contents: row.get(4)?,
        proposed_by: row.get(5)?,
        approved_by: row.get(6)?,
        status: row.get(7)?,
    })
}

fn conversation(tx: &Transaction<'_>, channel: &str, thread_key: &str) -> Result<i64> {
    tx.execute(
        "INSERT INTO conversations (channel, thread_key) VALUES (?1, ?2)
         ON CONFLICT(channel, thread_key) DO UPDATE SET
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
        params![channel, thread_key],
    )?;
    tx.query_row(
        "SELECT id FROM conversations WHERE channel = ?1 AND thread_key = ?2",
        params![channel, thread_key],
        |row| row.get(0),
    )
    .context("read conversation")
}

fn outbound_for_query(conn: &Connection, inbound_id: i64) -> Result<Option<OutboundMessage>> {
    conn.query_row(
        "SELECT id, content, delivery_status
             FROM messages WHERE in_reply_to_id = ?1",
        [inbound_id],
        |row| {
            let status: String = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, status))
        },
    )
    .optional()?
    .map(|(id, content, status)| {
        Ok(OutboundMessage {
            id,
            content,
            status: DeliveryStatus::parse(&status)?,
        })
    })
    .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{Choice, Question};
    use crate::test_support::temp_path;

    fn question(expires_at_ms: i64) -> Question {
        Question::new(
            AnswerOrigin {
                channel: "telegram".to_string(),
                thread_key: "telegram:dm:7:topic:9".to_string(),
                sender_key: "7".to_string(),
                chat_key: "7".to_string(),
            },
            "7:9",
            "Apply the draft?",
            vec![
                Choice {
                    label: "Approve".to_string(),
                    value: "approve".to_string(),
                },
                Choice {
                    label: "Reject".to_string(),
                    value: "reject".to_string(),
                },
            ],
            expires_at_ms,
        )
        .unwrap()
    }

    fn origin() -> AnswerOrigin {
        AnswerOrigin {
            channel: "telegram".to_string(),
            thread_key: "telegram:dm:7:topic:9".to_string(),
            sender_key: "7".to_string(),
            chat_key: "7".to_string(),
        }
    }

    #[test]
    fn migrates_new_database_and_reopens_it() {
        let path = temp_path("history-migration");

        drop(History::open(path.to_str().unwrap()).unwrap());
        let reopened = History::open(path.to_str().unwrap()).unwrap();

        let version: i64 = reopened
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn migrates_v1_history_database_without_losing_messages() {
        let path = temp_path("approval-v1-migration");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let inbound = history
            .record_inbound("imessage", "imessage:self:me", "imessage:1", "hello")
            .unwrap();
        history.execute_batch_for_test(
            "DROP TABLE job_draft_proposals; DROP TABLE job_runs; DROP TABLE approval_questions; PRAGMA user_version = 1;",
        );
        drop(history);

        let mut reopened = History::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        assert!(reopened.outbound_for(inbound).unwrap().is_none());
        let question = question(2_000);
        reopened.create_question(&question, 1_000).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn migrates_v2_approval_database_and_preserves_pending_question() {
        let path = temp_path("jobs-v2-migration");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let question = question(2_000);
        history.create_question(&question, 1_000).unwrap();
        history.execute_batch_for_test(
            "DROP TABLE job_draft_proposals; DROP TABLE job_runs; PRAGMA user_version = 2;",
        );
        drop(history);

        let mut reopened = History::open(path.to_str().unwrap()).unwrap();

        assert_eq!(
            reopened.question_state(&question.id, 1_100).unwrap(),
            Some(QuestionState::Pending)
        );
        assert_eq!(
            reopened
                .conn
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
        let run_table: i64 = reopened
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'job_runs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_table, 1);
        let delivery_columns: i64 = reopened
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('job_runs')
                 WHERE name IN ('delivery_channel', 'delivery_target')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(delivery_columns, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pending_question_survives_restart_and_answer_is_consumed_once() {
        let path = temp_path("approval-restart");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let question = question(2_000);
        history.create_question(&question, 1_000).unwrap();
        drop(history);

        let mut reopened = History::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened.answer_question(&origin(), "1", 1_100).unwrap(),
            AnswerOutcome::Selected(NormalizedAnswer {
                correlation_id: question.id.clone(),
                selected_number: 1,
                value: "approve".to_string(),
            })
        );
        assert_eq!(
            reopened.take_answer(&question.id, 1_200).unwrap(),
            Some(NormalizedAnswer {
                correlation_id: question.id.clone(),
                selected_number: 1,
                value: "approve".to_string(),
            })
        );
        assert_eq!(reopened.take_answer(&question.id, 1_300).unwrap(), None);
        assert_eq!(
            reopened
                .answer_question(&origin(), &format!("{} 1", question.id), 1_400)
                .unwrap(),
            AnswerOutcome::Duplicate(question.id.clone())
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn question_rejects_mismatch_expiry_invalid_choice_and_cancellation() {
        let path = temp_path("approval-rejections");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let expired = question(2_000);
        history.create_question(&expired, 1_000).unwrap();
        let mut wrong_topic = origin();
        wrong_topic.thread_key = "telegram:dm:7".to_string();
        assert_eq!(
            history
                .answer_question(&wrong_topic, &format!("{} 1", expired.id), 1_100)
                .unwrap(),
            AnswerOutcome::Mismatched(expired.id.clone())
        );
        assert_eq!(
            history
                .answer_question(&origin(), &format!("{} 3", expired.id), 1_200)
                .unwrap(),
            AnswerOutcome::InvalidChoice(expired.id.clone())
        );
        assert_eq!(
            history
                .answer_question(&origin(), &format!("{} 1", expired.id), 2_000)
                .unwrap(),
            AnswerOutcome::Expired(expired.id.clone())
        );
        assert_eq!(
            history.question_state(&expired.id, 2_001).unwrap(),
            Some(QuestionState::Expired)
        );

        let cancelled = question(4_000);
        history.create_question(&cancelled, 2_100).unwrap();
        assert!(history.cancel_question(&cancelled.id, 2_200).unwrap());
        assert!(!history.cancel_question(&cancelled.id, 2_300).unwrap());
        assert_eq!(
            history
                .answer_question(&origin(), &format!("{} 1", cancelled.id), 2_400)
                .unwrap(),
            AnswerOutcome::Cancelled(cancelled.id.clone())
        );
        assert_eq!(
            history.question_state(&cancelled.id, 2_500).unwrap(),
            Some(QuestionState::Cancelled)
        );

        let timed_out = question(3_000);
        history.create_question(&timed_out, 2_600).unwrap();
        assert!(!history.cancel_question(&timed_out.id, 3_100).unwrap());
        assert_eq!(
            history.question_state(&timed_out.id, 3_100).unwrap(),
            Some(QuestionState::Expired)
        );

        let stale = question(4_000);
        history.create_question(&stale, 3_200).unwrap();
        let live = question(6_000);
        history.create_question(&live, 3_300).unwrap();
        assert_eq!(
            history.answer_question(&origin(), "1", 5_000).unwrap(),
            AnswerOutcome::Selected(NormalizedAnswer {
                correlation_id: live.id,
                selected_number: 1,
                value: "approve".to_string(),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn retries_one_channel_event_without_duplicate_user_turn() {
        let path = temp_path("history-retry");
        let mut history = History::open(path.to_str().unwrap()).unwrap();

        let first = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:101", "hello")
            .unwrap();
        let retry = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:101", "hello")
            .unwrap();

        assert_eq!(first, retry);
        let count: i64 = history
            .conn
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generated_reply_is_unique_and_delivery_survives_restart() {
        let path = temp_path("history-crash-boundary");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let inbound = history
            .record_inbound("imessage", "imessage:self:me", "imessage:4", "hello")
            .unwrap();

        let first = history
            .record_outbound(inbound, OutboundOrigin::Backend, Some("claude"), "first")
            .unwrap();
        let duplicate = history
            .record_outbound(inbound, OutboundOrigin::Backend, Some("claude"), "second")
            .unwrap();
        assert_eq!(first, duplicate);
        assert_eq!(duplicate.content, "first");
        history
            .mark_delivery(first.id, DeliveryStatus::Delivered)
            .unwrap();
        drop(history);

        let reopened = History::open(path.to_str().unwrap()).unwrap();
        assert_eq!(
            reopened.outbound_for(inbound).unwrap().unwrap().status,
            DeliveryStatus::Delivered
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn recent_messages_are_bounded_to_one_channel_and_thread() {
        let path = temp_path("history-rehydration-isolation");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let first = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:1", "first")
            .unwrap();
        history
            .record_inbound("telegram", "telegram:dm:7:topic:9", "telegram:2", "topic")
            .unwrap();
        history
            .record_inbound("imessage", "telegram:dm:7", "imessage:3", "other channel")
            .unwrap();
        let current = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:4", "current")
            .unwrap();
        // Gateway polling may persist several inbound messages before the
        // per-thread worker generates the earlier reply. Rehydration still
        // orders that reply with the inbound turn it answers.
        let reply = history
            .record_outbound(first, OutboundOrigin::Backend, Some("codex"), "reply")
            .unwrap();
        history
            .mark_delivery(reply.id, DeliveryStatus::Delivered)
            .unwrap();

        assert_eq!(
            history
                .recent_messages_before("telegram", "telegram:dm:7", current, 20)
                .unwrap(),
            [
                ConversationMessage {
                    role: ConversationRole::User,
                    content: "first".to_string(),
                },
                ConversationMessage {
                    role: ConversationRole::Assistant,
                    content: "reply".to_string(),
                },
            ]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn malformed_utf8_history_is_replaced_without_failing_the_read() {
        let path = temp_path("history-rehydration-malformed");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let prior = history
            .record_inbound("imessage", "imessage:self:me", "imessage:1", "valid")
            .unwrap();
        history
            .conn
            .execute(
                "UPDATE messages SET content = CAST(x'666F80' AS TEXT) WHERE id = ?1",
                [prior],
            )
            .unwrap();
        let current = history
            .record_inbound("imessage", "imessage:self:me", "imessage:2", "current")
            .unwrap();

        let messages = history
            .recent_messages_before("imessage", "imessage:self:me", current, 20)
            .unwrap();

        assert_eq!(messages[0].content, "fo�");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn oversized_history_is_bounded_during_the_database_read() {
        let path = temp_path("history-rehydration-read-bound");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        history
            .record_inbound(
                "imessage",
                "imessage:self:me",
                "imessage:1",
                &"x".repeat(MAX_HISTORY_READ_BYTES * 100),
            )
            .unwrap();
        let current = history
            .record_inbound("imessage", "imessage:self:me", "imessage:2", "current")
            .unwrap();

        let messages = history
            .recent_messages_before("imessage", "imessage:self:me", current, 20)
            .unwrap();

        assert!(messages[0].content.len() <= MAX_HISTORY_READ_BYTES + READ_TRUNCATED.len());
        assert!(messages[0].content.ends_with(READ_TRUNCATED));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn same_thread_text_isolated_by_channel() {
        let path = temp_path("history-channel-isolation");
        let mut history = History::open(path.to_str().unwrap()).unwrap();

        history
            .record_inbound("imessage", "dm:7", "imessage:1", "one")
            .unwrap();
        history
            .record_inbound("telegram", "dm:7", "telegram:1", "two")
            .unwrap();

        let count: i64 = history
            .conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn gateway_and_backend_outbound_origins_are_distinct() {
        let path = temp_path("history-origins");
        let mut history = History::open(path.to_str().unwrap()).unwrap();
        let backend_inbound = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:1", "one")
            .unwrap();
        let gateway_inbound = history
            .record_inbound("telegram", "telegram:dm:7", "telegram:2", "/help")
            .unwrap();

        history
            .record_outbound(
                backend_inbound,
                OutboundOrigin::Backend,
                Some("codex"),
                "backend reply",
            )
            .unwrap();
        history
            .record_outbound(
                gateway_inbound,
                OutboundOrigin::Gateway,
                Some("codex"),
                "command reply",
            )
            .unwrap();

        let rows: Vec<(String, Option<String>, String, String)> = history
            .conn
            .prepare(
                "SELECT origin, backend, generation_status, delivery_status
                 FROM messages WHERE direction = 'outbound' ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            [
                (
                    "backend".to_string(),
                    Some("codex".to_string()),
                    "completed".to_string(),
                    "pending".to_string(),
                ),
                (
                    "gateway".to_string(),
                    Some("codex".to_string()),
                    "completed".to_string(),
                    "pending".to_string(),
                ),
            ]
        );
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn database_permissions_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("history-permissions");
        let _history = History::open(path.to_str().unwrap()).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = std::fs::remove_file(path);
    }
}
