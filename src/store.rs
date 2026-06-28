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

    pub fn last_row(&self) -> i64 {
        self.state.last_row_id
    }

    pub fn set_last_row(&mut self, id: i64) -> Result<()> {
        if id <= self.state.last_row_id {
            return Ok(());
        }
        self.state.last_row_id = id;
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
        if let Some(si) = self.state.sessions.get(thread) {
            if si.backend == backend {
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
            if let Some(id) = session_id {
                si.uuid = id.to_string();
            }
            if !si.started {
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
}

fn default_backend() -> String {
    "claude".to_string()
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
}
