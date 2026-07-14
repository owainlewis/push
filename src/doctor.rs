//! Environment checks behind `push doctor` and the gateway's startup preflight.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use crate::{config, drafts, history, jobs};

/// Fails fast with actionable messages when the environment is not ready.
pub fn preflight(cfg: &config::Config) -> Result<()> {
    let report = run_checks(cfg);
    if report.is_ok() {
        return Ok(());
    }
    let failed = report
        .checks
        .into_iter()
        .find(|check| matches!(check.status, CheckStatus::Fail))
        .expect("failed report has at least one failure");
    bail!("{}: {}", failed.name, failed.message);
}

pub fn doctor(config_path: &str) -> Result<()> {
    let cfg = match config::Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            let report = CheckReport {
                checks: vec![Check::fail(
                    "config",
                    format!(
                        "cannot load {config_path}: {e}. Create the file from config.toml.example or fix the invalid value."
                    ),
                )],
            };
            print!("{report}");
            bail!("doctor found 1 failed check");
        }
    };
    let report = run_checks(&cfg);
    print!("{report}");
    if report.is_ok() {
        Ok(())
    } else {
        bail!("doctor found {} failed check(s)", report.failed_count())
    }
}

fn run_checks(cfg: &config::Config) -> CheckReport {
    let mut checks = Vec::new();
    check_config(cfg, &mut checks);
    check_parent_dir(
        "state directory",
        "state_path",
        &cfg.state_path,
        &mut checks,
    );
    check_writable_dir(
        "sessions directory",
        "sessions_dir",
        Path::new(&cfg.sessions_dir),
        &mut checks,
    );
    check_drafts_dir(cfg, &mut checks);
    check_parent_dir(
        "audit log directory",
        "audit_log_path",
        &cfg.audit_log_path,
        &mut checks,
    );
    check_history_database(cfg, &mut checks);
    match cfg.enabled_channel_kinds() {
        Ok(channels) => {
            for channel in channels {
                match channel {
                    config::ChannelKind::IMessage => check_imessage_db(cfg, &mut checks),
                    config::ChannelKind::Telegram => check_telegram_config(cfg, &mut checks),
                }
            }
        }
        Err(e) => checks.push(Check::fail("channels", e.to_string())),
    }
    check_bins(cfg, &mut checks);
    CheckReport { checks }
}

fn check_config(cfg: &config::Config, checks: &mut Vec<Check>) {
    checks.push(Check::pass(
        "config",
        format!(
            "channels={}, agent={}, permission_profile={}, assistant_root={}, imessage.self_handles={}, imessage.allow_from={}, telegram.allow_user_ids={}, telegram.allow_chat_ids={}",
            cfg.enabled_channel_kinds()
                .map(|channels| channels.into_iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(","))
                .unwrap_or_else(|_| cfg.channel.clone()),
            cfg.agent,
            cfg.permission_profile,
            cfg.assistant_root,
            cfg.self_handles.len(),
            cfg.allow_from.len(),
            cfg.telegram_allow_user_ids.len(),
            cfg.telegram_allow_chat_ids.len()
        ),
    ));
}

/// Checks that the parent directory of a configured file path is writable.
fn check_parent_dir(name: &str, field: &str, path: &str, checks: &mut Vec<Check>) {
    if let Some(parent) = Path::new(path).parent() {
        check_writable_dir(name, field, parent, checks);
    } else {
        checks.push(Check::pass(
            name.to_string(),
            format!("{field} has no parent directory"),
        ));
    }
}

fn check_writable_dir(name: &str, field: &str, dir: &Path, checks: &mut Vec<Check>) {
    match ensure_writable_dir(dir) {
        Ok(()) => checks.push(Check::pass(
            name.to_string(),
            format!("{} is writable", dir.display()),
        )),
        Err(e) => checks.push(Check::fail(
            name.to_string(),
            format!(
                "cannot create {}: {e}. Create it or choose a writable {field}.",
                dir.display()
            ),
        )),
    }
}

