//! Assistant repository scaffolding behind `push init`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use crate::config::{
    validate_inline_slack_token_location, validate_inline_token_location,
    validate_inline_voice_key_location,
};
use crate::paths::PushPaths;
use crate::util::expand_home;

const SOUL: &str = r#"# SOUL

You are my personal assistant. Be calm, direct, practical, and honest.

## Working style

- Ask when a decision is important and genuinely unclear.
- Protect private information and confirm before external side effects.
- Prefer concise answers, but include the evidence needed to trust them.
"#;

const AGENTS: &str = r#"# Assistant repository instructions

- Treat `SOUL.md` as user-owned identity. Do not edit it unless the user asks.
- Use `context/` for durable user context and working notes.
- Treat `evals/` as user-owned evaluation criteria. Do not edit them during evaluation.
- Store job runbooks in `jobs/`. Create or update them directly when the user asks, then run `push job validate`. Say when an enabled schedule is saved but still awaiting Push's separate owner review.
- Keep secrets, sessions, databases, logs, and other runtime state outside this repository.
"#;

const CLAUDE: &str = "@AGENTS.md\n";

const README: &str = r#"# Assistant

This Git repository contains the durable, user-owned parts of one Push assistant.

- `SOUL.md` defines the assistant's identity and working style.
- `AGENTS.md` contains shared agent instructions; `CLAUDE.md` references it.
- `context/` contains durable context the assistant may read and update.
- `evals/` contains reusable agent evaluation criteria.
- `jobs/` contains installed Push job runbooks.
- `skills/` contains reusable capabilities.

You own `SOUL.md`, `AGENTS.md`, context, evals, jobs, and any skills you add. Push manages only `skills/push/` and its `push` discovery links under `.agents/skills/` and `.claude/skills/`; Codex and Pi share the `.agents/skills/` path. Rerun `push init` after an upgrade to refresh an unmodified managed skill. Push never silently replaces a modified managed copy.

Push owns channels, scheduling, history, security, and delivery outside this repository. The configured agent runtime owns skill discovery and execution, global skills, MCP servers, permissions, and authentication. Chats preserve configured agent permissions. Codex and Claude jobs bypass interactive permissions so unattended work can finish.
"#;

const CONTEXT_README: &str = r#"# Context

Store durable facts and working context here when they should be available across conversations.

Good examples include preferences, active projects, people, recurring processes, and reference notes. Keep secrets out of this repository. Start with small, focused Markdown files and update or remove stale information.
"#;

const PUSH_SKILL: &str = include_str!("../assistant/skills/push/SKILL.md");
const PUSH_SKILL_VERSION: u32 = 3;
const PUSH_SKILL_LINK: &str = "../../skills/push";
const PUSH_SKILL_MANIFEST: &str = ".push-managed.json";
const PUSH_SKILL_PROVIDERS: [&str; 2] = [".agents", ".claude"];

const DEFAULT_CONFIG: &str = r#"# Telegram quick start.
channel = "telegram"
agent = "codex"

[telegram]
# Paste the token from BotFather here.
bot_token = ""
# Replace this with your numeric Telegram user ID.
allow_user_ids = []
"#;

#[derive(Debug)]
pub struct InitResult {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub git_initialized: bool,
}

pub fn init(requested_path: &str, config_path: &str) -> Result<InitResult> {
    let paths = PushPaths::discover()?;
    let requested = expand_home(requested_path);
    if requested.starts_with('~') {
        bail!("cannot expand assistant path {requested_path:?}; set HOME or use an absolute path");
    }
    let target = absolute_path(Path::new(&requested)).context("resolve assistant path")?;
    let expanded_config = expand_home(config_path);
    if expanded_config.starts_with('~') {
        bail!("cannot expand config path {config_path:?}; set HOME or use an absolute path");
    }
    let config_path = absolute_path(Path::new(&expanded_config)).context("resolve config path")?;
    let existing_config = inspect_config(&config_path, &target, &paths)?;

    prepare_target(&target, &config_path, existing_config)?;
    let root = fs::canonicalize(&target)
        .with_context(|| format!("resolve assistant root {}", target.display()))?;
    scaffold(&root)?;
    let git_initialized = initialize_git(&root)?;
    persist_root(&config_path, &root, existing_config)?;
    if inspect_config(&config_path, &root, &paths)? != ConfigState::MatchingRoot {
        bail!(
            "assistant validation failed: {} did not persist assistant_root",
            config_path.display()
        );
    }
    validate_scaffold(&root)?;

    Ok(InitResult {
        root,
        config_path,
        git_initialized,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigState {
    MissingFile,
    MissingRoot,
    MatchingRoot,
}

fn inspect_config(config_path: &Path, target: &Path, paths: &PushPaths) -> Result<ConfigState> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validate_runtime_boundary(None, target, paths)?;
            return Ok(ConfigState::MissingFile);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read config {}", config_path.display()))
        }
    };
    let value: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse config {}", config_path.display()))?;
    let table = value.as_table().context("config must be a TOML table")?;
    validate_config_secrets(config_path, target, table)?;
    validate_runtime_boundary(Some(table), target, paths)?;
    if table.contains_key("assistant_dir") || table.contains_key("jobs_dir") {
        bail!(
            "{} uses legacy assistant_dir or jobs_dir settings. Move SOUL.md, context, and jobs under one assistant directory, replace those settings with assistant_root, then rerun push init.",
            config_path.display()
        );
    }
    let Some(value) = table.get("assistant_root") else {
        return Ok(ConfigState::MissingRoot);
    };
    let configured = value.as_str().context("assistant_root must be a string")?;
    let configured = configured_root(config_path, configured)?;
    let target = resolve_existing_or_lexical(target)?;
    if configured != target {
        bail!(
            "{} already configures assistant_root = {}. Push supports one assistant; use that directory or a different --config file.",
            config_path.display(),
            configured.display()
        );
    }
    Ok(ConfigState::MatchingRoot)
}

