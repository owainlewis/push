//! Persisted gateway state: last processed message ROWID and the mapping from
//! conversation thread to the active agent backend session.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub uuid: String,
    #[serde(default)]
    pub started: bool,
    #[serde(default = "default_backend")]
    pub backend: String,
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    #[serde(default)]
    last_row_id: i64,
    #[serde(default)]
    cursors: HashMap<String, i64>,
    #[serde(default)]
    sessions: HashMap<String, SessionInfo>,
}

/// Store owns the persisted state and writes it atomically on every change.
pub struct Store {
    path: PathBuf,
    state: State,
}

impl Store {
    pub fn open(path: &str) -> Result<Store> {
        let p = PathBuf::from(path);
        let state = match std::fs::read_to_string(&p) {
            Ok(s) => serde_json::from_str(&s).with_context(|| format!("parse state {path}"))?,
            Err(e) if e.kind() == ErrorKind::NotFound => State::default(),
            Err(e) => return Err(anyhow!("read state {path}: {e}")),
        };
        Ok(Store { path: p, state })
    }

    #[cfg(test)]
    pub fn last_row(&self) -> i64 {
        self.cursor("imessage")
    }

    pub fn has_cursor(&self, channel: &str) -> bool {
        self.state.cursors.contains_key(channel)
            || (channel == "imessage" && self.state.last_row_id != 0)
    }

    pub fn cursor(&self, channel: &str) -> i64 {
        self.state.cursors.get(channel).copied().unwrap_or_else(|| {
            if channel == "imessage" {
                self.state.last_row_id
            } else {
                0
            }
        })
    }

    pub fn set_cursor(&mut self, channel: &str, id: i64) -> Result<()> {
        if self.has_cursor(channel) && id <= self.cursor(channel) {
            return Ok(());
        }
        self.state.cursors.insert(channel.to_string(), id);
        if channel == "imessage" {
            self.state.last_row_id = id;
        }
        self.save()
    }

    /// Returns the agent session id for a thread, creating one if needed. The
    /// second value is true when the backend has not started that session yet.
    pub fn session_for(
        &mut self,
        thread: &str,
        backend: &str,
        initial_id: String,
    ) -> Result<(String, bool)> {
        self.migrate_legacy_imessage_session(thread)?;
        if let Some(si) = self.state.sessions.get(thread) {
            if si.backend == backend {
                if si.uuid.trim().is_empty() {
                    self.state.sessions.insert(
                        thread.to_string(),
                        SessionInfo {
                            uuid: initial_id.clone(),
                            started: false,
                            backend: backend.to_string(),
                        },
                    );
                    self.save()?;
                    return Ok((initial_id, true));
                }
                return Ok((si.uuid.clone(), !si.started));
            }
        }
        self.state.sessions.insert(
            thread.to_string(),
            SessionInfo {
                uuid: initial_id.clone(),
                started: false,
                backend: backend.to_string(),
            },
        );
        self.save()?;
        Ok((initial_id, true))
    }

    pub fn mark_started(&mut self, thread: &str, session_id: Option<&str>) -> Result<()> {
        if let Some(si) = self.state.sessions.get_mut(thread) {
            let session_id = session_id.and_then(non_empty_session_id);
            if let Some(id) = session_id {
                si.uuid = id.to_string();
            }
            if !si.started {
                if si.uuid.trim().is_empty() {
                    return Ok(());
                }
                si.started = true;
                return self.save();
            }
            if session_id.is_some() {
                return self.save();
            }
        }
        Ok(())
    }

    /// Assigns a fresh backend session to a thread (the `/clear` behavior).
    pub fn rotate(&mut self, thread: &str, backend: &str, initial_id: String) -> Result<()> {
        self.state.sessions.insert(
            thread.to_string(),
            SessionInfo {
                uuid: initial_id,
                started: false,
                backend: backend.to_string(),
            },
        );
        self.save()
    }

    fn migrate_legacy_imessage_session(&mut self, thread: &str) -> Result<()> {
        let Some(legacy) = thread.strip_prefix("imessage:") else {
            return Ok(());
        };
        if self.state.sessions.contains_key(thread) {
            return Ok(());
        }
        if let Some(session) = self.state.sessions.remove(legacy) {
            self.state.sessions.insert(thread.to_string(), session);
            self.save()?;
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).context("create state dir")?;
        }
        let tmp = self.path.with_extension("tmp");
        let data = serde_json::to_string_pretty(&self.state)?;
        std::fs::write(&tmp, data).context("write state")?;
        std::fs::rename(&tmp, &self.path).context("rename state")?;
        Ok(())
    }

    #[cfg(test)]
    pub fn set_path_for_test(&mut self, path: PathBuf) {
        self.path = path;
    }
}

