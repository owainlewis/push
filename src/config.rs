//! Gateway configuration loaded from a TOML file.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_channel")]
    pub channel: String,
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
    #[serde(default)]
    pub telegram_bot_token: Option<String>,
    #[serde(default = "default_telegram_bot_token_env")]
    pub telegram_bot_token_env: String,
    #[serde(default)]
    pub telegram_allow_user_ids: Vec<i64>,
    #[serde(default)]
    pub telegram_allow_chat_ids: Vec<i64>,
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
    #[serde(default = "default_audit_log_path")]
    pub audit_log_path: String,
    #[serde(default)]
    pub audit_log_content: bool,
    #[serde(default = "default_assistant_dir")]
    pub assistant_dir: String,
    #[serde(default = "default_reply_marker")]
    pub reply_marker: String,
}

impl Config {
    /// Load, expand `~` in path fields, and validate the config at `path`.
    pub fn load(path: &str) -> Result<Config> {
        let raw = std::fs::read_to_string(path).with_context(|| format!("read config {path}"))?;
        let mut value: toml::Value = toml::from_str(&raw).context("parse TOML config")?;
        let root = value
            .as_table_mut()
            .context("config must be a TOML table")?;
        flatten_provider_section(
            root,
            "imessage",
            &[
                ("db_path", "db_path"),
                ("self_handles", "self_handles"),
                ("allow_from", "allow_from"),
            ],
        )?;
        flatten_provider_section(
            root,
            "telegram",
            &[
                ("bot_token", "telegram_bot_token"),
                ("bot_token_env", "telegram_bot_token_env"),
                ("allow_user_ids", "telegram_allow_user_ids"),
                ("allow_chat_ids", "telegram_allow_chat_ids"),
            ],
        )?;
        let mut c: Config = value.try_into().context("parse TOML config")?;
        c.db_path = expand_home(&c.db_path);
        c.sessions_dir = expand_home(&c.sessions_dir);
        c.state_path = expand_home(&c.state_path);
        c.audit_log_path = expand_home(&c.audit_log_path);
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

    pub fn channel_kind(&self) -> Result<ChannelKind> {
        ChannelKind::parse(&self.channel)
    }

    pub fn telegram_token(&self) -> Option<String> {
        self.telegram_bot_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var(&self.telegram_bot_token_env)
                    .ok()
                    .map(|token| token.trim().to_string())
                    .filter(|token| !token.is_empty())
            })
    }

    pub fn agent_for_message(&self, channel: &str, thread: &str) -> Result<AgentBackend> {
        for route in self.routes.iter().filter(|route| route.thread.is_some()) {
            if route.matches(channel, thread) {
                return AgentBackend::parse(&route.agent);
            }
        }
        for route in &self.routes {
            if route.matches(channel, thread) {
                return AgentBackend::parse(&route.agent);
            }
        }
        self.agent_backend()
    }

    pub fn required_agent_bins(&self) -> Result<Vec<&str>> {
        let mut backends = HashSet::new();
        backends.insert(self.agent_backend()?);
        for route in self
            .routes
            .iter()
            .filter(|route| route.can_match_channel(&self.channel))
        {
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
        match self.channel_kind()? {
            ChannelKind::IMessage => {
                if self.self_handles.is_empty() && self.allow_from.is_empty() {
                    bail!("set imessage.self_handles or imessage.allow_from for iMessage");
                }
            }
            ChannelKind::Telegram => {
                if self.telegram_allow_user_ids.is_empty()
                    && self.telegram_allow_chat_ids.is_empty()
                {
                    bail!("set telegram.allow_user_ids or telegram.allow_chat_ids for Telegram");
                }
                if self
                    .telegram_bot_token
                    .as_deref()
                    .is_some_and(|v| v.trim().is_empty())
                {
                    bail!("telegram.bot_token cannot be empty");
                }
                if self.telegram_bot_token_env.trim().is_empty() {
                    bail!("telegram.bot_token_env cannot be empty");
                }
            }
        }
        self.agent_backend()?;
        for route in &self.routes {
            AgentBackend::parse(&route.agent)
                .with_context(|| format!("invalid route agent for {route:?}"))?;
            if route.thread.is_none() && route.channel.is_none() {
                bail!("route must set thread or channel");
            }
            if route.thread.as_deref().is_some_and(|v| v.trim().is_empty()) {
                bail!("route thread cannot be empty");
            }
            if let Some(channel) = &route.channel {
                ChannelKind::parse(channel).context("invalid route channel")?;
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

fn flatten_provider_section(
    root: &mut toml::Table,
    section: &str,
    fields: &[(&str, &str)],
) -> Result<()> {
    let Some(value) = root.remove(section) else {
        return Ok(());
    };
    let table = value
        .as_table()
        .with_context(|| format!("[{section}] must be a TOML table"))?;

    for (key, value) in table {
        let Some((_, destination)) = fields.iter().find(|(source, _)| source == key) else {
            let expected = fields
                .iter()
                .map(|(source, _)| *source)
                .collect::<Vec<_>>()
                .join(", ");
            bail!("unknown [{section}] setting {key:?}; expected one of: {expected}");
        };
        if root.contains_key(*destination) {
            bail!("set {destination} either at the top level or as [{section}].{key}, not both");
        }
        root.insert((*destination).to_string(), value.clone());
    }
    Ok(())
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
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    pub agent: String,
}

impl RouteRule {
    fn can_match_channel(&self, channel: &str) -> bool {
        self.channel.as_deref().is_none_or(|value| value == channel)
    }

    fn matches(&self, channel: &str, thread: &str) -> bool {
        if !self.can_match_channel(channel) {
            return false;
        }
        self.thread.as_deref().is_none_or(|value| {
            value == thread
                || (channel == "imessage"
                    && thread
                        .strip_prefix("imessage:")
                        .is_some_and(|legacy| legacy == value))
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelKind {
    IMessage,
    Telegram,
}

impl ChannelKind {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "imessage" => Ok(Self::IMessage),
            "telegram" => Ok(Self::Telegram),
            other => bail!("invalid channel {other:?}; expected \"imessage\" or \"telegram\""),
        }
    }
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
fn default_channel() -> String {
    "imessage".to_string()
}
fn default_telegram_bot_token_env() -> String {
    "TELEGRAM_BOT_TOKEN".to_string()
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
fn default_audit_log_path() -> String {
    "~/.push/audit.jsonl".to_string()
}
fn default_assistant_dir() -> String {
    "./assistant".to_string()
}
fn default_reply_marker() -> String {
    "\n\n-- sent by push".to_string()
}