fn check_drafts_dir(cfg: &config::Config, checks: &mut Vec<Check>) {
    match drafts::prepare(cfg) {
        Ok(()) => checks.push(Check::pass(
            "drafts directory",
            format!("{} is writable and protected", cfg.drafts_dir),
        )),
        Err(error) => checks.push(Check::fail(
            "drafts directory",
            format!(
                "cannot prepare {}: {error}. Create it with owner-only permissions or choose a writable drafts_dir.",
                cfg.drafts_dir
            ),
        )),
    }
}

fn check_history_database(cfg: &config::Config, checks: &mut Vec<Check>) {
    match history::History::open(&cfg.database_path) {
        Ok(_) => checks.push(Check::pass(
            "conversation database",
            format!("{} is ready", cfg.database_path),
        )),
        Err(error) => checks.push(Check::fail(
            "conversation database",
            format!(
                "cannot open {}: {error}. Choose a writable database_path and repair or remove an invalid database.",
                cfg.database_path
            ),
        )),
    }
}

fn check_imessage_db(cfg: &config::Config, checks: &mut Vec<Check>) {
    match std::fs::File::open(&cfg.db_path) {
        Ok(_) => checks.push(Check::pass(
            "iMessage database",
            format!("{} is readable", cfg.db_path),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => checks.push(Check::fail(
            "iMessage database",
            format!(
                "cannot read {}. Grant Full Disk Access to your terminal in System Settings > Privacy & Security > Full Disk Access.",
                cfg.db_path
            ),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => checks.push(Check::fail(
            "iMessage database",
            format!(
                "Messages database not found at {}. Sign in to iMessage or set imessage.db_path.",
                cfg.db_path
            ),
        )),
        Err(e) => checks.push(Check::fail(
            "iMessage database",
            format!(
                "cannot open {}: {e}. Check imessage.db_path and Messages permissions, then rerun doctor.",
                cfg.db_path
            ),
        )),
    }
}

fn check_telegram_config(cfg: &config::Config, checks: &mut Vec<Check>) {
    if cfg.telegram_token().is_some() {
        checks.push(Check::pass(
            "Telegram bot token",
            format!("loaded from config or {}", cfg.telegram_bot_token_env),
        ));
    } else {
        checks.push(Check::fail(
            "Telegram bot token",
            format!(
                "not configured. Set {} or telegram.bot_token without printing the token.",
                cfg.telegram_bot_token_env
            ),
        ));
    }
}

fn ensure_writable_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(format!(".push-doctor-write-test-{}", std::process::id()));
    std::fs::write(&probe, b"ok")?;
    std::fs::remove_file(probe)?;
    Ok(())
}

fn check_bins(cfg: &config::Config, checks: &mut Vec<Check>) {
    check_bins_with(cfg, checks, which);
}

fn check_bins_with(
    cfg: &config::Config,
    checks: &mut Vec<Check>,
    finder: impl Fn(&str) -> Option<PathBuf>,
) {
    let mut bins = match cfg.required_agent_bins() {
        Ok(bins) => bins,
        Err(e) => {
            checks.push(Check::fail("agent binaries", format!("{e}")));
            return;
        }
    };
    if let Ok(catalog) = jobs::Catalog::load(cfg) {
        bins.extend(catalog.jobs.values().map(|job| cfg.agent_bin(job.backend)));
    }
    if cfg
        .enabled_channel_kinds()
        .is_ok_and(|channels| channels.contains(&config::ChannelKind::IMessage))
    {
        bins.push("osascript");
    }
    bins.sort_unstable();
    bins.dedup();
    for bin in bins {
        match finder(bin) {
            Some(path) => checks.push(Check::pass(
                format!("binary {bin}"),
                format!("found at {}", path.display()),
            )),
            None => checks.push(Check::fail(
                format!("binary {bin}"),
                format!(
                    "{bin:?} not found on PATH. Install it or update the matching config field."
                ),
            )),
        }
    }
}

#[derive(Debug)]
struct CheckReport {
    checks: Vec<Check>,
}

impl CheckReport {
    fn is_ok(&self) -> bool {
        self.checks
            .iter()
            .all(|check| matches!(check.status, CheckStatus::Pass))
    }

    fn failed_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| matches!(check.status, CheckStatus::Fail))
            .count()
    }
}

impl fmt::Display for CheckReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "push doctor")?;
        for check in &self.checks {
            let marker = match check.status {
                CheckStatus::Pass => "PASS",
                CheckStatus::Fail => "FAIL",
            };
            writeln!(f, "[{marker}] {}: {}", check.name, check.message)?;
        }
        if self.is_ok() {
            writeln!(f, "All checks passed.")
        } else {
            writeln!(f, "{} check(s) failed.", self.failed_count())
        }
    }
}

