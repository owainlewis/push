//! Gateway configuration loaded from a JSON file.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_db_path")]
    pub db_path: String,
    #[serde(default = "default_poll_interval")]
    pub poll_interval: String,
    #[serde(default = "default_run_timeout")]
    pub run_timeout: String,
    #[serde(default)]
    pub self_handles: Vec<String>,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default = "default_claude_bin")]
    pub claude_bin: String,
    #[serde(default = "default_permission_mode")]
    pub permission_mode: String,
    #[serde(default = "default_sessions_dir")]
    pub sessions_dir: String,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    #[serde(default = "default_assistant_dir")]
    pub assistant_dir: String,
    #[serde(default = "default_reply_marker")]
    pub reply_marker: String,
}

impl Config {
    /// Load, expand `~` in path fields, and validate the config at `path`.
    pub fn load(path: &str) -> Result<Config> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut c: Config = serde_json::from_str(&raw).context("parse config")?;
        c.db_path = expand_home(&c.db_path);
        c.sessions_dir = expand_home(&c.sessions_dir);
        c.state_path = expand_home(&c.state_path);
        c.assistant_dir = expand_home(&c.assistant_dir);
        c.validate()?;
        Ok(c)
    }

    pub fn poll_interval_dur(&self) -> Result<Duration> {
        humantime::parse_duration(&self.poll_interval)
            .with_context(|| format!("invalid poll_interval {}", self.poll_interval))
    }

    pub fn run_timeout_dur(&self) -> Result<Duration> {
        humantime::parse_duration(&self.run_timeout)
            .with_context(|| format!("invalid run_timeout {}", self.run_timeout))
    }

    fn validate(&self) -> Result<()> {
        if self.self_handles.is_empty() && self.allow_from.is_empty() {
            bail!(
                "set at least one of self_handles or allow_from, or nobody can reach the assistant"
            );
        }
        self.poll_interval_dur()?;
        self.run_timeout_dur()?;
        Ok(())
    }
}

fn expand_home(p: &str) -> String {
    if p == "~" || p.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", home.to_string_lossy(), &p[1..]);
        }
    }
    p.to_string()
}

fn default_db_path() -> String {
    "~/Library/Messages/chat.db".to_string()
}
fn default_poll_interval() -> String {
    "3s".to_string()
}
fn default_run_timeout() -> String {
    "120s".to_string()
}
fn default_claude_bin() -> String {
    "claude".to_string()
}
fn default_permission_mode() -> String {
    "bypassPermissions".to_string()
}
fn default_sessions_dir() -> String {
    "~/.push/sessions".to_string()
}
fn default_state_path() -> String {
    "~/.push/state.json".to_string()
}
fn default_assistant_dir() -> String {
    "./assistant".to_string()
}
fn default_reply_marker() -> String {
    "\n\n— sent by push".to_string()
}