fn validate_config_secrets(config_path: &Path, target: &Path, config: &toml::Table) -> Result<()> {
    let config_path = resolve_existing_or_lexical(config_path)?;
    let assistant = resolve_existing_or_lexical(target)?;
    let flat_token = config
        .get("telegram_bot_token")
        .and_then(toml::Value::as_str);
    let nested_token = config
        .get("telegram")
        .and_then(toml::Value::as_table)
        .and_then(|telegram| telegram.get("bot_token"))
        .and_then(toml::Value::as_str);
    for token in [flat_token, nested_token] {
        validate_inline_token_location(&config_path, &assistant, token)?;
    }
    let slack = config.get("slack").and_then(toml::Value::as_table);
    let flat_app_token = config.get("slack_app_token").and_then(toml::Value::as_str);
    let flat_bot_token = config.get("slack_bot_token").and_then(toml::Value::as_str);
    let nested_app_token = slack
        .and_then(|slack| slack.get("app_token"))
        .and_then(toml::Value::as_str);
    let nested_bot_token = slack
        .and_then(|slack| slack.get("bot_token"))
        .and_then(toml::Value::as_str);
    for (app_token, bot_token) in [
        (flat_app_token, flat_bot_token),
        (nested_app_token, nested_bot_token),
    ] {
        validate_inline_slack_token_location(&config_path, &assistant, app_token, bot_token)?;
    }
    let flat_voice_key = config
        .get("voice_openai_api_key")
        .and_then(toml::Value::as_str);
    let nested_voice_key = config
        .get("voice")
        .and_then(toml::Value::as_table)
        .and_then(|voice| voice.get("openai_api_key"))
        .and_then(toml::Value::as_str);
    for key in [flat_voice_key, nested_voice_key] {
        validate_inline_voice_key_location(&config_path, &assistant, key)?;
    }
    Ok(())
}

fn validate_runtime_boundary(
    config: Option<&toml::Table>,
    target: &Path,
    defaults: &PushPaths,
) -> Result<()> {
    let assistant = resolve_existing_or_lexical(target)?;
    let runtime = defaults.clone().with_overrides(
        configured_override(config, "state_path")?,
        configured_override(config, "database_path")?,
        configured_override(config, "audit_log_path")?,
        configured_override(config, "jobs_run_dir")?,
    )?;
    let push_home = resolve_existing_or_lexical(&runtime.root)?;
    if assistant.starts_with(&push_home) || push_home.starts_with(&assistant) {
        bail!(
            "assistant_root {} must stay outside Push home {}; choose a separate assistant repository or set PUSH_HOME to a separate runtime directory",
            assistant.display(),
            push_home.display()
        );
    }
    let jobs_run = resolve_existing_or_lexical(&runtime.jobs_run)?;
    if assistant.starts_with(&jobs_run) || jobs_run.starts_with(&assistant) {
        bail!("jobs_run_dir must stay outside assistant_root; choose a separate assistant path or update jobs_run_dir");
    }
    for (key, path) in [
        ("state_path", runtime.state.as_path()),
        ("database_path", runtime.database.as_path()),
        ("audit_log_path", runtime.audit.as_path()),
        ("Slack inbox", runtime.inbox.as_path()),
        ("cache directory", runtime.cache.as_path()),
    ] {
        let runtime = resolve_existing_or_lexical(path)?;
        if runtime.starts_with(&assistant) {
            bail!("{key} must stay outside assistant_root; choose a separate assistant path or update {key}");
        }
    }
    Ok(())
}