#[derive(Debug)]
struct Check {
    name: String,
    status: CheckStatus,
    message: String,
}

impl Check {
    fn pass(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Pass,
            message: message.into(),
        }
    }

    fn fail(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            status: CheckStatus::Fail,
            message: message.into(),
        }
    }
}

#[derive(Debug)]
enum CheckStatus {
    Pass,
    Fail,
}

fn which(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return p.exists().then_some(p);
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(bin))
        .find(|cand| cand.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{temp_dir, temp_path, test_config};

    #[test]
    fn doctor_reports_config_load_failure() {
        let path = temp_path("missing-config");

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
    }

    #[test]
    fn doctor_reports_invalid_toml_config() {
        let path = temp_path("invalid-toml-config");
        std::fs::write(&path, "{").unwrap();

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_reports_invalid_config_value() {
        let path = temp_path("invalid-value-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
agent = "bogus"
"#,
        )
        .unwrap();

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_rejects_an_inline_token_inside_the_assistant_repository() {
        let root = temp_dir("doctor-inline-token");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            "channel = 'telegram'\nassistant_root = '.'\n[telegram]\nbot_token = 'committed-secret'\nallow_user_ids = [1]\n",
        )
        .unwrap();

        let error = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn doctor_reports_empty_claude_tool_filter() {
        let path = temp_path("empty-claude-tool-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
claude_allowed_tools = ["Read", " "]
"#,
        )
        .unwrap();

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_reports_empty_claude_tools_list() {
        let path = temp_path("empty-claude-tools-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
claude_tools = []
"#,
        )
        .unwrap();

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn binary_checks_report_present_and_missing_bins() {
        let cfg = test_config();
        let mut checks = Vec::new();

        check_bins_with(&cfg, &mut checks, |bin| {
            (bin == "/fake/codex").then(|| PathBuf::from(bin))
        });

        assert!(checks.iter().any(|check| {
            check.name == "binary /fake/codex" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(checks.iter().any(|check| {
            check.name == "binary osascript" && matches!(check.status, CheckStatus::Fail)
        }));
    }

    #[test]
    fn binary_checks_use_configured_pi_binary_when_pi_is_active() {
        let mut cfg = test_config();
        cfg.agent = "pi".to_string();
        cfg.pi_bin = "/custom/pi".to_string();
        let mut checks = Vec::new();

        check_bins_with(&cfg, &mut checks, |bin| {
            (bin == "/custom/pi" || bin == "osascript").then(|| PathBuf::from(bin))
        });

        assert!(checks.iter().any(|check| {
            check.name == "binary /custom/pi" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(!checks
            .iter()
            .any(|check| check.name == "binary /fake/codex"));
    }

    #[test]
    fn binary_checks_include_pi_selected_by_an_installed_job() {
        let mut cfg = test_config();
        let jobs_dir = temp_dir("doctor-pi-job");
        let workdir = temp_dir("doctor-pi-job-work");
        cfg.jobs_dir = jobs_dir.to_string_lossy().to_string();
        cfg.pi_bin = "/custom/pi".to_string();
        std::fs::write(
            jobs_dir.join("pi-job.md"),
            format!(
                "+++\nversion = 1\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"pi\"\n+++\nRun Pi.\n",
                workdir.to_string_lossy()
            ),
        )
        .unwrap();
        let mut checks = Vec::new();

        check_bins_with(&cfg, &mut checks, |bin| {
            (bin == "/fake/codex" || bin == "osascript").then(|| PathBuf::from(bin))
        });

        assert!(checks.iter().any(|check| {
            check.name == "binary /custom/pi" && matches!(check.status, CheckStatus::Fail)
        }));
    }

    #[test]
    fn telegram_binary_checks_do_not_require_osascript() {
        let mut cfg = test_config();
        cfg.channel = "telegram".to_string();
        cfg.self_handles.clear();
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];
        cfg.routes = vec![config::RouteRule {
            thread: None,
            channel: Some("imessage".to_string()),
            agent: "claude".to_string(),
            permission_profile: None,
        }];
        let mut checks = Vec::new();

        check_bins_with(&cfg, &mut checks, |bin| {
            (bin == "/fake/codex").then(|| PathBuf::from(bin))
        });

        assert!(checks.iter().any(|check| {
            check.name == "binary /fake/codex" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(!checks
            .iter()
            .any(|check| check.name == "binary /fake/claude"));
        assert!(!checks.iter().any(|check| check.name.contains("osascript")));
    }

    #[test]
    fn telegram_preflight_checks_token_without_imessage_database() {
        let mut cfg = test_config();
        cfg.channel = "telegram".to_string();
        cfg.self_handles.clear();
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];
        let mut checks = Vec::new();

        check_telegram_config(&cfg, &mut checks);

        assert!(checks.iter().any(|check| {
            check.name == "Telegram bot token" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(!checks.iter().any(|check| check.name == "iMessage database"));
        assert!(!format!("{:?}", checks[0].message).contains("secret"));
    }

    #[test]
    fn full_preflight_checks_only_enabled_reply_channels() {
        let mut cfg = test_config();
        cfg.channels = vec!["telegram".to_string()];
        cfg.self_handles.clear();
        cfg.db_path = "/definitely/missing/chat.db".to_string();
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];

        let report = run_checks(&cfg);

        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "Telegram bot token"));
        assert!(!report
            .checks
            .iter()
            .any(|check| check.name == "iMessage database"));
    }

    #[test]
    fn run_checks_reports_config_and_writable_paths() {
        let db_path = temp_path("chat-db");
        std::fs::write(&db_path, "").unwrap();
        let state_path = temp_path("state-dir").join("state.json");
        let sessions_dir = temp_dir("sessions-dir");
        let mut cfg = test_config();
        cfg.db_path = db_path.to_string_lossy().to_string();
        cfg.state_path = state_path.to_string_lossy().to_string();
        cfg.audit_log_path = state_path
            .with_extension("audit.jsonl")
            .to_string_lossy()
            .to_string();
        cfg.database_path = state_path
            .with_extension("push.db")
            .to_string_lossy()
            .to_string();
        cfg.sessions_dir = sessions_dir.to_string_lossy().to_string();

        let report = run_checks(&cfg);

        assert!(report
            .checks
            .iter()
            .any(|check| check.name == "config" && matches!(check.status, CheckStatus::Pass)));
        assert!(report.checks.iter().any(|check| {
            check.name == "state directory" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(report.checks.iter().any(|check| {
            check.name == "sessions directory" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(report.checks.iter().any(|check| {
            check.name == "audit log directory" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(report.checks.iter().any(|check| {
            check.name == "conversation database" && matches!(check.status, CheckStatus::Pass)
        }));
        assert!(report.checks.iter().any(|check| {
            check.name == "iMessage database" && matches!(check.status, CheckStatus::Pass)
        }));

        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(cfg.audit_log_path);
        let _ = std::fs::remove_file(cfg.database_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
    }

    #[test]
    fn writable_dir_check_writes_probe_file() {
        let dir = temp_dir("doctor-writable");

        ensure_writable_dir(&dir).unwrap();

        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
