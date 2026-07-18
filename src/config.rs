//! Gateway configuration loaded from a TOML file.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::util::expand_home;

pub const TELEGRAM_BOT_TOKEN_ENV: &str = "TELEGRAM_BOT_TOKEN";
pub const DEFAULT_VOICE_NAME: &str = "cedar";
const SUPPORTED_VOICE_NAMES: &[&str] = &[
    "alloy", "ash", "ballad", "coral", "echo", "fable", "nova", "onyx", "sage", "shimmer", "verse",
    "marin", "cedar",
];

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct AgentCommands {
    pub claude: String,
    pub codex: String,
    pub pi: String,
}

#[cfg(test)]
impl Default for AgentCommands {
    fn default() -> Self {
        Self {
            claude: "claude".to_string(),
            codex: "codex".to_string(),
            pi: "pi".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub primary_delivery: Option<PrimaryDeliveryConfig>,
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
    #[serde(default)]
    pub telegram_allow_user_ids: Vec<i64>,
    #[serde(default)]
    pub telegram_allow_chat_ids: Vec<i64>,
    #[serde(default)]
    pub voice_openai_api_key: Option<String>,
    #[serde(default = "default_voice_name")]
    pub voice_name: String,
    #[serde(default = "default_agent")]
    pub agent: String,
    #[serde(default)]
    pub routes: Vec<RouteRule>,
    /// Canonical root of the single user-owned assistant repository.
    #[serde(default)]
    pub assistant_root: String,
    /// Derived from `assistant_root`. Parsed only for legacy migration.
    #[serde(default = "default_jobs_dir")]
    pub jobs_dir: String,
    #[serde(default = "default_drafts_dir")]
    pub drafts_dir: String,
    #[serde(default)]
    pub jobs_agent: Option<String>,
    #[serde(default = "default_jobs_max_timeout")]
    pub jobs_max_timeout: String,
    #[serde(default = "default_jobs_run_dir")]
    pub jobs_run_dir: String,
    #[serde(default = "default_jobs_max_workers")]
    pub jobs_max_workers: usize,
    #[serde(default = "default_state_path")]
    pub state_path: String,
    #[serde(default = "default_audit_log_path")]
    pub audit_log_path: String,
    #[serde(default = "default_database_path")]
    pub database_path: String,
    #[serde(default)]
    pub audit_log_content: bool,
    /// Derived from `assistant_root`. Parsed only for legacy migration.
    #[serde(default = "default_assistant_dir")]
    pub assistant_dir: String,
    /// Canonical path of the loaded config file. Set by `load`, never parsed.
    #[serde(skip)]
    pub config_path: String,
    /// Test-only command injection point. Never parsed from user configuration.
    #[cfg(test)]
    #[serde(skip)]
    pub(crate) agent_commands: AgentCommands,
}

impl Config {
    /// Load, expand `~` in path fields, and validate the config at `path`.
    pub fn load(path: &str) -> Result<Config> {
        let expanded_path = expand_home(path);
        let raw = std::fs::read_to_string(&expanded_path)
            .with_context(|| format!("read config {expanded_path}"))?;
        let mut value: toml::Value = toml::from_str(&raw).context("parse TOML config")?;
        let root = value
            .as_table_mut()
            .context("config must be a TOML table")?;
        if root.contains_key("assistant") {
            bail!(
                "structured [assistant] settings are no longer supported; move assistant identity into assistant_root/SOUL.md"
            );
        }
        for removed in ["permission_profile", "permission_profiles"] {
            if root.contains_key(removed) {
                bail!(
                    "{removed} is no longer supported; configure permissions in the selected agent and remove this key from Push config"
                );
            }
        }
        if root
            .get("routes")
            .and_then(toml::Value::as_array)
            .is_some_and(|routes| {
                routes.iter().any(|route| {
                    route
                        .as_table()
                        .is_some_and(|route| route.contains_key("permission_profile"))
                })
            })
        {
            bail!(
                "route permission_profile is no longer supported; configure permissions in the selected agent and remove this key from Push config"
            );
        }
        for legacy in [
            "permission_mode",
            "tools",
            "allowed_tools",
            "disallowed_tools",
            "claude_permission_mode",
            "claude_tools",
            "claude_allowed_tools",
            "claude_disallowed_tools",
            "codex_sandbox",
            "codex_approval_policy",
        ] {
            if root.contains_key(legacy) {
                bail!(
                    "legacy permission setting {legacy:?} is no longer supported; configure permissions in the selected agent and remove this key from Push config"
                );
            }
        }
        let has_removed_telegram_token_env = root.contains_key("telegram_bot_token_env")
            || root
                .get("telegram")
                .and_then(toml::Value::as_table)
                .is_some_and(|telegram| telegram.contains_key("bot_token_env"));
        if has_removed_telegram_token_env {
            bail!(
                "telegram_bot_token_env / telegram.bot_token_env is no longer configurable; set TELEGRAM_BOT_TOKEN or remove this key from Push config"
            );
        }
        for (removed, replacement) in [
            ("claude_bin", "put claude on the service PATH instead"),
            ("codex_bin", "put codex on the service PATH instead"),
            ("pi_bin", "put pi on the service PATH instead"),
            ("codex_model", "configure the model in Codex instead"),
            ("sessions_dir", "remove this key from Push config"),
            ("reply_marker", "remove this key from Push config"),
        ] {
            if root.contains_key(removed) {
                bail!("{removed} is no longer configurable; {replacement}");
            }
        }
        if root.contains_key("job_permission_profiles") {
            bail!(
                "job_permission_profiles is no longer supported; jobs run with the backend's own permission configuration, so remove this key"
            );
        }
        let has_assistant_root = root.contains_key("assistant_root");
        let has_legacy_assistant_dir = root.contains_key("assistant_dir");
        let has_legacy_jobs_dir = root.contains_key("jobs_dir");
        if has_assistant_root && (has_legacy_assistant_dir || has_legacy_jobs_dir) {
            bail!(
                "assistant_root replaces legacy assistant_dir and jobs_dir; remove the legacy keys after moving SOUL.md, context, and jobs under assistant_root"
            );
        }
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
                ("allow_user_ids", "telegram_allow_user_ids"),
                ("allow_chat_ids", "telegram_allow_chat_ids"),
            ],
        )?;
        flatten_provider_section(
            root,
            "voice",
            &[
                ("openai_api_key", "voice_openai_api_key"),
                ("name", "voice_name"),
            ],
        )?;
        let mut c: Config = value.try_into().context("parse TOML config")?;
        let config_path = std::fs::canonicalize(&expanded_path)
            .with_context(|| format!("resolve config {expanded_path}"))?;
        c.db_path = expand_home(&c.db_path);
        c.state_path = expand_home(&c.state_path);
        c.audit_log_path = expand_home(&c.audit_log_path);
        c.database_path = expand_home(&c.database_path);
        let assistant_root = if has_assistant_root {
            resolve_assistant_root(&c.assistant_root, &config_path)?
        } else {
            let legacy_root = resolve_assistant_root(&c.assistant_dir, &config_path)?;
            let legacy_jobs = resolved_absolute("jobs_dir", Path::new(&expand_home(&c.jobs_dir)))?;
            let derived_jobs = legacy_root.join("jobs");
            if legacy_jobs != derived_jobs {
                bail!(
                    "legacy assistant_dir ({}) and jobs_dir ({}) do not form one assistant repository. Move SOUL.md, context, and jobs under one directory, then replace both settings with assistant_root = \"/path/to/assistant\"",
                    legacy_root.display(),
                    legacy_jobs.display()
                );
            }
            legacy_root
        };
        if has_assistant_root {
            validate_inline_token_location(
                &config_path,
                &assistant_root,
                c.telegram_bot_token.as_deref(),
            )?;
            validate_inline_voice_key_location(
                &config_path,
                &assistant_root,
                c.voice_openai_api_key.as_deref(),
            )?;
        }
        c.assistant_root = assistant_root.to_string_lossy().to_string();
        c.assistant_dir = c.assistant_root.clone();
        c.jobs_dir = assistant_root.join("jobs").to_string_lossy().to_string();
        c.drafts_dir = expand_home(&c.drafts_dir);
        c.jobs_run_dir = expand_home(&c.jobs_run_dir);
        if has_assistant_root {
            validate_runtime_outside_assistant(&c)?;
        }
        c.validate()?;
        c.config_path = validate_resolved_config_path(config_path, &c)?;
        Ok(c)
    }

