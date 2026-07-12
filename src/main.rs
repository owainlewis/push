//! push is a tiny iMessage gateway for personal assistant agents. It polls the
//! macOS Messages database for new messages, sends each through a configured
//! coding-agent backend, and texts the reply back.

mod agent;
mod approval;
mod audit;
mod channel;
mod claude;
mod codex;
mod config;
mod gateway;
mod history;
mod imessage;
mod jobs;
mod rehydration;
mod soul;
mod store;
mod telegram;
#[cfg(test)]
mod test_support;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let args = Args::parse(std::env::args().skip(1).collect())?;
    match args.command {
        Command::Doctor => doctor(&args.config_path),
        Command::Job(command) => run_job_command(&args.config_path, command).await,
        Command::Run => {
            let cfg = config::Config::load(&args.config_path).context("config")?;
            preflight(&cfg).context("preflight")?;
            report_invalid_jobs(&cfg)?;
            gateway::GatewayGroup::new(cfg).context("init")?.run().await
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    command: Command,
    config_path: String,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Run,
    Doctor,
    Job(JobCommand),
}

#[derive(Debug, PartialEq, Eq)]
enum JobCommand {
    Validate,
    List,
    Show(String),
    Run(String),
    Runs(Option<String>),
}

impl Args {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut config_path = "config.toml".to_string();
        let mut positional = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--config" => {
                    let Some(path) = args.get(i + 1) else {
                        bail!("--config requires a path");
                    };
                    config_path = path.clone();
                    i += 2;
                }
                value => {
                    positional.push(value.to_string());
                    i += 1;
                }
            }
        }
        let command = match positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] => Command::Run,
            ["doctor"] => Command::Doctor,
            ["job", "validate"] => Command::Job(JobCommand::Validate),
            ["job", "list"] => Command::Job(JobCommand::List),
            ["job", "show", name] => Command::Job(JobCommand::Show((*name).to_string())),
            ["job", "run", name] => Command::Job(JobCommand::Run((*name).to_string())),
            ["job", "runs"] => Command::Job(JobCommand::Runs(None)),
            ["job", "runs", name] => Command::Job(JobCommand::Runs(Some((*name).to_string()))),
            _ => bail!(
                "unknown command; expected doctor, job validate, job list, job show <name>, job run <name>, job runs [<name>], or --config <path>"
            ),
        };
        Ok(Self {
            command,
            config_path,
        })
    }
}

async fn run_job_command(config_path: &str, command: JobCommand) -> Result<()> {
    let cfg = config::Config::load(config_path).context("config")?;
    match command {
        JobCommand::Validate => {
            let catalog = jobs::Catalog::load(&cfg)?;
            for job in catalog.jobs.values() {
                println!("VALID\t{}", job.name);
            }
            for error in &catalog.errors {
                println!(
                    "INVALID\t{}\t{}\t{}",
                    error.name,
                    error.path.display(),
                    error.message
                );
            }
            if catalog.errors.is_empty() {
                Ok(())
            } else {
                bail!("{} invalid job(s)", catalog.errors.len())
            }
        }
        JobCommand::List => {
            let catalog = jobs::Catalog::load(&cfg)?;
            for job in catalog.jobs.values() {
                println!(
                    "{}\tvalid\t{}\t{}",
                    job.name,
                    job.backend.as_str(),
                    job.permission.name
                );
            }
            for error in catalog.errors {
                println!("{}\tinvalid\t{}", error.name, error.message);
            }
            Ok(())
        }
        JobCommand::Show(name) => {
            let job = jobs::Catalog::load_named(&cfg, &name)?;
            print!("{}", jobs::format_job(&job));
            Ok(())
        }
        JobCommand::Run(name) => {
            let job = jobs::Catalog::load_named(&cfg, &name)?;
            let (run_id, output) = jobs::run_manual(&cfg, job).await?;
            println!("run_id: {run_id}");
            println!("{output}");
            Ok(())
        }
        JobCommand::Runs(name) => {
            if let Some(name) = name.as_deref() {
                jobs::validate_job_name(name)?;
            }
            let ledger = jobs::Ledger::open(&cfg.database_path)?;
            for run in ledger.runs(name.as_deref())? {
                let trigger = run
                    .trigger_id
                    .as_deref()
                    .map(|id| format!("{}:{id}", run.trigger_kind))
                    .unwrap_or(run.trigger_kind);
                let scheduled = run
                    .scheduled_at_ms
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string());
                let delivery = format!("{}({})", run.delivery_state, run.delivery_attempts);
                let destination = run
                    .delivery_channel
                    .zip(run.delivery_target)
                    .map(|(channel, target)| format!("{channel}:{target}"))
                    .unwrap_or_else(|| "-".to_string());
                let execution_detail = run
                    .result
                    .or(run.error)
                    .unwrap_or_default()
                    .replace('\n', " ");
                let delivery_error = run.delivery_error.unwrap_or_default().replace('\n', " ");
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    run.id,
                    run.job_name,
                    run.state,
                    run.backend,
                    trigger,
                    scheduled,
                    run.queued_at_ms,
                    delivery,
                    destination,
                    execution_detail,
                    delivery_error,
                );
            }
            Ok(())
        }
    }
}

