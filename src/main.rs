//! push is a tiny iMessage gateway for personal assistant agents. It polls the
//! macOS Messages database for new messages, sends each through a configured
//! coding-agent backend, and texts the reply back.

mod agent;
mod claude;
mod codex;
mod config;
mod gateway;
mod imessage;
mod memory;
mod store;
#[cfg(test)]
mod test_support;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let args = Args::parse(std::env::args().skip(1).collect())?;
    if args.command == Command::Doctor {
        return doctor(&args.config_path);
    }
    let cfg = config::Config::load(&args.config_path).context("config")?;
    preflight(&cfg).context("preflight")?;
    gateway::Gateway::new(cfg).context("init")?.run().await
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
}

impl Args {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut command = Command::Run;
        let mut config_path = "config.json".to_string();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "doctor" if command == Command::Run => {
                    command = Command::Doctor;
                    i += 1;
                }
                "--config" => {
                    let Some(path) = args.get(i + 1) else {
                        bail!("--config requires a path");
                    };
                    config_path = path.clone();
                    i += 2;
                }
                other => bail!("unknown argument {other:?}; expected doctor or --config <path>"),
            }
        }
        Ok(Self {
            command,
            config_path,
        })
    }
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
                        "cannot load {config_path}: {e}. Create the file from config.example.json or fix the invalid value."
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
    check_imessage_db(cfg, &mut checks);
    check_bins(cfg, &mut checks);
    CheckReport { checks }
}

fn check_config(cfg: &config::Config, checks: &mut Vec<Check>) {
    checks.push(Check::pass(
        "config",
        format!(
            "agent={}, self_handles={}, allow_from={}",
            cfg.agent,
            cfg.self_handles.len(),
            cfg.allow_from.len()
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
                "Messages database not found at {}. Sign in to iMessage or set db_path.",
                cfg.db_path
            ),
        )),
        Err(e) => checks.push(Check::fail(
            "iMessage database",
            format!(
                "cannot open {}: {e}. Check db_path and Messages permissions, then rerun doctor.",
                cfg.db_path
            ),
        )),
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
    bins.push("osascript");
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
    use crate::config::{AssistantProfile, Config};
    use crate::test_support::{temp_dir, temp_path};

    #[test]
    fn parses_doctor_with_config_path() {
        let args = Args::parse(vec![
            "doctor".to_string(),
            "--config".to_string(),
            "custom.json".to_string(),
        ])
        .unwrap();

        assert_eq!(
            args,
            Args {
                command: Command::Doctor,
                config_path: "custom.json".to_string(),
            }
        );
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
    fn doctor_reports_invalid_json_config() {
        let path = temp_path("invalid-json-config");
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
            r#"{
  "self_handles": ["me@icloud.com"],
  "agent": "bogus"
}"#,
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
            r#"{
  "self_handles": ["me@icloud.com"],
  "claude_allowed_tools": ["Read", " "]
}"#,
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
            r#"{
  "self_handles": ["me@icloud.com"],
  "claude_tools": []
}"#,
        )
        .unwrap();

        let err = doctor(path.to_str().unwrap()).unwrap_err();

        assert!(err.to_string().contains("doctor found 1 failed check"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_accepts_unprefixed_claude_tool_filter_aliases() {
        let path = temp_path("claude-tool-alias-config");
        std::fs::write(
            &path,
            r#"{
  "self_handles": ["me@icloud.com"],
  "allowed_tools": ["Read"],
  "disallowed_tools": ["Edit"]
}"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.claude_allowed_tools, vec!["Read"]);
        assert_eq!(cfg.claude_disallowed_tools, vec!["Edit"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn doctor_accepts_unprefixed_claude_tools_alias() {
        let path = temp_path("claude-tools-alias-config");
        std::fs::write(
            &path,
            r#"{
  "self_handles": ["me@icloud.com"],
  "tools": ["Read", "Grep"]
}"#,
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(
            cfg.claude_tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
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
    fn run_checks_reports_config_and_writable_paths() {
        let db_path = temp_path("chat-db");
        std::fs::write(&db_path, "").unwrap();
        let state_path = temp_path("state-dir").join("state.json");
        let sessions_dir = temp_dir("sessions-dir");
        let mut cfg = test_config();
        cfg.db_path = db_path.to_string_lossy().to_string();
        cfg.state_path = state_path.to_string_lossy().to_string();
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
            check.name == "iMessage database" && matches!(check.status, CheckStatus::Pass)
        }));

        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(state_path);
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
            db_path: "/fake/chat.db".to_string(),
            poll_interval: "1s".to_string(),
            run_timeout: "1s".to_string(),
            self_handles: vec!["me@icloud.com".to_string()],
            allow_from: Vec::new(),
            agent: "codex".to_string(),
            routes: Vec::new(),
            assistant: AssistantProfile::default(),
            claude_bin: "/fake/claude".to_string(),
            claude_permission_mode: "bypassPermissions".to_string(),
            claude_tools: None,
            claude_allowed_tools: Vec::new(),
            claude_disallowed_tools: Vec::new(),
            codex_bin: "/fake/codex".to_string(),
            codex_sandbox: "workspace-write".to_string(),
            codex_approval_policy: "never".to_string(),
            codex_model: None,
            sessions_dir: "/fake/sessions".to_string(),
            state_path: "/fake/state.json".to_string(),
            assistant_dir: "/fake/assistant".to_string(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }
}