    /// Returns the assistant context directory only when it is a real
    /// directory directly beneath the canonical assistant root. This keeps a
    /// repository symlink from widening an agent backend's writable boundary.
    pub fn backend_context_dir(&self) -> Result<Option<PathBuf>> {
        let root = std::fs::canonicalize(&self.assistant_root)
            .with_context(|| format!("resolve assistant_root {}", self.assistant_root))?;
        let context = root.join("context");
        let metadata = match std::fs::symlink_metadata(&context) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect assistant context {}", context.display()));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "assistant context {} must be a real directory, not a symlink or file",
                context.display()
            );
        }
        let resolved = std::fs::canonicalize(&context)
            .with_context(|| format!("resolve assistant context {}", context.display()))?;
        if resolved.parent() != Some(root.as_path()) {
            bail!(
                "assistant context {} must stay directly beneath assistant_root {}",
                resolved.display(),
                root.display()
            );
        }
        Ok(Some(resolved))
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

    pub fn enabled_channel_kinds(&self) -> Result<Vec<ChannelKind>> {
        if self.channels.is_empty() {
            return Ok(vec![self.channel_kind()?]);
        }
        let mut seen = HashSet::new();
        let mut enabled = Vec::with_capacity(self.channels.len());
        for name in &self.channels {
            let kind = ChannelKind::parse(name)?;
            if !seen.insert(kind) {
                bail!("duplicate enabled channel {:?}", kind.as_str());
            }
            enabled.push(kind);
        }
        Ok(enabled)
    }

    pub fn telegram_token(&self) -> Option<String> {
        self.telegram_bot_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string)
            .or_else(|| {
                std::env::var(TELEGRAM_BOT_TOKEN_ENV)
                    .ok()
                    .map(|token| token.trim().to_string())
                    .filter(|token| !token.is_empty())
            })
    }

    pub fn route_for_message(
        &self,
        channel: &str,
        route_thread_groups: &[Vec<String>],
    ) -> Result<RouteSelection> {
        for aliases in route_thread_groups {
            for route in self.routes.iter().filter(|route| route.thread.is_some()) {
                if route.matches_threads(channel, aliases) {
                    return self.resolve_route(route);
                }
            }
        }
        for route in self.routes.iter().filter(|route| route.thread.is_none()) {
            if route.matches(channel) {
                return self.resolve_route(route);
            }
        }
        Ok(RouteSelection {
            backend: self.agent_backend()?,
        })
    }

    pub fn jobs_backend(&self) -> Result<AgentBackend> {
        AgentBackend::parse(self.jobs_agent.as_deref().unwrap_or(&self.agent))
            .context("invalid jobs_agent")
    }

    pub fn jobs_max_timeout_dur(&self) -> Result<Duration> {
        humantime::parse_duration(&self.jobs_max_timeout)
            .with_context(|| format!("invalid jobs_max_timeout {}", self.jobs_max_timeout))
    }

    // Jobs run with the backend's own permission configuration, which may
    // allow writes, so every job workdir must stay clear of Push-owned paths,
    // including the loaded config file itself.
    pub fn validate_job_workdir(&self, workdir: &Path) -> Result<()> {
        let workdir = resolved_absolute("job workdir", workdir)?;
        let protected_paths = [
            ("assistant_root", self.assistant_root.as_str()),
            ("jobs_dir", self.jobs_dir.as_str()),
            ("drafts_dir", self.drafts_dir.as_str()),
            ("jobs_run_dir", self.jobs_run_dir.as_str()),
            ("state_path", self.state_path.as_str()),
            ("database_path", self.database_path.as_str()),
            ("audit_log_path", self.audit_log_path.as_str()),
            ("config file", self.config_path.as_str()),
        ];
        for (label, protected) in protected_paths
            .into_iter()
            .filter(|(_, protected)| !protected.is_empty())
        {
            let protected = resolved_absolute(label, Path::new(protected))?;
            if paths_overlap(&workdir, &protected) {
                bail!(
                    "job workdir {} overlaps Push-owned {label} {}",
                    workdir.display(),
                    protected.display()
                );
            }
        }
        Ok(())
    }

    fn resolve_route(&self, route: &RouteRule) -> Result<RouteSelection> {
        Ok(RouteSelection {
            backend: AgentBackend::parse(&route.agent)?,
        })
    }

    pub fn required_agent_bins(&self) -> Result<Vec<&str>> {
        let mut backends = HashSet::new();
        backends.insert(self.agent_backend()?);
        let enabled = self.enabled_channel_kinds()?;
        for route in self.routes.iter().filter(|route| {
            enabled
                .iter()
                .any(|kind| route.can_match_channel(kind.as_str()))
        }) {
            backends.insert(AgentBackend::parse(&route.agent)?);
        }
        if let Some(jobs_agent) = self.jobs_agent.as_deref() {
            backends.insert(AgentBackend::parse(jobs_agent).context("invalid jobs_agent")?);
        }

        let mut bins = Vec::new();
        for backend in backends {
            bins.push(self.agent_bin(backend));
        }
        Ok(bins)
    }

    pub fn agent_bin(&self, backend: AgentBackend) -> &str {
        #[cfg(test)]
        {
            match backend {
                AgentBackend::Claude => self.agent_commands.claude.as_str(),
                AgentBackend::Codex => self.agent_commands.codex.as_str(),
                AgentBackend::Pi => self.agent_commands.pi.as_str(),
            }
        }
        #[cfg(not(test))]
        {
            backend.as_str()
        }
    }

    fn validate(&self) -> Result<()> {
        if self
            .voice_openai_api_key
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            bail!("voice.openai_api_key cannot be empty");
        }
        if !SUPPORTED_VOICE_NAMES.contains(&self.voice_name.as_str()) {
            bail!(
                "invalid voice.name {:?}; expected one of: {}",
                self.voice_name,
                SUPPORTED_VOICE_NAMES.join(", ")
            );
        }
        for channel in self.enabled_channel_kinds()? {
            match channel {
                ChannelKind::IMessage => {
                    if self.self_handles.is_empty() && self.allow_from.is_empty() {
                        bail!("set imessage.self_handles or imessage.allow_from for iMessage");
                    }
                }
                ChannelKind::Telegram => {
                    if self.telegram_allow_user_ids.is_empty()
                        && self.telegram_allow_chat_ids.is_empty()
                    {
                        bail!(
                            "set telegram.allow_user_ids or telegram.allow_chat_ids for Telegram"
                        );
                    }
                    if self
                        .telegram_bot_token
                        .as_deref()
                        .is_some_and(|v| v.trim().is_empty())
                    {
                        bail!("telegram.bot_token cannot be empty");
                    }
                }
            }
        }
        self.agent_backend()?;
        self.jobs_backend()?;
        if self.jobs_max_timeout_dur()?.is_zero() {
            bail!("jobs_max_timeout must be positive");
        }
        if self.jobs_dir.trim().is_empty()
            || self.drafts_dir.trim().is_empty()
            || self.jobs_run_dir.trim().is_empty()
        {
            bail!("jobs_dir, drafts_dir, and jobs_run_dir cannot be empty");
        }
        if self.jobs_max_workers == 0 {
            bail!("jobs_max_workers must be positive");
        }
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
        validate_protected_paths(self)?;
        self.poll_interval_dur()?;
        self.run_timeout_dur()?;
        Ok(())
    }
}

