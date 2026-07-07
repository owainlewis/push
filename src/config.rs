//! Gateway configuration loaded from a JSON file.

use std::collections::HashSet;
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
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    #[serde(default)]
    pub assistant: AssistantProfile,
    #[serde(default = "default_claude_bin")]
    pub claude_bin: String,
    #[serde(default = "default_claude_permission_mode", alias = "permission_mode")]
    pub claude_permission_mode: String,
    #[serde(default, alias = "tools")]
    pub claude_tools: Option<Vec<String>>,
    #[serde(default, alias = "allowed_tools")]
    pub claude_allowed_tools: Vec<String>,
    #[serde(default, alias = "disallowed_tools")]
    pub claude_disallowed_tools: Vec<String>,
    #[serde(default = "default_codex_bin")]
    pub codex_bin: String,
    #[serde(default = "default_codex_sandbox")]
    pub codex_sandbox: String,
    #[serde(default = "default_codex_approval_policy")]
    pub codex_approval_policy: String,
    #[serde(default)]
    pub codex_model: Option<String>,
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

    pub fn agent_backend(&self) -> Result<AgentBackend> {
        AgentBackend::parse(&self.agent)
    }

    pub fn agent_for_thread(&self, thread: &str) -> Result<AgentBackend> {
        for route in &self.routes {
            if route.thread == thread {
                return AgentBackend::parse(&route.agent);
            }
        }
        self.agent_backend()
    }

    pub fn required_agent_bins(&self) -> Result<Vec<&str>> {
        let mut backends = HashSet::new();
        backends.insert(self.agent_backend()?);
        for route in &self.routes {
            backends.insert(AgentBackend::parse(&route.agent)?);
        }

        let mut bins = Vec::new();
        for backend in backends {
            bins.push(match backend {
                AgentBackend::Claude => self.claude_bin.as_str(),
                AgentBackend::Codex => self.codex_bin.as_str(),
            });
        }
        Ok(bins)
    }

    fn validate(&self) -> Result<()> {
        if self.self_handles.is_empty() && self.allow_from.is_empty() {
            bail!(
                "set at least one of self_handles or allow_from, or nobody can reach the assistant"
            );
        }
        self.agent_backend()?;
        for route in &self.routes {
            AgentBackend::parse(&route.agent)
                .with_context(|| format!("invalid route agent for {}", route.thread))?;
            if route.thread.trim().is_empty() {
                bail!("route thread cannot be empty");
            }
        }
        for tool in self
            .claude_allowed_tools
            .iter()
            .chain(self.claude_disallowed_tools.iter())
        {
            if tool.trim().is_empty() {
                bail!("claude tool filters cannot contain empty entries");
            }
        }
        if self.claude_tools.as_ref().is_some_and(Vec::is_empty) {
            bail!("claude_tools must be null or contain at least one entry");
        }
        if !matches!(
            self.codex_sandbox.as_str(),
            "read-only" | "workspace-write" | "danger-full-access"
        ) {
            bail!("invalid codex_sandbox {}; expected read-only, workspace-write, or danger-full-access", self.codex_sandbox);
        }
        if !matches!(
            self.codex_approval_policy.as_str(),
            "untrusted" | "on-request" | "never"
        ) {
            bail!(
                "invalid codex_approval_policy {}; expected untrusted, on-request, or never",
                self.codex_approval_policy
            );
        }
        self.poll_interval_dur()?;
        self.run_timeout_dur()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AssistantProfile {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub tone: String,
    #[serde(default)]
    pub business: String,
    #[serde(default)]
    pub projects: Vec<String>,
    #[serde(default)]
    pub preferences: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct RouteRule {
    pub thread: String,
    pub agent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBackend {
    Claude,
    Codex,
}

impl AgentBackend {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(AgentBackend::Claude),
            "codex" => Ok(AgentBackend::Codex),
            other => bail!("invalid agent {other:?}; expected \"claude\" or \"codex\""),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentBackend::Claude => "claude",
            AgentBackend::Codex => "codex",
        }
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
fn default_agent() -> String {
    "claude".to_string()
}
fn default_claude_bin() -> String {
    "claude".to_string()
}
fn default_claude_permission_mode() -> String {
    "bypassPermissions".to_string()
}
fn default_codex_bin() -> String {
    "codex".to_string()
}
fn default_codex_sandbox() -> String {
    "workspace-write".to_string()
}
fn default_codex_approval_policy() -> String {
    "never".to_string()
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
    "\n\n-- sent by push".to_string()
}