fn report_invalid_jobs(cfg: &config::Config) -> Result<()> {
    let catalog = jobs::Catalog::load(cfg)?;
    for error in catalog.errors {
        tracing::warn!(
            "job {:?} disabled ({}): {}",
            error.name,
            error.path.display(),
            error.message
        );
    }
    Ok(())
}

/// Fails fast with actionable messages when the environment is not ready.
fn preflight(cfg: &config::Config) -> Result<()> {
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

fn doctor(config_path: &str) -> Result<()> {
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
    check_state_dir(cfg, &mut checks);
    check_sessions_dir(cfg, &mut checks);
    check_audit_log_dir(cfg, &mut checks);
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
            "channels={}, agent={}, permission_profile={}, imessage.self_handles={}, imessage.allow_from={}, telegram.allow_user_ids={}, telegram.allow_chat_ids={}",
            cfg.enabled_channel_kinds()
                .map(|channels| channels.into_iter().map(|kind| kind.as_str()).collect::<Vec<_>>().join(","))
                .unwrap_or_else(|_| cfg.channel.clone()),
            cfg.agent,
            cfg.permission_profile,
            cfg.self_handles.len(),
            cfg.allow_from.len(),
            cfg.telegram_allow_user_ids.len(),
            cfg.telegram_allow_chat_ids.len()
        ),
    ));
}

fn check_state_dir(cfg: &config::Config, checks: &mut Vec<Check>) {
    if let Some(parent) = Path::new(&cfg.state_path).parent() {
        match ensure_writable_dir(parent) {
            Ok(()) => checks.push(Check::pass(
                "state directory",
                format!("{} is writable", parent.display()),
            )),
            Err(e) => checks.push(Check::fail(
                "state directory",
                format!(
                    "cannot create {}: {e}. Create it or choose a writable state_path.",
                    parent.display()
                ),
            )),
        }
    } else {
        checks.push(Check::pass(
            "state directory",
            "state_path has no parent directory",
        ));
    }
}