fn validate_protected_paths(cfg: &Config) -> Result<()> {
    let jobs = resolved_absolute("jobs_dir", Path::new(&cfg.jobs_dir))?;
    let drafts = resolved_absolute("drafts_dir", Path::new(&cfg.drafts_dir))?;
    let assistant = resolved_absolute("assistant_root", Path::new(&cfg.assistant_root))?;
    if paths_overlap(&jobs, &drafts) {
        bail!("jobs_dir and drafts_dir must not overlap");
    }
    if assistant.starts_with(&drafts) {
        bail!("assistant_root must not be inside drafts_dir");
    }
    for (label, value) in [
        ("state_path", cfg.state_path.as_str()),
        ("database_path", cfg.database_path.as_str()),
        ("audit_log_path", cfg.audit_log_path.as_str()),
    ] {
        let path = resolved_absolute(label, Path::new(value))?;
        if path.starts_with(&drafts) {
            bail!("{label} must not be inside drafts_dir");
        }
    }
    Ok(())
}

fn validate_runtime_outside_assistant(cfg: &Config) -> Result<()> {
    let assistant = resolved_absolute("assistant_root", Path::new(&cfg.assistant_root))?;
    for (label, value) in [
        ("drafts_dir", cfg.drafts_dir.as_str()),
        ("jobs_run_dir", cfg.jobs_run_dir.as_str()),
    ] {
        let path = resolved_absolute(label, Path::new(value))?;
        if paths_overlap(&assistant, &path) {
            bail!("{label} must stay outside assistant_root");
        }
    }
    for (label, value) in [
        ("state_path", cfg.state_path.as_str()),
        ("database_path", cfg.database_path.as_str()),
        ("audit_log_path", cfg.audit_log_path.as_str()),
    ] {
        let path = resolved_absolute(label, Path::new(value))?;
        if path.starts_with(&assistant) {
            bail!("{label} must stay outside assistant_root");
        }
    }
    Ok(())
}

