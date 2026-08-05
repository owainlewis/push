//! Transactional channel cursors and backend session mappings.
//!
//! New state lives in the canonical SQLite database. On startup, an existing
//! JSON state file is imported once in the same transaction as its migration
//! marker. The source file remains untouched as an operator recovery copy.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::paths::PushPaths;
use crate::util::non_empty_session_id;

#[derive(Deserialize, Clone)]
struct SessionInfo {
    uuid: String,
    #[serde(default)]
    started: bool,
    #[serde(default = "default_backend")]
    backend: String,
}

#[derive(Deserialize, Default)]
struct LegacyState {
    #[serde(default)]
    last_row_id: i64,
    #[serde(default)]
    cursors: HashMap<String, i64>,
    #[serde(default)]
    sessions: HashMap<String, SessionInfo>,
}

/// Store owns transactional cursor and session state in `database_path`.
pub struct Store {
    database_path: PathBuf,
    legacy_state_path: PathBuf,
    conn: Connection,
    #[cfg(test)]
    cursor_save_failures_remaining: usize,
    #[cfg(test)]
    completed_row_save_failures_remaining: usize,
    #[cfg(test)]
    session_save_failures_remaining: usize,
}

impl Store {
    pub fn open(paths: &PushPaths) -> Result<Self> {
        Self::open_inner(&paths.database, &paths.state, false)
    }

    #[cfg(test)]
    pub(crate) fn open_at(
        database_path: impl AsRef<Path>,
        legacy_state_path: impl AsRef<Path>,
    ) -> Result<Self> {
        Self::open_inner(database_path.as_ref(), legacy_state_path.as_ref(), false)
    }

    fn open_inner(
        database_path: &Path,
        legacy_state_path: &Path,
        #[cfg_attr(not(test), allow(unused_variables))] fail_migration_before_commit: bool,
    ) -> Result<Self> {
        // History owns all forward schema migrations for the canonical database.
        drop(
            crate::history::History::open(database_path).with_context(|| {
                format!(
                    "prepare canonical state database {}",
                    database_path.display()
                )
            })?,
        );

        let database_path = database_path.to_path_buf();
        let conn = Connection::open(&database_path)
            .with_context(|| format!("open state database {}", database_path.display()))?;
        crate::util::restrict_permissions(&database_path, false).with_context(|| {
            format!("restrict database permissions {}", database_path.display())
        })?;
        conn.busy_timeout(Duration::from_secs(5))
            .context("configure state database busy timeout")?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("enable state database foreign keys")?;

        let mut store = Self {
            database_path,
            legacy_state_path: legacy_state_path.to_path_buf(),
            conn,
            #[cfg(test)]
            cursor_save_failures_remaining: 0,
            #[cfg(test)]
            completed_row_save_failures_remaining: 0,
            #[cfg(test)]
            session_save_failures_remaining: 0,
        };
        store.migrate_legacy_state(fail_migration_before_commit)?;
        Ok(store)
    }

    #[cfg(test)]
    pub fn last_row(&self) -> i64 {
        self.cursor("imessage").unwrap()
    }

    pub fn has_cursor(&self, channel: &str) -> Result<bool> {
        self.conn
            .query_row(
                "SELECT 1 FROM channel_cursors WHERE channel = ?1",
                [channel],
                |_| Ok(()),
            )
            .optional()
            .with_context(|| {
                format!(
                    "read {channel} cursor existence from {}",
                    self.database_path.display()
                )
            })
            .map(|cursor| cursor.is_some())
    }

    pub fn cursor(&self, channel: &str) -> Result<i64> {
        self.conn
            .query_row(
                "SELECT cursor FROM channel_cursors WHERE channel = ?1",
                [channel],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| {
                format!(
                    "read {channel} cursor from {}",
                    self.database_path.display()
                )
            })
            .map(|cursor| cursor.unwrap_or(0))
    }