fn configured_override<'a>(config: Option<&'a toml::Table>, key: &str) -> Result<Option<&'a str>> {
    config
        .and_then(|table| table.get(key))
        .map(|value| {
            let value = value
                .as_str()
                .with_context(|| format!("{key} must be a string"))?;
            let expanded = expand_home(value);
            if expanded.starts_with('~') {
                bail!("cannot expand configured {key} {value:?}");
            }
            if !Path::new(&expanded).is_absolute() {
                bail!("{key} must be an absolute path or start with ~");
            }
            Ok(value)
        })
        .transpose()
}

fn configured_root(config_path: &Path, configured: &str) -> Result<PathBuf> {
    let expanded = expand_home(configured);
    if expanded.starts_with('~') {
        bail!("cannot expand configured assistant_root {configured:?}");
    }
    let path = Path::new(&expanded);
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_path
            .parent()
            .context("config path has no parent")?
            .join(path)
    };
    resolve_existing_or_lexical(&candidate)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("read current directory")?
            .join(path)
    };
    normalize(&path)
}

fn normalize(path: &Path) -> Result<PathBuf> {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str())
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    bail!("path {} escapes its filesystem root", path.display());
                }
            }
        }
    }
    Ok(normalized)
}

fn resolve_existing_or_lexical(path: &Path) -> Result<PathBuf> {
    let normalized = normalize(path)?;
    let mut existing = normalized.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .with_context(|| format!("{} has no existing ancestor", path.display()))?;
        missing.push(name.to_os_string());
        existing = existing
            .parent()
            .with_context(|| format!("{} has no existing ancestor", path.display()))?;
    }
    let mut resolved = fs::canonicalize(existing)
        .with_context(|| format!("resolve existing ancestor for {}", path.display()))?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn prepare_target(target: &Path, config_path: &Path, config_state: ConfigState) -> Result<()> {
    if target.exists() {
        if !target.is_dir() {
            bail!("assistant target {} is not a directory", target.display());
        }
        let entries = fs::read_dir(target)
            .with_context(|| format!("inspect assistant target {}", target.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        let resolved_config = resolve_existing_or_lexical(config_path)?;
        match fs::symlink_metadata(target.join(".git")) {
            Ok(_) => {
                let resolved_target = fs::canonicalize(target)
                    .with_context(|| format!("resolve assistant target {}", target.display()))?;
                verify_git_root(&resolved_target)
                    .context("validate existing Git metadata before init")?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect Git metadata under {}", target.display()))
            }
        }
        if entries.is_empty()
            || entries.iter().all(|entry| {
                entry.file_name() == ".git"
                    || resolve_existing_or_lexical(&entry.path())
                        .is_ok_and(|path| path == resolved_config)
            })
        {
            return Ok(());
        }
        if !valid_assistant_structure(target) && config_state != ConfigState::MatchingRoot {
            bail!(
                "assistant target {} is non-empty but is not a complete assistant repository. Choose an empty directory or a valid assistant containing SOUL.md, AGENTS.md, README.md, context/README.md, and jobs/.",
                target.display()
            );
        }
        return Ok(());
    }
    fs::create_dir_all(target)
        .with_context(|| format!("create assistant directory {}", target.display()))
}

fn scaffold(root: &Path) -> Result<()> {
    create_directory(&root.join("context"))?;
    create_directory(&root.join("evals"))?;
    create_directory(&root.join("jobs"))?;
    create_file(&root.join("SOUL.md"), SOUL)?;
    create_file(&root.join("AGENTS.md"), AGENTS)?;
    create_file(&root.join("CLAUDE.md"), CLAUDE)?;
    create_file(&root.join("README.md"), README)?;
    create_file(&root.join("context/README.md"), CONTEXT_README)?;
    install_push_skill(root)?;
    Ok(())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct ManagedSkillManifest {
    version: u32,
    sha256: String,
}

fn install_push_skill(root: &Path) -> Result<()> {
    let skills = root.join("skills");
    let skill_dir = skills.join("push");
    let skill_path = skill_dir.join("SKILL.md");
    let manifest_path = skill_dir.join(PUSH_SKILL_MANIFEST);
    create_directory(&skills)?;
    create_directory(&skill_dir)?;

    let actual = read_optional_regular_file(&skill_path)?;
    let manifest = read_skill_manifest(&manifest_path)?;
    match manifest {
        Some(manifest) => {
            if manifest.version > PUSH_SKILL_VERSION {
                bail!(
                    "{} is managed by Push skill version {}, which is newer than this Push binary supports (version {}). Upgrade Push instead of downgrading the skill.",
                    skill_path.display(),
                    manifest.version,
                    PUSH_SKILL_VERSION
                );
            }
            match actual.as_deref() {
                Some(actual) if actual == PUSH_SKILL.as_bytes() => {
                    if manifest.version != PUSH_SKILL_VERSION
                        || manifest.sha256 != sha256(PUSH_SKILL.as_bytes())
                    {
                        write_skill_manifest(&manifest_path)?;
                    }
                }
                Some(actual) if sha256(actual) != manifest.sha256 => {
                    return Err(user_modified_skill_error(&skill_path));
                }
                Some(_) => {
                    if manifest.version == PUSH_SKILL_VERSION {
                        return Err(user_modified_skill_error(&skill_path));
                    }
                    write_atomic_file(&skill_path, PUSH_SKILL.as_bytes())?;
                    write_skill_manifest(&manifest_path)?;
                }
                None => {
                    write_atomic_file(&skill_path, PUSH_SKILL.as_bytes())?;
                    write_skill_manifest(&manifest_path)?;
                }
            }
        }
        None => match actual.as_deref() {
            None => {
                write_atomic_file(&skill_path, PUSH_SKILL.as_bytes())?;
                write_skill_manifest(&manifest_path)?;
            }
            Some(actual) if actual == PUSH_SKILL.as_bytes() => {
                write_skill_manifest(&manifest_path)?;
            }
            Some(_) => {
                bail!(
                    "Push found an existing unmanaged skill at {} and left it unchanged. Rename that skill, or move it to a different skill directory, then rerun `push init`.",
                    skill_path.display()
                );
            }
        },
    }

    for provider in PUSH_SKILL_PROVIDERS {
        let provider_skills = root.join(provider).join("skills");
        create_directory(&root.join(provider))?;
        create_directory(&provider_skills)?;
        create_skill_link(&provider_skills.join("push"))?;
    }
    Ok(())
}

fn read_optional_regular_file(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::read(path)
            .map(Some)
            .with_context(|| format!("read {}", path.display())),
        Ok(_) => bail!(
            "{} must be a regular file inside the assistant repository; Push left it unchanged",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn read_skill_manifest(path: &Path) -> Result<Option<ManagedSkillManifest>> {
    let Some(raw) = read_optional_regular_file(path)? else {
        return Ok(None);
    };
    serde_json::from_slice(&raw)
        .with_context(|| {
            format!(
                "parse managed skill metadata {}. Push left the skill unchanged; restore this file or move the skill before rerunning `push init`",
                path.display()
            )
        })
        .map(Some)
}

fn write_skill_manifest(path: &Path) -> Result<()> {
    let manifest = ManagedSkillManifest {
        version: PUSH_SKILL_VERSION,
        sha256: sha256(PUSH_SKILL.as_bytes()),
    };
    let mut raw = serde_json::to_vec_pretty(&manifest).context("serialize Push skill metadata")?;
    raw.push(b'\n');
    write_atomic_file(path, &raw)
}

fn sha256(contents: &[u8]) -> String {
    format!("{:x}", Sha256::digest(contents))
}

fn user_modified_skill_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "Push found a user-modified managed skill at {} and left it unchanged. Move your changes to a differently named skill or restore the managed copy, then rerun `push init`.",
        path.display()
    )
}

fn write_atomic_file(path: &Path, contents: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                bail!(
                    "{} must be a regular file inside the assistant repository; Push left it unchanged",
                    path.display()
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect managed file {}", path.display()))
        }
    }
    let parent = path.parent().context("managed file path has no parent")?;
    let name = path
        .file_name()
        .context("managed file path has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.push-init-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create temporary managed file {}", temporary.display()))?;
        file.write_all(contents)
            .with_context(|| format!("write temporary managed file {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary managed file {}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("replace managed file {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_skill_link(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(path)
                .with_context(|| format!("read skill link {}", path.display()))?;
            if target == Path::new(PUSH_SKILL_LINK) {
                return Ok(());
            }
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(PUSH_SKILL_LINK, path)
                    .with_context(|| format!("create skill link {}", path.display()))?;
                return Ok(());
            }
            #[cfg(not(unix))]
            bail!("Push project skills require symbolic link support");
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect skill link {}", path.display()))
        }
    }
    bail!(
        "Push found a conflicting provider skill at {} and left it unchanged. Move or remove that path, then rerun `push init`.",
        path.display()
    )
}

fn create_directory(path: &Path) -> Result<()> {
    if path.exists() {
        if fs::symlink_metadata(path)?.file_type().is_dir() {
            return Ok(());
        }
        bail!(
            "cannot create directory {} because a file or symlink exists there",
            path.display()
        );
    }
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))
}

