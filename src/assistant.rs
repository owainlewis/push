//! Assistant repository scaffolding behind `push init`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::validate_inline_token_location;
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
- Treat `jobs/` as installed runbooks. Propose job changes through Push's approval workflow.
- Keep secrets, sessions, databases, drafts, logs, and other runtime state outside this repository.
"#;

const README: &str = r#"# Assistant

This Git repository contains the durable, user-owned parts of one Push assistant.

- `SOUL.md` defines the assistant's identity and working style.
- `context/` contains durable context the assistant may read and update.
- `jobs/` contains installed Push job runbooks.

Push owns channels, scheduling, history, security, approvals, and delivery outside this repository. The configured agent runtime owns reasoning, tools, skills, MCP servers, and authentication.
"#;

const CONTEXT_README: &str = r#"# Context

Store durable facts and working context here when they should be available across conversations.

Good examples include preferences, active projects, people, recurring processes, and reference notes. Keep secrets out of this repository. Start with small, focused Markdown files and update or remove stale information.
"#;

#[derive(Debug)]
pub struct InitResult {
    pub root: PathBuf,
    pub config_path: PathBuf,
    pub git_initialized: bool,
}

pub fn init(requested_path: &str, config_path: &str) -> Result<InitResult> {
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
    let existing_config = inspect_config(&config_path, &target)?;

    prepare_target(&target, &config_path)?;
    let root = fs::canonicalize(&target)
        .with_context(|| format!("resolve assistant root {}", target.display()))?;
    scaffold(&root)?;
    let git_initialized = initialize_git(&root)?;
    persist_root(&config_path, &root, existing_config)?;
    if inspect_config(&config_path, &root)? != ConfigState::MatchingRoot {
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
    MissingRoot,
    MatchingRoot,
}

fn inspect_config(config_path: &Path, target: &Path) -> Result<ConfigState> {
    let raw = match fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            validate_runtime_boundary(None, target)?;
            return Ok(ConfigState::MissingRoot);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("read config {}", config_path.display()))
        }
    };
    let value: toml::Value =
        toml::from_str(&raw).with_context(|| format!("parse config {}", config_path.display()))?;
    let table = value.as_table().context("config must be a TOML table")?;
    validate_config_secrets(config_path, target, table)?;
    validate_runtime_boundary(Some(table), target)?;
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
    Ok(())
}

fn validate_runtime_boundary(config: Option<&toml::Table>, target: &Path) -> Result<()> {
    let assistant = resolve_existing_or_lexical(target)?;
    for (key, default) in [
        ("sessions_dir", "~/.push/sessions"),
        ("drafts_dir", "~/.push/drafts"),
        ("jobs_run_dir", "~/.push/run"),
    ] {
        let runtime = configured_runtime_path(config, key, default)?;
        if assistant.starts_with(&runtime) || runtime.starts_with(&assistant) {
            bail!("{key} must stay outside assistant_root; choose a separate assistant path or update {key}");
        }
    }
    for (key, default) in [
        ("state_path", "~/.push/state.json"),
        ("database_path", "~/.push/push.db"),
        ("audit_log_path", "~/.push/audit.jsonl"),
    ] {
        let runtime = configured_runtime_path(config, key, default)?;
        if runtime.starts_with(&assistant) {
            bail!("{key} must stay outside assistant_root; choose a separate assistant path or update {key}");
        }
    }
    Ok(())
}

fn configured_runtime_path(
    config: Option<&toml::Table>,
    key: &str,
    default: &str,
) -> Result<PathBuf> {
    let value = match config.and_then(|table| table.get(key)) {
        Some(value) => value
            .as_str()
            .with_context(|| format!("{key} must be a string"))?,
        None => default,
    };
    let expanded = expand_home(value);
    if expanded.starts_with('~') {
        bail!("cannot expand configured {key} {value:?}");
    }
    let path = Path::new(&expanded);
    if !path.is_absolute() {
        bail!("{key} must be an absolute path or start with ~");
    }
    resolve_existing_or_lexical(path)
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

fn prepare_target(target: &Path, config_path: &Path) -> Result<()> {
    if target.exists() {
        if !target.is_dir() {
            bail!("assistant target {} is not a directory", target.display());
        }
        let entries = fs::read_dir(target)
            .with_context(|| format!("inspect assistant target {}", target.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        let resolved_config = resolve_existing_or_lexical(config_path)?;
        if entries.is_empty()
            || entries.iter().all(|entry| {
                entry.file_name() == ".git"
                    || resolve_existing_or_lexical(&entry.path())
                        .is_ok_and(|path| path == resolved_config)
            })
        {
            if target.join(".git").exists() {
                verify_git_root(target).context("validate existing Git metadata before init")?;
            }
            return Ok(());
        }
        if !valid_assistant_structure(target) {
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
    create_directory(&root.join("jobs"))?;
    create_file(&root.join("SOUL.md"), SOUL)?;
    create_file(&root.join("AGENTS.md"), AGENTS)?;
    create_file(&root.join("README.md"), README)?;
    create_file(&root.join("context/README.md"), CONTEXT_README)?;
    Ok(())
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
            if fs::symlink_metadata(path)?.file_type().is_file() {
                Ok(())
            } else {
                bail!(
                    "cannot create file {} because it is not a regular file",
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
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
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
        assert!(target.join("README.md").is_file());
        assert!(target.join("context/README.md").is_file());
        assert!(target.join("jobs").is_dir());
        assert_eq!(fs::read_dir(target.join("jobs")).unwrap().count(), 0);
        assert!(target.join(".git").exists());
        let raw = fs::read_to_string(config).unwrap();
        assert!(raw.contains(&format!(
            "assistant_root = {}",
            toml::Value::String(result.root.to_string_lossy().to_string())
        )));
        let _ = fs::remove_dir_all(parent);
    }

    #[test]
    fn repeat_initialization_preserves_user_files_and_configuration() {
        let parent = temp_dir("assistant-reinit");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();
        fs::write(target.join("SOUL.md"), "My identity\n").unwrap();
        fs::write(target.join("context/private.md"), "Keep me\n").unwrap();
        let config_before = fs::read_to_string(&config).unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        assert!(!result.git_initialized);
        assert_eq!(
            fs::read_to_string(target.join("SOUL.md")).unwrap(),
            "My identity\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("context/private.md")).unwrap(),
            "Keep me\n"
        );
        assert_eq!(fs::read_to_string(config).unwrap(), config_before);
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
    fn persists_root_at_top_level_when_config_ends_with_a_table() {
        let parent = temp_dir("assistant-table-config");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::write(
            &config,
            "channel = 'imessage'\nself_handles = ['me@example.com']\npermission_profile = 'custom'\n\n[permission_profiles.custom]\ncapability = 'read-only'\n",
        )
        .unwrap();

        let result = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap();

        let raw = fs::read_to_string(&config).unwrap();
        let value: toml::Value = toml::from_str(&raw).unwrap();
        assert_eq!(
            value.get("assistant_root").and_then(toml::Value::as_str),
            Some(result.root.to_str().unwrap())
        );
        assert!(value["permission_profiles"]["custom"]
            .get("assistant_root")
            .is_none());
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
    fn refuses_runtime_state_inside_new_assistant_repository() {
        let parent = temp_dir("assistant-runtime-boundary");
        let target = parent.join("assistant");
        let config = parent.join("push.toml");
        fs::write(
            &config,
            format!("sessions_dir = {:?}\n", target.join("sessions")),
        )
        .unwrap();

        let error = init(target.to_str().unwrap(), config.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("sessions_dir must stay outside"));
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
}
