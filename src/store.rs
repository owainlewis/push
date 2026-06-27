//! Persisted gateway state: last processed message ROWID and the mapping from
//! conversation thread to Claude Code session UUID.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionInfo {
    pub uuid: String,
    #[serde(default)]
    pub started: bool,
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

    /// Returns the session UUID for a thread, creating one if needed. The second
    /// value is true when the UUID has not been started yet (use `--session-id`
    /// rather than `--resume`).
    pub fn session_for(&mut self, thread: &str) -> Result<(String, bool)> {
        if let Some(si) = self.state.sessions.get(thread) {
            return Ok((si.uuid.clone(), !si.started));
        }
        let uuid = Uuid::new_v4().to_string();
        self.state.sessions.insert(
            thread.to_string(),
            SessionInfo {
                uuid: uuid.clone(),
                started: false,
            },
        );
        self.save()?;
        Ok((uuid, true))
    }

    pub fn mark_started(&mut self, thread: &str) -> Result<()> {
        if let Some(si) = self.state.sessions.get_mut(thread) {
            if !si.started {
                si.started = true;
                return self.save();
            }
        }
        Ok(())
    }

    /// Assigns a fresh session UUID to a thread (the `/clear` behavior).
    pub fn rotate(&mut self, thread: &str) -> Result<()> {
        self.state.sessions.insert(
            thread.to_string(),
            SessionInfo {
                uuid: Uuid::new_v4().to_string(),
                started: false,
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