pub(crate) fn validate_inline_token_location(
    config_path: &Path,
    assistant_root: &Path,
    token: Option<&str>,
) -> Result<()> {
    if token.is_some_and(|token| !token.trim().is_empty())
        && config_path.starts_with(assistant_root)
    {
        bail!(
            "config {} contains an inline Telegram token inside the Git-versioned assistant repository. Move the config outside the assistant or use TELEGRAM_BOT_TOKEN.",
            config_path.display()
        );
    }
    Ok(())
}

pub(crate) fn validate_inline_voice_key_location(
    config_path: &Path,
    assistant_root: &Path,
    key: Option<&str>,
) -> Result<()> {
    if key.is_some_and(|key| !key.trim().is_empty()) && config_path.starts_with(assistant_root) {
        bail!(
            "config {} contains an inline OpenAI API key inside the Git-versioned assistant repository. Move the config outside the assistant or use OPENAI_API_KEY.",
            config_path.display()
        );
    }
    Ok(())
}

fn validate_resolved_config_path(config: PathBuf, cfg: &Config) -> Result<String> {
    let drafts = resolved_absolute("drafts_dir", Path::new(&cfg.drafts_dir))?;
    if config.starts_with(&drafts) {
        bail!("config file must not be inside drafts_dir");
    }
    Ok(config.to_string_lossy().to_string())
}