fn check_sessions_dir(cfg: &config::Config, checks: &mut Vec<Check>) {
    match ensure_writable_dir(Path::new(&cfg.sessions_dir)) {
        Ok(()) => checks.push(Check::pass(
            "sessions directory",
            format!("{} is writable", cfg.sessions_dir),
        )),
        Err(e) => checks.push(Check::fail(
            "sessions directory",
            format!(
                "cannot create {}: {e}. Create it or choose a writable sessions_dir.",
                cfg.sessions_dir
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

fn check_audit_log_dir(cfg: &config::Config, checks: &mut Vec<Check>) {
    if let Some(parent) = Path::new(&cfg.audit_log_path).parent() {
        match ensure_writable_dir(parent) {
            Ok(()) => checks.push(Check::pass(
                "audit log directory",
                format!("{} is writable", parent.display()),
            )),
            Err(e) => checks.push(Check::fail(
                "audit log directory",
                format!(
                    "cannot create {}: {e}. Create it or choose a writable audit_log_path.",
                    parent.display()
                ),
            )),
        }
    } else {
        checks.push(Check::pass(
            "audit log directory",
            "audit_log_path has no parent directory",
        ));
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
    use crate::config::Config;
    use crate::test_support::{temp_dir, temp_path};

    #[test]
    fn parses_doctor_with_config_path() {
        let args = Args::parse(vec![
            "doctor".to_string(),
            "--config".to_string(),
            "custom.toml".to_string(),
        ])
        .unwrap();

        assert_eq!(
            args,
            Args {
                command: Command::Doctor,
                config_path: "custom.toml".to_string(),
            }
        );
    }

    #[test]
    fn parses_all_job_commands_with_config_anywhere() {
        assert_eq!(
            Args::parse(vec!["job".into(), "validate".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Validate)
        );
        assert_eq!(
            Args::parse(vec![
                "--config".into(),
                "x.toml".into(),
                "job".into(),
                "list".into()
            ])
            .unwrap(),
            Args {
                command: Command::Job(JobCommand::List),
                config_path: "x.toml".to_string(),
            }
        );
        assert_eq!(
            Args::parse(vec!["job".into(), "show".into(), "daily".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Show("daily".to_string()))
        );
        assert_eq!(
            Args::parse(vec!["job".into(), "run".into(), "daily".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Run("daily".to_string()))
        );
        assert_eq!(
            Args::parse(vec!["job".into(), "runs".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Runs(None))
        );
        assert_eq!(
            Args::parse(vec!["job".into(), "runs".into(), "daily".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Runs(Some("daily".to_string())))
        );
    }

    #[test]
    fn invalid_jobs_are_non_fatal_during_gateway_startup() {
        let jobs_dir = temp_dir("invalid-startup-jobs");
        std::fs::write(jobs_dir.join("invalid.md"), "not a runbook").unwrap();
        let state_path = temp_path("invalid-startup-state");
        let sessions_dir = temp_dir("invalid-startup-sessions");
        let assistant_dir = temp_dir("invalid-startup-assistant");
        let mut cfg = crate::gateway::tests::test_config_for_jobs(
            state_path.to_str().unwrap(),
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.jobs_dir = jobs_dir.to_string_lossy().to_string();

        assert!(report_invalid_jobs(&cfg).is_ok());
        assert!(gateway::Gateway::new(cfg).is_ok());
    }

    #[test]
    fn defaults_to_toml_config_path() {
        assert_eq!(
            Args::parse(Vec::new()).unwrap(),
            Args {
                command: Command::Run,
                config_path: "config.toml".to_string(),
            }
        );
    }

    #[test]
    fn example_toml_is_a_minimal_telegram_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.toml.example");

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.channel, "telegram");
        assert!(cfg.channels.is_empty());
        assert!(cfg.primary_delivery.is_none());
        assert_eq!(cfg.agent, "codex");
        assert_eq!(cfg.telegram_bot_token_env, "TELEGRAM_BOT_TOKEN");
        assert_eq!(cfg.telegram_allow_user_ids, [123456789]);
        assert!(cfg.telegram_allow_chat_ids.is_empty());
        assert_eq!(cfg.permission_profile, "restricted");
        assert!(cfg.permission_profiles.is_empty());
        assert_eq!(cfg.jobs_agent, None);
        assert_eq!(cfg.jobs_max_timeout, "30m");
        assert_eq!(cfg.jobs_max_workers, 2);
        assert_eq!(
            cfg.jobs_dir,
            Path::new(&std::env::var("HOME").unwrap())
                .join(".push/jobs")
                .to_string_lossy()
        );
        assert_eq!(
            cfg.database_path,
            Path::new(&std::env::var("HOME").unwrap())
                .join(".push/push.db")
                .to_string_lossy()
        );
        assert_eq!(
            cfg.assistant_dir,
            Path::new(&std::env::var("HOME").unwrap())
                .join(".push")
                .to_string_lossy()
        );
    }

    #[test]
    fn explicit_assistant_dir_override_is_preserved() {
        let path = temp_path("assistant-dir-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
assistant_dir = "/srv/push-assistant"
"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.assistant_dir, "/srv/push-assistant");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn structured_assistant_profile_reports_migration() {
        let path = temp_path("structured-assistant-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]

[assistant]
name = "push"
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("assistant_dir/SOUL.md"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_sections_load_channel_settings() {
        let path = temp_path("provider-section-config");
        std::fs::write(
            &path,
            r#"channel = "telegram"
agent = "codex"

[imessage]
db_path = "/tmp/messages.db"
self_handles = ["me@example.com"]

[telegram]
bot_token_env = "PUSH_TEST_TOKEN"
allow_user_ids = [7]
allow_chat_ids = [9]
"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.db_path, "/tmp/messages.db");
        assert_eq!(cfg.self_handles, ["me@example.com"]);
        assert_eq!(cfg.telegram_bot_token_env, "PUSH_TEST_TOKEN");
        assert_eq!(cfg.telegram_allow_user_ids, [7]);
        assert_eq!(cfg.telegram_allow_chat_ids, [9]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_sections_reject_duplicate_flat_settings() {
        let path = temp_path("duplicate-provider-config");
        std::fs::write(
            &path,
            r#"channel = "telegram"
agent = "codex"
telegram_allow_user_ids = [7]

[telegram]
allow_user_ids = [9]
"#,
        )
        .unwrap();

        let err = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("not both"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn legacy_flat_telegram_settings_remain_supported() {
        let path = temp_path("legacy-flat-telegram-config");
        std::fs::write(
            &path,
            r#"channel = "telegram"
agent = "codex"
telegram_bot_token_env = "LEGACY_TOKEN"
telegram_allow_user_ids = [7]
telegram_allow_chat_ids = [9]
"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.telegram_bot_token_env, "LEGACY_TOKEN");
        assert_eq!(cfg.telegram_allow_user_ids, [7]);
        assert_eq!(cfg.telegram_allow_chat_ids, [9]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_missing_config_path_arg() {
        let err = Args::parse(vec!["--config".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--config requires a path"));
    }

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
    fn config_rejects_legacy_claude_tool_filter_aliases() {
        let path = temp_path("claude-tool-alias-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
allowed_tools = ["Read"]
disallowed_tools = ["Edit"]
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("named permission_profile"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn config_rejects_legacy_claude_tools_alias() {
        let path = temp_path("claude-tools-alias-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
tools = ["Read", "Grep"]
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("named permission_profile"));
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
    fn multi_channel_config_is_opt_in_and_defers_primary_resolution() {
        let path = temp_path("multi-channel-config");
        std::fs::write(
            &path,
            r#"channels = ["imessage", "telegram"]
agent = "codex"

[imessage]
self_handles = ["me@icloud.com"]

[telegram]
bot_token = "secret"
allow_user_ids = [7]

[primary_delivery]
channel = "telegram"
target = "not-an-allowed-target"
"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(
            cfg.enabled_channel_kinds().unwrap(),
            vec![config::ChannelKind::IMessage, config::ChannelKind::Telegram]
        );
        assert_eq!(
            cfg.primary_delivery,
            Some(config::PrimaryDeliveryConfig {
                channel: "telegram".to_string(),
                target: "not-an-allowed-target".to_string(),
            })
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn duplicate_enabled_channels_are_rejected() {
        let path = temp_path("duplicate-channel-config");
        std::fs::write(
            &path,
            r#"channels = ["telegram", "telegram"]
[telegram]
bot_token = "secret"
allow_user_ids = [7]
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("duplicate enabled channel"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn routes_support_channel_override_exact_thread_and_legacy_imessage_key() {
        let mut cfg = test_config();
        cfg.agent = "claude".to_string();
        cfg.routes = vec![
            config::RouteRule {
                thread: None,
                channel: Some("telegram".to_string()),
                agent: "codex".to_string(),
                permission_profile: Some("workspace".to_string()),
            },
            config::RouteRule {
                thread: Some("telegram:dm:7".to_string()),
                channel: None,
                agent: "claude".to_string(),
                permission_profile: None,
            },
            config::RouteRule {
                thread: Some("telegram:dm:7:topic:99".to_string()),
                channel: None,
                agent: "codex".to_string(),
                permission_profile: None,
            },
            config::RouteRule {
                thread: Some("self:me@icloud.com".to_string()),
                channel: None,
                agent: "codex".to_string(),
                permission_profile: None,
            },
        ];

        assert_eq!(
            cfg.route_for_message("telegram", "telegram:dm:7")
                .unwrap()
                .backend,
            config::AgentBackend::Claude
        );
        assert_eq!(
            cfg.route_for_message("telegram", "telegram:dm:8")
                .unwrap()
                .backend,
            config::AgentBackend::Codex
        );
        assert_eq!(
            cfg.route_for_message("telegram", "telegram:dm:8")
                .unwrap()
                .permission
                .capability,
            config::PermissionCapability::Workspace
        );
        assert_eq!(
            cfg.route_for_message("telegram", "telegram:dm:7:topic:99")
                .unwrap()
                .backend,
            config::AgentBackend::Codex
        );
        assert_eq!(
            cfg.route_for_message("telegram", "telegram:dm:7:topic:100")
                .unwrap()
                .backend,
            config::AgentBackend::Claude
        );
        assert_eq!(
            cfg.route_for_message("imessage", "imessage:self:me@icloud.com")
                .unwrap()
                .backend,
            config::AgentBackend::Codex
        );
    }

    #[test]
    fn unknown_route_permission_profile_fails_config_load() {
        let path = temp_path("unknown-route-permission");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]

[[routes]]
channel = "imessage"
agent = "claude"
permission_profile = "missing"
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error
            .to_string()
            .contains("invalid permission profile for route"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn job_profile_errors_are_scoped_and_enforce_named_allow_list() {
        let path = temp_path("job-permission-profile");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
job_permission_profiles = ["job-writer"]

[permission_profiles.job-writer]
capability = "workspace"
"#,
        )
        .unwrap();
        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(
            cfg.permission_for_job("job-writer").unwrap().capability,
            config::PermissionCapability::Workspace
        );
        assert!(cfg
            .permission_for_job("missing")
            .unwrap_err()
            .to_string()
            .contains("invalid job permission profile"));
        assert!(cfg
            .permission_for_job("workspace")
            .unwrap_err()
            .to_string()
            .contains("is not included in job_permission_profiles"));
        assert_eq!(
            cfg.route_for_message("imessage", "imessage:self:me@icloud.com")
                .unwrap()
                .permission
                .capability,
            config::PermissionCapability::ReadOnly
        );
        let _ = std::fs::remove_file(path);
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

    fn test_config() -> Config {
        Config {
            channel: "imessage".to_string(),
            channels: Vec::new(),
            primary_delivery: None,
            db_path: "/fake/chat.db".to_string(),
            poll_interval: "1s".to_string(),
            run_timeout: "1s".to_string(),
            self_handles: vec!["me@icloud.com".to_string()],
            allow_from: Vec::new(),
            telegram_bot_token: None,
            telegram_bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            telegram_allow_user_ids: Vec::new(),
            telegram_allow_chat_ids: Vec::new(),
            agent: "codex".to_string(),
            routes: Vec::new(),
            permission_profile: "restricted".to_string(),
            job_permission_profiles: vec!["restricted".to_string()],
            permission_profiles: std::collections::HashMap::new(),
            jobs_dir: "/fake/jobs".to_string(),
            jobs_agent: None,
            jobs_max_timeout: "30m".to_string(),
            jobs_run_dir: "/fake/run".to_string(),
            jobs_max_workers: 2,
            claude_bin: "/fake/claude".to_string(),
            codex_bin: "/fake/codex".to_string(),
            codex_model: None,
            sessions_dir: "/fake/sessions".to_string(),
            state_path: "/fake/state.json".to_string(),
            audit_log_path: "/fake/audit.jsonl".to_string(),
            database_path: "/fake/push.db".to_string(),
            audit_log_content: false,
            assistant_dir: "/fake/assistant".to_string(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }
}