    pub fn set_cursor(&mut self, channel: &str, id: i64) -> Result<()> {
        validate_channel(channel)?;
        #[cfg(test)]
        if self.cursor_save_failures_remaining > 0 {
            self.cursor_save_failures_remaining -= 1;
            return Err(anyhow::anyhow!("injected cursor save failure"));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin {channel} cursor transaction in {}",
                    self.database_path.display()
                )
            })?;
        insert_monotonic_cursor(&tx, channel, id).with_context(|| {
            format!(
                "advance {channel} cursor transactionally in {}",
                self.database_path.display()
            )
        })?;
        tx.execute(
            "DELETE FROM channel_completed_rows
             WHERE channel = ?1
               AND row_id <= COALESCE(
                   (SELECT cursor FROM channel_cursors WHERE channel = ?1),
                   0
               )",
            [channel],
        )
        .with_context(|| {
            format!(
                "prune {channel} completed rows in {}",
                self.database_path.display()
            )
        })?;
        tx.execute(
            "DELETE FROM channel_pending_filename_polls
             WHERE channel = ?1
               AND row_id <= COALESCE(
                   (SELECT cursor FROM channel_cursors WHERE channel = ?1),
                   0
               )",
            [channel],
        )
        .with_context(|| {
            format!(
                "prune {channel} pending filename rows in {}",
                self.database_path.display()
            )
        })?;
        tx.commit().with_context(|| {
            format!(
                "commit {channel} cursor transaction in {}",
                self.database_path.display()
            )
        })?;
        Ok(())
    }

    pub fn mark_row_completed(&mut self, channel: &str, row_id: i64) -> Result<()> {
        validate_channel(channel)?;
        #[cfg(test)]
        if self.completed_row_save_failures_remaining > 0 {
            self.completed_row_save_failures_remaining -= 1;
            return Err(anyhow::anyhow!("injected completed row save failure"));
        }
        self.conn
            .execute(
                "INSERT OR IGNORE INTO channel_completed_rows (channel, row_id)
                 VALUES (?1, ?2)",
                params![channel, row_id],
            )
            .with_context(|| {
                format!(
                    "persist {channel} completed row {row_id} in {}",
                    self.database_path.display()
                )
            })?;
        Ok(())
    }

    pub fn completed_rows_after(&self, channel: &str, cursor: i64) -> Result<Vec<i64>> {
        validate_channel(channel)?;
        let mut statement = self
            .conn
            .prepare(
                "SELECT row_id FROM channel_completed_rows
                 WHERE channel = ?1 AND row_id > ?2
                 ORDER BY row_id",
            )
            .with_context(|| {
                format!(
                    "prepare {channel} completed row read from {}",
                    self.database_path.display()
                )
            })?;
        let rows = statement
            .query_map(params![channel, cursor], |row| row.get(0))
            .with_context(|| {
                format!(
                    "read {channel} completed rows from {}",
                    self.database_path.display()
                )
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().with_context(|| {
            format!(
                "decode {channel} completed rows from {}",
                self.database_path.display()
            )
        })
    }

    pub fn should_defer_pending_filename(
        &mut self,
        channel: &str,
        row_id: i64,
        poll_limit: u8,
    ) -> Result<bool> {
        validate_channel(channel)?;
        let exhausted = i64::from(poll_limit) + 1;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin {channel} pending filename transaction in {}",
                    self.database_path.display()
                )
            })?;
        tx.execute(
            "INSERT INTO channel_pending_filename_polls (channel, row_id, polls)
             VALUES (?1, ?2, 1)
             ON CONFLICT(channel, row_id) DO UPDATE SET
                 polls = MIN(channel_pending_filename_polls.polls + 1, ?3),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![channel, row_id, exhausted],
        )
        .with_context(|| {
            format!(
                "advance {channel} pending filename row {row_id} in {}",
                self.database_path.display()
            )
        })?;
        let polls: i64 = tx
            .query_row(
                "SELECT polls FROM channel_pending_filename_polls
                 WHERE channel = ?1 AND row_id = ?2",
                params![channel, row_id],
                |row| row.get(0),
            )
            .with_context(|| {
                format!(
                    "read {channel} pending filename row {row_id} from {}",
                    self.database_path.display()
                )
            })?;
        tx.commit().with_context(|| {
            format!(
                "commit {channel} pending filename transaction in {}",
                self.database_path.display()
            )
        })?;
        Ok(polls <= i64::from(poll_limit))
    }

    pub fn clear_pending_filename(&mut self, channel: &str, row_id: i64) -> Result<()> {
        validate_channel(channel)?;
        self.conn
            .execute(
                "DELETE FROM channel_pending_filename_polls
                 WHERE channel = ?1 AND row_id = ?2",
                params![channel, row_id],
            )
            .with_context(|| {
                format!(
                    "clear {channel} pending filename row {row_id} from {}",
                    self.database_path.display()
                )
            })?;
        Ok(())
    }

    /// Returns the agent session id for a thread, creating one if needed. The
    /// second value is true when the backend has not started that session yet.
    pub fn session_for(
        &mut self,
        thread: &str,
        backend: &str,
        initial_id: String,
    ) -> Result<(String, bool)> {
        validate_backend(backend)?;
        let (channel, thread_key) = split_thread(thread)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin session transaction in {}",
                    self.database_path.display()
                )
            })?;
        let existing = tx
            .query_row(
                "SELECT backend, session_id, started
                 FROM backend_sessions
                 WHERE channel = ?1 AND thread_key = ?2",
                params![channel, thread_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                },
            )
            .optional()
            .with_context(|| format!("read session for {thread:?}"))?;

        if let Some((stored_backend, session_id, started)) = existing {
            if stored_backend == backend && !session_id.trim().is_empty() {
                tx.commit()
                    .with_context(|| format!("commit session read for {thread:?}"))?;
                return Ok((session_id, !started));
            }
        }

        #[cfg(test)]
        if self.session_save_failures_remaining > 0 {
            self.session_save_failures_remaining -= 1;
            return Err(anyhow::anyhow!("injected session save failure"));
        }
        tx.execute(
            "INSERT INTO backend_sessions (
                 channel, thread_key, backend, session_id, started
             ) VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(channel, thread_key) DO UPDATE SET
                 backend = excluded.backend,
                 session_id = excluded.session_id,
                 started = 0,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![channel, thread_key, backend, initial_id],
        )
        .with_context(|| format!("store fresh session for {thread:?}"))?;
        tx.commit()
            .with_context(|| format!("commit fresh session for {thread:?}"))?;
        Ok((initial_id, true))
    }

    pub fn mark_started(&mut self, thread: &str, session_id: Option<&str>) -> Result<()> {
        let (channel, thread_key) = split_thread(thread)?;
        let session_id = session_id.and_then(non_empty_session_id);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin session activation transaction in {}",
                    self.database_path.display()
                )
            })?;
        let current = tx
            .query_row(
                "SELECT session_id, started
                 FROM backend_sessions
                 WHERE channel = ?1 AND thread_key = ?2",
                params![channel, thread_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
            .with_context(|| format!("read session for {thread:?}"))?;
        let Some((current_id, started)) = current else {
            tx.commit()
                .with_context(|| format!("commit missing session read for {thread:?}"))?;
            return Ok(());
        };
        let effective_id = session_id.unwrap_or(&current_id);
        let should_start = !started && !effective_id.trim().is_empty();
        if session_id.is_none() && !should_start {
            tx.commit()
                .with_context(|| format!("commit unchanged session for {thread:?}"))?;
            return Ok(());
        }

        #[cfg(test)]
        if self.session_save_failures_remaining > 0 {
            self.session_save_failures_remaining -= 1;
            return Err(anyhow::anyhow!("injected session save failure"));
        }
        tx.execute(
            "UPDATE backend_sessions
             SET session_id = ?3,
                 started = CASE WHEN ?4 THEN 1 ELSE started END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
             WHERE channel = ?1 AND thread_key = ?2",
            params![channel, thread_key, effective_id, should_start],
        )
        .with_context(|| format!("mark session started for {thread:?}"))?;
        tx.commit()
            .with_context(|| format!("commit session activation for {thread:?}"))?;
        Ok(())
    }

    /// Assigns a fresh backend session to a thread (the `/clear` behavior).
    ///
    /// Returns the previous `session_id` when one existed, so callers can discard
    /// abandoned backend artifacts (for example Pi session JSONL files).
    pub fn rotate(
        &mut self,
        thread: &str,
        backend: &str,
        initial_id: String,
    ) -> Result<Option<String>> {
        validate_backend(backend)?;
        let (channel, thread_key) = split_thread(thread)?;
        self.fail_session_write_for_test()?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin session rotate transaction in {}",
                    self.database_path.display()
                )
            })?;
        let previous = tx
            .query_row(
                "SELECT session_id
                 FROM backend_sessions
                 WHERE channel = ?1 AND thread_key = ?2",
                params![channel, thread_key],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .with_context(|| format!("read session before rotate for {thread:?}"))?
            .and_then(|id| non_empty_session_id(&id).map(str::to_string));
        tx.execute(
            "INSERT INTO backend_sessions (
                 channel, thread_key, backend, session_id, started
             ) VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(channel, thread_key) DO UPDATE SET
                 backend = excluded.backend,
                 session_id = excluded.session_id,
                 started = 0,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
            params![channel, thread_key, backend, initial_id],
        )
        .with_context(|| format!("rotate session for {thread:?}"))?;
        tx.commit()
            .with_context(|| format!("commit session rotate for {thread:?}"))?;
        Ok(previous)
    }

    fn migrate_legacy_state(&mut self, _fail_before_commit: bool) -> Result<()> {
        let path = self.legacy_state_path.clone();
        let source_path = path.to_string_lossy().to_string();
        if self
            .conn
            .query_row(
                "SELECT 1 FROM legacy_state_migrations WHERE source_path = ?1",
                [&source_path],
                |_| Ok(()),
            )
            .optional()
            .with_context(|| format!("check legacy state migration for {}", path.display()))?
            .is_some()
        {
            return Ok(());
        }

        let data = match std::fs::read(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("read legacy state {}", path.display()));
            }
        };
        crate::util::restrict_permissions(&path, false)
            .with_context(|| format!("restrict legacy state permissions {}", path.display()))?;
        let legacy: LegacyState = serde_json::from_slice(&data).with_context(|| {
            format!(
                "parse legacy state {}. Restore valid JSON from backup, or move the file aside only if you intend to start without its cursors and sessions",
                path.display()
            )
        })?;
        let source_sha256 = format!("{:x}", Sha256::digest(&data));

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| {
                format!(
                    "begin legacy state migration into {}",
                    self.database_path.display()
                )
            })?;
        if tx
            .query_row(
                "SELECT 1 FROM legacy_state_migrations WHERE source_path = ?1",
                [&source_path],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            tx.commit()?;
            return Ok(());
        }

        for (channel, cursor) in &legacy.cursors {
            insert_monotonic_cursor(&tx, channel, *cursor)?;
        }
        if !legacy.cursors.contains_key("imessage") && legacy.last_row_id != 0 {
            insert_monotonic_cursor(&tx, "imessage", legacy.last_row_id)?;
        }

        // Qualified entries win over legacy unqualified iMessage aliases,
        // regardless of JSON map iteration order.
        for (thread, session) in legacy
            .sessions
            .iter()
            .filter(|(thread, _)| legacy_qualified_thread(thread).is_some())
        {
            let (channel, thread_key) = legacy_qualified_thread(thread).expect("filtered");
            insert_migrated_session(&tx, channel, thread_key, session)?;
        }
        for (thread, session) in legacy
            .sessions
            .iter()
            .filter(|(thread, _)| legacy_qualified_thread(thread).is_none())
        {
            insert_migrated_session(&tx, "imessage", thread, session)?;
        }

        #[cfg(test)]
        if _fail_before_commit {
            bail!("injected migration interruption before commit");
        }
        tx.execute(
            "INSERT INTO legacy_state_migrations (source_path, source_sha256)
             VALUES (?1, ?2)",
            params![source_path, source_sha256],
        )
        .with_context(|| format!("record migration of {}", path.display()))?;
        tx.commit().with_context(|| {
            format!(
                "commit legacy state migration from {} into {}",
                path.display(),
                self.database_path.display()
            )
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub fn fail_next_cursor_save_for_test(&mut self) {
        self.cursor_save_failures_remaining += 1;
    }

    #[cfg(test)]
    pub fn fail_next_completed_row_save_for_test(&mut self) {
        self.completed_row_save_failures_remaining += 1;
    }

    #[cfg(test)]
    pub fn fail_next_session_save_for_test(&mut self) {
        self.session_save_failures_remaining += 1;
    }

    fn fail_session_write_for_test(&mut self) -> Result<()> {
        #[cfg(test)]
        if self.session_save_failures_remaining > 0 {
            self.session_save_failures_remaining -= 1;
            return Err(anyhow::anyhow!("injected session save failure"));
        }
        Ok(())
    }
}

fn insert_monotonic_cursor(
    tx: &rusqlite::Transaction<'_>,
    channel: &str,
    cursor: i64,
) -> Result<()> {
    validate_channel(channel)?;
    tx.execute(
        "INSERT INTO channel_cursors (channel, cursor)
         VALUES (?1, ?2)
         ON CONFLICT(channel) DO UPDATE SET
             cursor = excluded.cursor,
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
         WHERE excluded.cursor > channel_cursors.cursor",
        params![channel, cursor],
    )?;
    Ok(())
}

fn insert_migrated_session(
    tx: &rusqlite::Transaction<'_>,
    channel: &str,
    thread_key: &str,
    session: &SessionInfo,
) -> Result<()> {
    validate_channel(channel)?;
    validate_thread_key(thread_key)?;
    validate_backend(&session.backend)?;
    tx.execute(
        "INSERT INTO backend_sessions (
             channel, thread_key, backend, session_id, started
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(channel, thread_key) DO NOTHING",
        params![
            channel,
            thread_key,
            session.backend,
            session.uuid,
            session.started
        ],
    )?;
    Ok(())
}

fn split_thread(thread: &str) -> Result<(&str, &str)> {
    let Some((channel, thread_key)) = thread.split_once(':') else {
        bail!("backend session thread {thread:?} must be channel-qualified");
    };
    validate_channel(channel)?;
    validate_thread_key(thread_key)?;
    Ok((channel, thread_key))
}

fn legacy_qualified_thread(thread: &str) -> Option<(&str, &str)> {
    let (channel, thread_key) = thread.split_once(':')?;
    matches!(channel, "imessage" | "telegram" | "slack").then_some((channel, thread_key))
}

fn validate_channel(channel: &str) -> Result<()> {
    if channel.trim().is_empty() {
        bail!("channel cannot be empty");
    }
    Ok(())
}

fn validate_thread_key(thread_key: &str) -> Result<()> {
    if thread_key.trim().is_empty() {
        bail!("backend session thread key cannot be empty");
    }
    Ok(())
}

fn validate_backend(backend: &str) -> Result<()> {
    if backend.trim().is_empty() {
        bail!("backend cannot be empty");
    }
    Ok(())
}

fn default_backend() -> String {
    "claude".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use uuid::Uuid;

    fn temp_paths() -> (String, String) {
        let base = std::env::temp_dir().join(format!("push-store-test-{}", Uuid::new_v4()));
        (
            base.with_extension("db").to_string_lossy().to_string(),
            base.with_extension("json").to_string_lossy().to_string(),
        )
    }

    fn open(database_path: &str, state_path: &str) -> Store {
        Store::open_at(database_path, state_path).unwrap()
    }

    fn cleanup(database_path: &str, state_path: &str) {
        let _ = std::fs::remove_file(database_path);
        let _ = std::fs::remove_file(format!("{database_path}-wal"));
        let _ = std::fs::remove_file(format!("{database_path}-shm"));
        let _ = std::fs::remove_file(state_path);
    }

    #[test]
    fn backend_change_starts_fresh_session() {
        let (database_path, state_path) = temp_paths();
        let mut store = open(&database_path, &state_path);

        let first = store
            .session_for("imessage:self:me", "claude", "claude-session".to_string())
            .unwrap();
        assert_eq!(first, ("claude-session".to_string(), true));
        store.mark_started("imessage:self:me", None).unwrap();

        let second = store
            .session_for("imessage:self:me", "codex", String::new())
            .unwrap();
        assert_eq!(second, (String::new(), true));
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn mark_started_can_store_backend_owned_session_id() {
        let (database_path, state_path) = temp_paths();
        let mut store = open(&database_path, &state_path);
        store
            .session_for("telegram:dm:7", "codex", String::new())
            .unwrap();
        store
            .mark_started("telegram:dm:7", Some("codex-thread-id"))
            .unwrap();

        let resumed = store
            .session_for("telegram:dm:7", "codex", String::new())
            .unwrap();
        assert_eq!(resumed, ("codex-thread-id".to_string(), false));
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn empty_session_ids_preserve_fresh_session_behavior() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{"sessions":{"telegram:dm:7":{"uuid":"","started":true,"backend":"codex"}}}"#,
        )
        .unwrap();
        let mut store = open(&database_path, &state_path);

        let fresh = store
            .session_for("telegram:dm:7", "codex", String::new())
            .unwrap();
        assert_eq!(fresh, (String::new(), true));
        store.mark_started("telegram:dm:7", Some(" \n ")).unwrap();
        assert_eq!(
            store
                .session_for("telegram:dm:7", "codex", String::new())
                .unwrap(),
            (String::new(), true)
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn migrates_legacy_cursors_channels_backends_and_imessage_keys() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{
  "last_row_id": 42,
  "cursors": {"telegram": 99},
  "sessions": {
    "dm:+15551234567": {
      "uuid": "legacy-imessage", "started": true, "backend": "claude"
    },
    "telegram:dm:7": {
      "uuid": "telegram-session", "started": true, "backend": "codex"
    },
    "slack:dm:workspace:channel": {
      "uuid": "slack-session", "started": false, "backend": "pi"
    }
  }
}"#,
        )
        .unwrap();
        let mut store = open(&database_path, &state_path);

        assert_eq!(store.cursor("imessage").unwrap(), 42);
        assert_eq!(store.cursor("telegram").unwrap(), 99);
        assert_eq!(
            store
                .session_for("imessage:dm:+15551234567", "claude", "unused".into())
                .unwrap(),
            ("legacy-imessage".into(), false)
        );
        assert_eq!(
            store
                .session_for("telegram:dm:7", "codex", "unused".into())
                .unwrap(),
            ("telegram-session".into(), false)
        );
        assert_eq!(
            store
                .session_for("slack:dm:workspace:channel", "pi", "unused".into())
                .unwrap(),
            ("slack-session".into(), true)
        );
        assert!(std::path::Path::new(&state_path).exists());
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn explicit_imessage_cursor_wins_over_last_row_id() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{"last_row_id":42,"cursors":{"imessage":12}}"#,
        )
        .unwrap();
        let store = open(&database_path, &state_path);
        assert_eq!(store.cursor("imessage").unwrap(), 12);
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn qualified_imessage_session_wins_over_legacy_alias() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{"sessions":{
  "dm:+15551234567":{"uuid":"legacy","started":true,"backend":"claude"},
  "imessage:dm:+15551234567":{"uuid":"qualified","started":true,"backend":"codex"}
}}"#,
        )
        .unwrap();
        let mut store = open(&database_path, &state_path);
        assert_eq!(
            store
                .session_for("imessage:dm:+15551234567", "codex", "unused".into())
                .unwrap(),
            ("qualified".into(), false)
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn migration_is_repeatable_without_regressing_new_state() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{"cursors":{"telegram":10},"sessions":{
  "telegram:dm:7":{"uuid":"legacy","started":true,"backend":"codex"}
}}"#,
        )
        .unwrap();
        let mut store = open(&database_path, &state_path);
        store.set_cursor("telegram", 20).unwrap();
        store
            .rotate("telegram:dm:7", "claude", "new".into())
            .unwrap();
        drop(store);

        let mut reopened = open(&database_path, &state_path);
        assert_eq!(reopened.cursor("telegram").unwrap(), 20);
        assert_eq!(
            reopened
                .session_for("telegram:dm:7", "claude", "unused".into())
                .unwrap(),
            ("new".into(), true)
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn completed_rows_survive_reopen_and_are_pruned_by_cursor() {
        let (database_path, state_path) = temp_paths();
        let mut store = open(&database_path, &state_path);
        store.mark_row_completed("imessage", 12).unwrap();
        store.mark_row_completed("imessage", 14).unwrap();
        store.mark_row_completed("telegram", 13).unwrap();
        drop(store);

        let mut reopened = open(&database_path, &state_path);
        assert_eq!(
            reopened.completed_rows_after("imessage", 0).unwrap(),
            vec![12, 14]
        );
        assert_eq!(
            reopened.completed_rows_after("telegram", 0).unwrap(),
            vec![13]
        );

        reopened.set_cursor("imessage", 12).unwrap();
        assert_eq!(
            reopened.completed_rows_after("imessage", 0).unwrap(),
            vec![14]
        );
        reopened.set_cursor("imessage", 14).unwrap();
        assert!(reopened
            .completed_rows_after("imessage", 0)
            .unwrap()
            .is_empty());
        assert_eq!(
            reopened.completed_rows_after("telegram", 0).unwrap(),
            vec![13]
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn pending_filename_grace_survives_reopen_and_clears() {
        let (database_path, state_path) = temp_paths();
        let mut store = open(&database_path, &state_path);
        assert!(store
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        drop(store);

        let mut reopened = open(&database_path, &state_path);
        assert!(reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        drop(reopened);

        let mut reopened = open(&database_path, &state_path);
        assert!(reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        assert!(!reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        assert!(!reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());

        reopened.clear_pending_filename("imessage", 12).unwrap();
        assert!(reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        reopened.set_cursor("imessage", 12).unwrap();
        assert!(reopened
            .should_defer_pending_filename("imessage", 12, 3)
            .unwrap());
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn interrupted_migration_rolls_back_and_retries_cleanly() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(
            &state_path,
            r#"{"cursors":{"telegram":10},"sessions":{
  "telegram:dm:7":{"uuid":"legacy","started":true,"backend":"codex"}
}}"#,
        )
        .unwrap();
        let error = Store::open_inner(Path::new(&database_path), Path::new(&state_path), true)
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("injected migration interruption"));
        assert!(std::path::Path::new(&state_path).exists());

        let mut store = open(&database_path, &state_path);
        assert_eq!(store.cursor("telegram").unwrap(), 10);
        assert_eq!(
            store
                .session_for("telegram:dm:7", "codex", "unused".into())
                .unwrap(),
            ("legacy".into(), false)
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn corrupt_json_fails_with_recovery_guidance() {
        let (database_path, state_path) = temp_paths();
        std::fs::write(&state_path, "{broken").unwrap();
        let error = Store::open_at(&database_path, &state_path).err().unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("parse legacy state"));
        assert!(message.contains("Restore valid JSON from backup"));
        assert!(std::path::Path::new(&state_path).exists());
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn database_open_failure_is_actionable_before_state_is_available() {
        let (database_path, state_path) = temp_paths();
        let blocker = PathBuf::from(&database_path).with_extension("blocker");
        std::fs::write(&blocker, "not a directory").unwrap();
        let blocked_database = blocker.join("push.db");

        let error = Store::open_at(&blocked_database, &state_path)
            .err()
            .unwrap();
        let message = format!("{error:#}");
        assert!(message.contains("prepare canonical state database"));
        assert!(
            message.contains("create database directory")
                || message.contains("open conversation database")
        );
        assert!(!std::path::Path::new(&state_path).exists());
        let _ = std::fs::remove_file(blocker);
    }

    #[test]
    fn concurrent_cursor_writes_never_move_backwards() {
        let (database_path, state_path) = temp_paths();
        drop(open(&database_path, &state_path));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for cursor in [80, 10, 70, 20, 60, 30, 50, 40] {
            let database_path = database_path.clone();
            let state_path = state_path.clone();
            let barrier = barrier.clone();
            handles.push(thread::spawn(move || {
                let mut store = open(&database_path, &state_path);
                barrier.wait();
                store.set_cursor("telegram", cursor).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            open(&database_path, &state_path)
                .cursor("telegram")
                .unwrap(),
            80
        );
        cleanup(&database_path, &state_path);
    }

    #[test]
    fn clear_rotates_only_the_exact_channel_qualified_session() {
        let (database_path, state_path) = temp_paths();
        let mut store = open(&database_path, &state_path);
        for thread in ["imessage:dm:7", "telegram:dm:7"] {
            store
                .session_for(thread, "codex", format!("{thread}-old"))
                .unwrap();
            store.mark_started(thread, None).unwrap();
        }

        assert_eq!(
            store
                .rotate("telegram:dm:7", "codex", "telegram-new".into())
                .unwrap(),
            Some("telegram:dm:7-old".into())
        );
        assert_eq!(
            store
                .session_for("telegram:dm:7", "codex", "unused".into())
                .unwrap(),
            ("telegram-new".into(), true)
        );
        assert_eq!(
            store
                .session_for("imessage:dm:7", "codex", "unused".into())
                .unwrap(),
            ("imessage:dm:7-old".into(), false)
        );
        cleanup(&database_path, &state_path);
    }

    #[cfg(unix)]
    #[test]
    fn database_and_legacy_backup_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (database_path, state_path) = temp_paths();
        std::fs::write(&state_path, "{}").unwrap();
        std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o666)).unwrap();
        drop(open(&database_path, &state_path));
        assert_eq!(
            std::fs::metadata(&database_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        cleanup(&database_path, &state_path);
    }
}