fn create_file(path: &Path, contents: &str) -> Result<()> {
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let file_type = fs::symlink_metadata(path)?.file_type();
            if file_type.is_file()
                || (file_type.is_symlink() && fs::metadata(path).is_ok_and(|meta| meta.is_file()))
            {
                Ok(())
            } else {
                bail!(
                    "cannot create file {} because it is not a regular file or a symlink to one",
                    path.display()
                )
            }
        }
        Err(error) => Err(error).with_context(|| format!("create {}", path.display())),
    }
}

fn initialize_git(root: &Path) -> Result<bool> {
    if root.join(".git").exists() {
        verify_git_root(root)?;
        return Ok(false);
    }
    let output = Command::new("git")
        .arg("init")
        .arg("--quiet")
        .arg(root)
        .output()
        .context("run git init")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git init failed for {}: {}", root.display(), stderr.trim());
    }
    verify_git_root(root)?;
    Ok(true)
}

fn verify_git_root(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("verify Git repository")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "{} has .git metadata but is not a valid Git repository: {}",
            root.display(),
            stderr.trim()
        );
    }
    let reported = String::from_utf8(output.stdout).context("Git root is not UTF-8")?;
    let reported = fs::canonicalize(reported.trim())
        .with_context(|| format!("resolve Git root {}", reported.trim()))?;
    if reported != root {
        bail!(
            "Git repository root {} does not match assistant root {}",
            reported.display(),
            root.display()
        );
    }
    Ok(())
}