fn default_backend() -> String {
    "claude".to_string()
}

fn non_empty_session_id(id: &str) -> Option<&str> {
    let trimmed = id.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_state_path() -> String {
        std::env::temp_dir()
            .join(format!("push-store-test-{}.json", Uuid::new_v4()))
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn backend_change_starts_fresh_session() {
        let path = temp_state_path();
        let mut store = Store::open(&path).unwrap();

        let first = store
            .session_for("self:me", "claude", "claude-session".to_string())
            .unwrap();
        assert_eq!(first, ("claude-session".to_string(), true));
        store.mark_started("self:me", None).unwrap();

        let second = store
            .session_for("self:me", "codex", String::new())
            .unwrap();
        assert_eq!(second, (String::new(), true));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mark_started_can_store_backend_owned_session_id() {
        let path = temp_state_path();
        let mut store = Store::open(&path).unwrap();

        store
            .session_for("self:me", "codex", String::new())
            .unwrap();
        store
            .mark_started("self:me", Some("codex-thread-id"))
            .unwrap();

        let resumed = store
            .session_for("self:me", "codex", String::new())
            .unwrap();
        assert_eq!(resumed, ("codex-thread-id".to_string(), false));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mark_started_ignores_empty_backend_session_id() {
        let path = temp_state_path();
        let mut store = Store::open(&path).unwrap();

        store
            .session_for("self:me", "claude", "initial-session".to_string())
            .unwrap();
        store.mark_started("self:me", Some(" \t\n ")).unwrap();

        let resumed = store
            .session_for("self:me", "claude", "unused-session".to_string())
            .unwrap();
        assert_eq!(resumed, ("initial-session".to_string(), false));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn existing_empty_backend_session_id_starts_fresh_session() {
        let path = temp_state_path();
        std::fs::write(
            &path,
            r#"{
  "sessions": {
    "self:me": {
      "uuid": "",
      "started": true,
      "backend": "claude"
    }
  }
}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();

        let fresh = store
            .session_for("self:me", "claude", "fresh-session".to_string())
            .unwrap();
        assert_eq!(fresh, ("fresh-session".to_string(), true));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn existing_empty_unstarted_backend_session_id_starts_fresh_session() {
        let path = temp_state_path();
        std::fs::write(
            &path,
            r#"{
  "sessions": {
    "self:me": {
      "uuid": "",
      "started": false,
      "backend": "claude"
    }
  }
}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();

        let fresh = store
            .session_for("self:me", "claude", "fresh-session".to_string())
            .unwrap();
        assert_eq!(fresh, ("fresh-session".to_string(), true));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn mark_started_does_not_activate_empty_backend_session_id() {
        let path = temp_state_path();
        let mut store = Store::open(&path).unwrap();

        store
            .session_for("self:me", "codex", String::new())
            .unwrap();
        store.mark_started("self:me", Some("")).unwrap();

        let fresh = store
            .session_for("self:me", "codex", String::new())
            .unwrap();
        assert_eq!(fresh, (String::new(), true));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_last_row_id_remains_the_imessage_cursor() {
        let path = temp_state_path();
        std::fs::write(&path, r#"{"last_row_id":42,"sessions":{}}"#).unwrap();
        let store = Store::open(&path).unwrap();

        assert!(store.has_cursor("imessage"));
        assert_eq!(store.cursor("imessage"), 42);
        assert_eq!(store.cursor("telegram"), 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn channel_cursors_persist_independently() {
        let path = temp_state_path();
        let mut store = Store::open(&path).unwrap();

        store.set_cursor("imessage", 12).unwrap();
        store.set_cursor("telegram", 99).unwrap();
        store.set_cursor("imessage", 13).unwrap();
        drop(store);

        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.cursor("imessage"), 13);
        assert_eq!(reopened.cursor("telegram"), 99);
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"last_row_id\": 13"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_imessage_session_moves_to_channel_qualified_key() {
        let path = temp_state_path();
        std::fs::write(
            &path,
            r#"{
  "sessions": {
    "dm:+15551234567": {
      "uuid": "existing-session",
      "started": true,
      "backend": "claude"
    }
  }
}"#,
        )
        .unwrap();
        let mut store = Store::open(&path).unwrap();

        let session = store
            .session_for("imessage:dm:+15551234567", "claude", "unused".to_string())
            .unwrap();

        assert_eq!(session, ("existing-session".to_string(), false));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("imessage:dm:+15551234567"));
        assert!(!raw.contains("\"dm:+15551234567\""));
        let _ = std::fs::remove_file(path);
    }
}