fn resolve_assistant_root(value: &str, config_path: &Path) -> Result<PathBuf> {
    let expanded = expand_home(value);
    if expanded.trim().is_empty() {
        bail!("assistant_root cannot be empty");
    }
    let configured = Path::new(&expanded);
    let candidate = if configured.is_absolute() {
        configured.to_path_buf()
    } else {
        config_path
            .parent()
            .context("config path has no parent directory")?
            .join(configured)
    };
    resolved_absolute("assistant_root", &candidate)
}

fn normalized_absolute(label: &str, path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("{label} must be an absolute path");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => bail!("{label} cannot contain '..'"),
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    Ok(normalized)
}

fn resolved_absolute(label: &str, path: &Path) -> Result<PathBuf> {
    let normalized = normalized_absolute(label, path)?;
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .with_context(|| format!("{label} has no existing ancestor"))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .with_context(|| format!("{label} has no existing ancestor"))?;
    }
    let mut resolved = std::fs::canonicalize(existing)
        .with_context(|| format!("resolve existing ancestor for {label}"))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
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

#[derive(Debug, Deserialize, Clone)]
pub struct RouteRule {
    #[serde(default)]
    pub thread: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    pub agent: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct PrimaryDeliveryConfig {
    pub channel: String,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSelection {
    pub backend: AgentBackend,
}

impl RouteRule {
    fn can_match_channel(&self, channel: &str) -> bool {
        self.channel.as_deref().is_none_or(|value| value == channel)
    }

    fn matches_threads(&self, channel: &str, threads: &[String]) -> bool {
        if !self.can_match_channel(channel) {
            return false;
        }
        self.thread
            .as_deref()
            .is_some_and(|value| threads.iter().any(|thread| value == thread))
    }

    fn matches(&self, channel: &str) -> bool {
        self.can_match_channel(channel) && self.thread.is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    pub fn as_str(self) -> &'static str {
        match self {
            Self::IMessage => "imessage",
            Self::Telegram => "telegram",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBackend {
    Claude,
    Codex,
    Pi,
}

impl AgentBackend {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(AgentBackend::Claude),
            "codex" => Ok(AgentBackend::Codex),
            "pi" => Ok(AgentBackend::Pi),
            other => bail!("invalid agent {other:?}; expected \"claude\", \"codex\", or \"pi\""),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AgentBackend::Claude => "claude",
            AgentBackend::Codex => "codex",
            AgentBackend::Pi => "pi",
        }
    }
}

fn default_db_path() -> String {
    "~/Library/Messages/chat.db".to_string()
}
fn default_channel() -> String {
    "imessage".to_string()
}
fn default_poll_interval() -> String {
    "3s".to_string()
}
fn default_run_timeout() -> String {
    "10m".to_string()
}
fn default_agent() -> String {
    "claude".to_string()
}
fn default_voice_name() -> String {
    DEFAULT_VOICE_NAME.to_string()
}
fn default_jobs_dir() -> String {
    "~/.push/jobs".to_string()
}

fn default_drafts_dir() -> String {
    "~/.push/drafts".to_string()
}
fn default_jobs_max_timeout() -> String {
    "30m".to_string()
}
fn default_jobs_run_dir() -> String {
    "~/.push/run".to_string()
}
fn default_jobs_max_workers() -> usize {
    2
}
fn default_state_path() -> String {
    "~/.push/state.json".to_string()
}
fn default_audit_log_path() -> String {
    "~/.push/audit.jsonl".to_string()
}
fn default_database_path() -> String {
    "~/.push/push.db".to_string()
}
fn default_assistant_dir() -> String {
    "~/.push".to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_dir;

    fn config() -> Config {
        let root = temp_dir("config-draft-boundary");
        Config {
            channel: "imessage".to_string(),
            channels: Vec::new(),
            primary_delivery: None,
            db_path: root.join("chat.db").to_string_lossy().to_string(),
            poll_interval: "1s".to_string(),
            run_timeout: "1s".to_string(),
            self_handles: vec!["me@example.com".to_string()],
            allow_from: Vec::new(),
            telegram_bot_token: None,
            telegram_allow_user_ids: Vec::new(),
            telegram_allow_chat_ids: Vec::new(),
            voice_openai_api_key: None,
            voice_name: DEFAULT_VOICE_NAME.to_string(),
            agent: "codex".to_string(),
            routes: Vec::new(),
            assistant_root: root.to_string_lossy().to_string(),
            jobs_dir: root.join("jobs").to_string_lossy().to_string(),
            drafts_dir: root.join("drafts").to_string_lossy().to_string(),
            jobs_agent: None,
            jobs_max_timeout: "30m".to_string(),
            jobs_run_dir: root.join("run").to_string_lossy().to_string(),
            jobs_max_workers: 2,
            state_path: root.join("state.json").to_string_lossy().to_string(),
            audit_log_path: root.join("audit.jsonl").to_string_lossy().to_string(),
            database_path: root.join("push.db").to_string_lossy().to_string(),
            audit_log_content: false,
            config_path: String::new(),
            agent_commands: AgentCommands::default(),
            assistant_dir: root.to_string_lossy().to_string(),
        }
    }

    #[test]
    fn pi_parses_and_is_selectable_for_default_routes_and_jobs() {
        let mut cfg = config();
        cfg.agent = "pi".to_string();
        cfg.jobs_agent = Some("pi".to_string());
        cfg.routes = vec![RouteRule {
            thread: Some("imessage:chat:pi".to_string()),
            channel: Some("imessage".to_string()),
            agent: "pi".to_string(),
        }];

        assert_eq!(AgentBackend::parse("pi").unwrap(), AgentBackend::Pi);
        assert_eq!(AgentBackend::Pi.as_str(), "pi");
        assert_eq!(cfg.agent_backend().unwrap(), AgentBackend::Pi);
        assert_eq!(cfg.jobs_backend().unwrap(), AgentBackend::Pi);
        assert_eq!(
            cfg.route_for_message("imessage", &[vec!["imessage:chat:pi".to_string()]])
                .unwrap()
                .backend,
            AgentBackend::Pi
        );
        assert_eq!(cfg.required_agent_bins().unwrap(), vec!["pi"]);
    }

    #[test]
    fn pi_binary_defaults_to_pi_when_loading_toml() {
        let cfg: Config = toml::from_str("agent = 'pi'").unwrap();

        assert_eq!(cfg.agent_backend().unwrap(), AgentBackend::Pi);
        assert_eq!(cfg.agent_bin(AgentBackend::Pi), "pi");
    }

    #[test]
    fn chat_run_timeout_defaults_to_ten_minutes_and_accepts_an_override() {
        let default: Config = toml::from_str("agent = 'codex'").unwrap();
        let overridden: Config = toml::from_str("agent = 'codex'\nrun_timeout = '45s'").unwrap();

        assert_eq!(default.run_timeout, "10m");
        assert_eq!(default.run_timeout_dur().unwrap(), Duration::from_secs(600));
        assert_eq!(
            overridden.run_timeout_dur().unwrap(),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn pi_binary_is_only_required_when_selected() {
        let mut cfg = config();
        assert_eq!(cfg.required_agent_bins().unwrap(), vec!["codex"]);

        cfg.routes.push(RouteRule {
            thread: None,
            channel: Some("telegram".to_string()),
            agent: "pi".to_string(),
        });
        assert_eq!(cfg.required_agent_bins().unwrap(), vec!["codex"]);

        cfg.jobs_agent = Some("pi".to_string());
        let mut bins = cfg.required_agent_bins().unwrap();
        bins.sort_unstable();
        assert_eq!(bins, vec!["codex", "pi"]);
    }

    #[test]
    fn rejects_uncontained_routes_jobs_and_protected_path_overlap() {
        let mut cfg = config();
        assert!(cfg.validate().is_ok());

        cfg.drafts_dir = format!("{}/drafts", cfg.jobs_dir);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("must not overlap"));

        let mut cfg = config();
        cfg.assistant_root = format!("{}/identity", cfg.drafts_dir);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("assistant_root must not be inside drafts_dir"));
    }

    #[test]
    fn job_workdir_must_not_contain_the_loaded_config_file() {
        let mut cfg = config();
        let workdir = crate::test_support::temp_dir("config-shield-workdir");
        cfg.config_path = workdir.join("config.toml").to_string_lossy().to_string();

        let error = cfg.validate_job_workdir(&workdir).unwrap_err();
        assert!(error.to_string().contains("config file"));

        let sibling = crate::test_support::temp_dir("config-shield-sibling");
        assert!(cfg.validate_job_workdir(&sibling).is_ok());
        let _ = std::fs::remove_dir_all(workdir);
        let _ = std::fs::remove_dir_all(sibling);
    }

    #[cfg(unix)]
    #[test]
    fn backend_context_rejects_a_symlink_outside_the_assistant() {
        use std::os::unix::fs::symlink;

        let mut cfg = config();
        let assistant = crate::test_support::temp_dir("config-context-assistant");
        let outside = crate::test_support::temp_dir("config-context-outside");
        symlink(&outside, assistant.join("context")).unwrap();
        cfg.assistant_root = assistant.to_string_lossy().to_string();
        cfg.assistant_dir = cfg.assistant_root.clone();

        let error = cfg.backend_context_dir().unwrap_err();

        assert!(error.to_string().contains("not a symlink"));
        let _ = std::fs::remove_dir_all(assistant);
        let _ = std::fs::remove_dir_all(outside);
    }
}