fn persist_root(config_path: &Path, root: &Path, state: ConfigState) -> Result<()> {
    if state == ConfigState::MatchingRoot {
        return Ok(());
    }
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }
    let config_parent = config_path
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok());
    let persisted = if config_parent.as_deref() == Some(root) {
        ".".to_string()
    } else {
        root.to_string_lossy().to_string()
    };
    let existing = match fs::read_to_string(config_path) {
        Ok(existing) => existing,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && state == ConfigState::MissingFile =>
        {
            DEFAULT_CONFIG.to_string()
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read config {}", config_path.display()))
        }
    };
    let mut document = existing
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("parse config {}", config_path.display()))?;
    document["assistant_root"] = toml_edit::value(persisted);
    write_config(config_path, document.to_string().as_bytes())
}

fn write_config(config_path: &Path, contents: &[u8]) -> Result<()> {
    let destination = match fs::symlink_metadata(config_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => fs::canonicalize(config_path)
            .with_context(|| format!("resolve config symlink {}", config_path.display()))?,
        Ok(_) => config_path.to_path_buf(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => config_path.to_path_buf(),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect config {}", config_path.display()))
        }
    };
    let parent = destination
        .parent()
        .context("config path has no parent directory")?;
    let name = destination
        .file_name()
        .context("config path has no file name")?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.push-init-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create temporary config {}", temporary.display()))?;
        if let Ok(metadata) = fs::metadata(&destination) {
            fs::set_permissions(&temporary, metadata.permissions()).with_context(|| {
                format!("preserve config permissions for {}", destination.display())
            })?;
        } else {
            crate::util::restrict_permissions(&temporary, false).with_context(|| {
                format!("restrict config permissions for {}", destination.display())
            })?;
        }
        file.write_all(contents)
            .with_context(|| format!("write temporary config {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temporary config {}", temporary.display()))?;
        fs::rename(&temporary, &destination)
            .with_context(|| format!("replace config {}", destination.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn valid_assistant_structure(root: &Path) -> bool {
    [
        root.join("SOUL.md"),
        root.join("AGENTS.md"),
        root.join("README.md"),
        root.join("context/README.md"),
    ]
    .iter()
    .all(|path| fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file()))
        && [root.join("context"), root.join("jobs")]
            .iter()
            .all(|path| {
                fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
            })
}

fn validate_scaffold(root: &Path) -> Result<()> {
    if !valid_assistant_structure(root) {
        bail!(
            "assistant validation failed: {} does not contain the conventional structure",
            root.display()
        );
    }
    verify_git_root(root).context("assistant validation failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{temp_dir, temp_path};

    #[test]
    fn creates_structure_initializes_git_and_persists_canonical_root() {
        let parent = temp_dir("assistant-init");
        let target = parent.join("chosen");
        let config = parent.join("push.toml");

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert_eq!(result.root, fs::canonicalize(&target).unwrap());
        assert!(result.git_initialized);
        assert!(target.join("SOUL.md").is_file());
        assert!(target.join("AGENTS.md").is_file());
        assert_eq!(
            fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
            CLAUDE
        );
        assert!(target.join("README.md").is_file());
        assert!(target.join("context/README.md").is_file());
        assert!(target.join("evals").is_dir());
        assert!(target.join("jobs").is_dir());
        assert_eq!(fs::read_dir(target.join("jobs")).unwrap().count(), 0);
        assert_eq!(
            fs::read_to_string(target.join("skills/push/SKILL.md")).unwrap(),
            PUSH_SKILL
        );
        let manifest: ManagedSkillManifest = serde_json::from_str(
            &fs::read_to_string(target.join("skills/push/.push-managed.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.version, PUSH_SKILL_VERSION);
        assert_eq!(manifest.sha256, sha256(PUSH_SKILL.as_bytes()));
        assert_push_skill_links(&target);
        assert!(target.join(".git").exists());
        let raw = fs::read_to_string(config).unwrap();
        assert!(raw.contains(&format!(
            "assistant_root = {}",
            toml::Value::String(result.root.to_string_lossy().to_string())
        )));
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn configured_partial_assistant_with_broken_git_link_is_not_scaffolded() {
        use std::os::unix::fs::symlink;

        let parent = temp_dir("assistant-configured-partial-broken-git-link");
        let target = parent.join("assistant");
        let missing_git = parent.join("missing-git");
        let config = parent.join("push.toml");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("SOUL.md"), "Existing identity\n").unwrap();
        symlink(&missing_git, target.join(".git")).unwrap();
        fs::write(
            &config,
            format!(
                "assistant_root = {}\n",
                toml::Value::String(target.to_string_lossy().to_string())
            ),
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("validate existing Git metadata"));
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "Existing identity\n"
        );
        assert!(!target.join("AGENTS.md").exists());
        assert!(!target.join("README.md").exists());
        assert!(!target.join("context").exists());
        assert!(!target.join("jobs").exists());
        assert!(!target.join("skills").exists());
        assert!(!missing_git.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn repeat_initialization_preserves_user_files_and_configuration() {
        let parent = temp_dir("assistant-reinit");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        fs::write(target.join("SOUL.md"), "My identity\n").unwrap();
        fs::write(target.join("AGENTS.md"), "My repository rules\n").unwrap();
        fs::write(target.join("context/private.md"), "Keep me\n").unwrap();
        fs::remove_file(target.join("CLAUDE.md")).unwrap();
        let config_before = fs::read_to_string(&config).unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert!(!result.git_initialized);
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "My identity\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("AGENTS.md")).unwrap(),
            "My repository rules\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("context/private.md")).unwrap(),
            "Keep me\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("CLAUDE.md")).unwrap(),
            CLAUDE
        );
        assert_eq!(
            fs::read_to_string(target.join("skills/push/SKILL.md")).unwrap(),
            PUSH_SKILL
        );
        assert_push_skill_links(&target);
        assert_eq!(fs::read_to_string(config).unwrap(), config_before);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refreshes_an_unmodified_older_managed_skill() {
        let parent = temp_dir("assistant-skill-upgrade");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        let skill_path = target.join("skills/push/SKILL.md");
        let manifest_path = target.join("skills/push/.push-managed.json");
        let old_skill = b"---\nname: push\ndescription: Old managed Push skill.\n---\n";
        fs::write(&skill_path, old_skill).unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&ManagedSkillManifest {
                version: 0,
                sha256: sha256(old_skill),
            })
            .unwrap(),
        )
        .unwrap();

        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert_eq!(fs::read_to_string(skill_path).unwrap(), PUSH_SKILL);
        let manifest: ManagedSkillManifest =
            serde_json::from_str(&fs::read_to_string(manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.version, PUSH_SKILL_VERSION);
        assert_eq!(manifest.sha256, sha256(PUSH_SKILL.as_bytes()));
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_to_overwrite_a_user_modified_managed_skill() {
        let parent = temp_dir("assistant-skill-modified");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        let skill_path = target.join("skills/push/SKILL.md");
        let modified = format!("{PUSH_SKILL}\nUser addition.\n");
        fs::write(&skill_path, &modified).unwrap();
        fs::write(target.join("SOUL.md"), "User identity.\n").unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("user-modified managed skill"));
        assert!(error.to_string().contains("left it unchanged"));
        assert!(error.to_string().contains("differently named skill"));
        assert_eq!(fs::read_to_string(skill_path).unwrap(), modified);
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "User identity.\n"
        );
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn recreates_missing_provider_directories_and_links() {
        let parent = temp_dir("assistant-skill-provider-directories");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        for provider in PUSH_SKILL_PROVIDERS {
            fs::remove_dir_all(target.join(provider)).unwrap();
        }

        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert_push_skill_links(&target);
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn managed_skill_paths_stay_inside_the_assistant() {
        use std::os::unix::fs::symlink;

        let parent = temp_dir("assistant-skill-path-safety");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        assert_push_skill_links(&target);
        for provider in PUSH_SKILL_PROVIDERS {
            let link = target.join(provider).join("skills/push");
            assert_eq!(fs::read_link(&link).unwrap(), Path::new(PUSH_SKILL_LINK));
            assert_eq!(
                fs::canonicalize(link).unwrap(),
                fs::canonicalize(target.join("skills/push")).unwrap()
            );
        }

        let outside = parent.join("outside-skill.md");
        fs::write(&outside, "outside\n").unwrap();
        let skill_path = target.join("skills/push/SKILL.md");
        fs::remove_file(&skill_path).unwrap();
        symlink(&outside, &skill_path).unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("must be a regular file"));
        assert_eq!(fs::read_to_string(outside).unwrap(), "outside\n");
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_to_replace_a_conflicting_provider_skill() {
        use std::os::unix::fs::symlink;

        let parent = temp_dir("assistant-skill-provider-conflict");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        let link = target.join(".claude/skills/push");
        fs::remove_file(&link).unwrap();
        symlink("../../skills/user-push", &link).unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("conflicting provider skill"));
        assert_eq!(
            fs::read_link(&link).unwrap(),
            Path::new("../../skills/user-push")
        );
        let _ = fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn repeat_initialization_preserves_symlinked_claude_instructions() {
        use std::os::unix::fs::symlink;

        let parent = temp_dir("assistant-reinit-claude-symlink");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        fs::remove_file(target.join("CLAUDE.md")).unwrap();
        symlink("AGENTS.md", target.join("CLAUDE.md")).unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert!(!result.git_initialized);
        assert!(fs::symlink_metadata(target.join("CLAUDE.md"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(target.join("CLAUDE.md")).unwrap(),
            Path::new("AGENTS.md")
        );
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_partial_assistant_layouts_without_completing_them() {
        let parent = temp_dir("assistant-partial");
        for name in ["soul-only", "agents-only", "context-only"] {
            let target = parent.join(name);
            fs::create_dir_all(&target).unwrap();
            match name {
                "soul-only" => fs::write(target.join("SOUL.md"), "Existing soul").unwrap(),
                "agents-only" => fs::write(target.join("AGENTS.md"), "Existing rules").unwrap(),
                "context-only" => fs::create_dir(target.join("context")).unwrap(),
                _ => unreachable!(),
            }
            let config = parent.join(format!("{name}.toml"));

            let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

            assert!(error.to_string().contains("not a complete assistant"));
            assert!(!target.join("README.md").exists());
            assert!(!target.join("jobs").exists());
            assert!(!config.exists());
        }
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn completes_a_partial_configured_assistant_without_overwriting_user_files() {
        let parent = temp_dir("assistant-configured-partial");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::create_dir_all(target.join("context")).unwrap();
        fs::create_dir(target.join("jobs")).unwrap();
        fs::write(target.join("SOUL.md"), "Existing identity\n").unwrap();
        fs::write(target.join("context/private.md"), "Existing context\n").unwrap();
        fs::write(
            &config,
            format!(
                "assistant_root = {}\n",
                toml::Value::String(target.to_string_lossy().to_string())
            ),
        )
        .unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert_eq!(result.root, fs::canonicalize(&target).unwrap());
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "Existing identity\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("context/private.md")).unwrap(),
            "Existing context\n"
        );
        assert!(target.join("AGENTS.md").is_file());
        assert!(target.join("README.md").is_file());
        assert!(target.join("context/README.md").is_file());
        assert!(target.join("evals").is_dir());
        assert!(target.join("skills/push/SKILL.md").is_file());
        assert_push_skill_links(&target);
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn configured_partial_assistant_with_invalid_git_is_not_scaffolded() {
        let parent = temp_dir("assistant-configured-partial-invalid-git");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::create_dir_all(target.join(".git")).unwrap();
        fs::write(target.join("SOUL.md"), "Existing identity\n").unwrap();
        fs::write(
            &config,
            format!(
                "assistant_root = {}\n",
                toml::Value::String(target.to_string_lossy().to_string())
            ),
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("validate existing Git metadata"));
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "Existing identity\n"
        );
        assert!(!target.join("AGENTS.md").exists());
        assert!(!target.join("README.md").exists());
        assert!(!target.join("context").exists());
        assert!(!target.join("jobs").exists());
        assert!(!target.join("skills").exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn persists_root_at_top_level_when_config_ends_with_a_table() {
        let parent = temp_dir("assistant-table-config");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::write(
            &config,
            "channel = 'imessage'\nself_handles = ['me@example.com']\n\n[telegram]\nbot_token = 'secret'\n",
        )
        .unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&config).unwrap();
        let value: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(
            value.get("assistant_root").and_then(toml::Value::as_str),
            Some(result.root.to_str().unwrap())
        );
        assert!(value["telegram"].get("assistant_root").is_none());
        assert_eq!(
            crate::config::Config::load(config.to_str().unwrap())
                .unwrap()
                .assistant_root,
            result.root.to_string_lossy()
        );
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_unrelated_non_empty_target_without_touching_it() {
        let parent = temp_dir("assistant-unrelated");
        let target = parent.join("project");
        fs::create_dir_all(target.join("context")).unwrap();
        fs::write(target.join("notes.txt"), "mine").unwrap();
        let config = parent.join("push.toml");

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("non-empty"));
        assert_eq!(
            fs::read_to_string(target.join("notes.txt")).unwrap(),
            "mine"
        );
        assert!(!target.join("SOUL.md").exists());
        assert!(!config.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_invalid_git_metadata_before_scaffolding() {
        let parent = temp_dir("assistant-invalid-git");
        let target = parent.join("assistant");
        fs::create_dir_all(target.join(".git")).unwrap();
        let config = parent.join("push.toml");

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("validate existing Git metadata"));
        assert!(!target.join("SOUL.md").exists());
        assert!(!config.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_to_replace_a_different_configured_assistant() {
        let parent = temp_dir("assistant-single");
        let first = parent.join("first");
        let second = parent.join("second");
        let config = parent.join("push.toml");
        init(first.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        let error = init(second.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("supports one assistant"));
        assert!(!second.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_legacy_independent_paths_with_migration_help() {
        let parent = temp_dir("assistant-legacy-init");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::write(
            &config,
            "assistant_dir = '/old/identity'\njobs_dir = '/old/jobs'\n",
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("legacy"));
        assert!(error.to_string().contains("assistant_root"));
        assert!(!target.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn refuses_job_runtime_state_inside_new_assistant_repository() {
        let parent = temp_dir("assistant-runtime-boundary");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::write(
            &config,
            format!("jobs_run_dir = {:?}\n", target.join("run")),
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("jobs_run_dir must stay outside"));
        assert!(!target.exists());
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn config_inside_root_uses_portable_relative_value() {
        let target = temp_path("assistant-dot");
        fs::create_dir_all(&target).unwrap();
        let config = target.join("config.toml");

        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert!(fs::read_to_string(&config)
            .unwrap()
            .contains("assistant_root = \".\""));
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn init_dot_accepts_an_existing_selected_config_in_the_target() {
        let target = temp_path("assistant-dot-config");
        fs::create_dir_all(&target).unwrap();
        let config = target.join("config.toml");
        fs::write(&config, "channel = 'telegram'\n").unwrap();

        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&config).unwrap();
        assert!(raw.starts_with("channel = 'telegram'\n"));
        assert!(raw.contains("assistant_root = \".\""));
        assert!(!raw.contains("agent = \"codex\""));
        assert!(!raw.contains("allow_user_ids"));
        assert!(target.join("SOUL.md").is_file());
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn refuses_an_inline_secret_in_a_config_inside_the_assistant() {
        let target = temp_path("assistant-secret-config");
        fs::create_dir_all(&target).unwrap();
        let config = target.join("config.toml");
        fs::write(
            &config,
            "channel = 'telegram'\n[telegram]\nbot_token = 'secret'\n",
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("inline Telegram token"));
        assert!(!target.join("SOUL.md").exists());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "channel = 'telegram'\n[telegram]\nbot_token = 'secret'\n"
        );
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn refuses_an_inline_voice_key_in_a_config_inside_the_assistant() {
        let target = temp_path("assistant-voice-secret-config");
        fs::create_dir_all(&target).unwrap();
        let config = target.join("config.toml");
        fs::write(&config, "[voice]\nopenai_api_key = 'secret'\n").unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("inline OpenAI API key"));
        assert!(!target.join("SOUL.md").exists());
        assert_eq!(
            fs::read_to_string(&config).unwrap(),
            "[voice]\nopenai_api_key = 'secret'\n"
        );
        let _ = fs::remove_dir_all(target);
    }

    #[test]
    fn refuses_inline_slack_tokens_in_a_config_inside_the_assistant() {
        let target = temp_path("assistant-slack-secret-config");
        fs::create_dir_all(&target).unwrap();
        let config = target.join("config.toml");
        fs::write(
            &config,
            "channel = 'slack'\n[slack]\napp_token = 'xapp-secret'\nbot_token = 'xoxb-secret'\n",
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("inline Slack tokens"));
        assert!(!target.join("SOUL.md").exists());
        let _ = fs::remove_dir_all(target);
    }

    fn assert_push_skill_links(root: &Path) {
        let canonical = fs::canonicalize(root.join("skills/push")).unwrap();
        for provider in PUSH_SKILL_PROVIDERS {
            let link = root.join(provider).join("skills/push");
            assert!(fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::canonicalize(link).unwrap(), canonical);
        }
    }
}
