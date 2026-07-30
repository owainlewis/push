//! push is a tiny iMessage gateway for personal assistant agents. It polls the
//! macOS Messages database for new messages, sends each through a configured
//! coding-agent backend, and texts the reply back.

mod agent;
mod approval;
mod assistant;
mod audit;
mod channel;
mod claude;
mod cli_json;
mod codex;
mod config;
mod doctor;
mod gateway;
mod history;
mod imessage;
mod jobs;
mod markdown;
mod paths;
mod pi;
mod prompt;
mod restart;
mod slack;
mod store;
mod telegram;
#[cfg(test)]
mod test_support;
mod util;
mod voice;

use anyhow::{bail, Context, Result};

const HELP: &str = "Push turns coding agents into a personal assistant you can text.

Usage: push [OPTIONS] [COMMAND]

Commands:
  help              Print this help
  version           Print the installed Push version
  init [path]       Create an assistant repository (default: ./assistant)
  doctor            Validate the configuration and dependencies
  status            Show the installed gateway service status
  paths             Show resolved configuration and storage paths
  reload            Reload the installed gateway service
  restart           Alias for reload
  job validate      Validate all installed jobs
  job list          List installed jobs
  job show <name>   Show an installed job
  job run <name>    Run an installed job
  job runs [name]   Show job run history
  job reviews [name]  Show schedule activation review history

Options:
  --config <path>   Use a configuration file (default: $PUSH_HOME/config.toml)
  --json            Emit stable machine-readable output where supported
  -h, --help        Print help
  -V, --version     Print version

Environment:
  PUSH_HOME         Runtime root (default: ~/.push)
";

#[tokio::main]
async fn main() {
    let raw_args = std::env::args().skip(1).collect::<Vec<_>>();
    let wants_json = raw_args.iter().any(|arg| arg == "--json");
    if !wants_json {
        tracing_subscriber::fmt().with_target(false).init();
    }

    let result = match Args::parse(raw_args) {
        Ok(args) if args.json => run_json(args).await,
        Ok(args) => run_human(args)
            .await
            .map_err(cli_json::CliError::unexpected),
        Err(error) => Err(cli_json::CliError::invalid_input(error)),
    };
    if let Err(error) = result {
        if wants_json {
            cli_json::write_error(&error);
        } else {
            eprintln!("Error: {:#}", error.source());
        }
        std::process::exit(if wants_json { error.exit_code() } else { 1 });
    }
}

async fn run_json(args: Args) -> Result<(), cli_json::CliError> {
    let config_path = if args.command.json_needs_config() {
        Some(args.resolved_config_path().map_err(|error| {
            cli_json::CliError::configuration("Push runtime paths could not be resolved", error)
        })?)
    } else {
        None
    };
    cli_json::run(config_path.as_deref().unwrap_or(""), args.command).await
}

async fn run_human(args: Args) -> Result<()> {
    let explicit_config = args.config_path.is_some();
    let config_path = if args.command.needs_config() {
        Some(args.resolved_config_path()?)
    } else {
        None
    };
    match args.command {
        Command::Help => {
            print!("{HELP}");
            Ok(())
        }
        Command::Version => {
            println!("push {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Init(path) => {
            let config_path = config_path.expect("init resolves a config path");
            let result = assistant::init(&path, &config_path)?;
            println!("Initialized assistant at {}", result.root.display());
            println!(
                "Configured assistant_root in {}",
                result.config_path.display()
            );
            if result.git_initialized {
                println!("Initialized Git repository.");
            }
            println!("\nNext:");
            println!("  Review or configure the channel and its allowlist:");
            println!("    $EDITOR {}", result.config_path.display());
            println!("  Customize the assistant:");
            println!("    $EDITOR {}/SOUL.md", result.root.display());
            println!("    $EDITOR {}/context/README.md", result.root.display());
            println!("  Validate and run:");
            if !explicit_config {
                println!("    push doctor");
                println!("    push");
            } else {
                println!("    push doctor --config {}", result.config_path.display());
                println!("    push --config {}", result.config_path.display());
            }
            Ok(())
        }
        Command::Doctor => doctor::doctor(config_path.as_deref().expect("doctor has config")),
        Command::Status => restart::print_gateway_status(),
        Command::Paths => {
            let cfg = load_run_config(config_path.as_deref().expect("paths has config"))?;
            print!("{}", cli_json::format_paths(&cfg));
            Ok(())
        }
        Command::Restart => restart::gateway(),
        Command::Job(command) => {
            run_job_command(
                config_path.as_deref().expect("job command has config"),
                command,
            )
            .await
        }
        Command::Run => {
            let cfg = load_run_config(config_path.as_deref().expect("run has config"))?;
            doctor::preflight(&cfg).context("preflight")?;
            report_invalid_jobs(&cfg)?;
            gateway::GatewayGroup::new(cfg).context("init")?.run().await
        }
    }
}

fn load_run_config(path: &str) -> Result<config::Config> {
    if let Some(message) = missing_config_message(path) {
        bail!(message);
    }
    let expanded_path = util::expand_home(path);
    let cfg = config::Config::load(path).with_context(|| format!("load config {expanded_path}"))?;
    jobs::Ledger::capture_legacy_schedule_baseline(&cfg)
        .context("capture existing schedule migration baseline")?;
    Ok(cfg)
}

fn missing_config_message(path: &str) -> Option<String> {
    let expanded_path = util::expand_home(path);
    if !matches!(
        std::fs::symlink_metadata(&expanded_path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ) {
        return None;
    }
    if paths::PushPaths::discover()
        .is_ok_and(|paths| paths.config == std::path::Path::new(&expanded_path))
    {
        return Some(format!(
            "configuration not found at {expanded_path}\n\nCreate it with:\n  push init\n\nThen configure a channel and run `push doctor`."
        ));
    }
    let path_arg = shell_quote(path);
    Some(format!(
        "configuration not found at {path}\n\nCreate it with:\n  push init --config {path_arg}\n\nThen configure a channel and run `push doctor --config {path_arg}`."
    ))
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Args {
    command: Command,
    config_path: Option<String>,
    json: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command {
    Help,
    Version,
    Run,
    Init(String),
    Doctor,
    Status,
    Paths,
    Restart,
    Job(JobCommand),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobCommand {
    Validate,
    List,
    Show(String),
    Run(String),
    Runs(Option<String>),
    Reviews(Option<String>),
}

impl Command {
    fn needs_config(&self) -> bool {
        !matches!(
            self,
            Self::Help | Self::Version | Self::Status | Self::Restart
        )
    }

    fn json_needs_config(&self) -> bool {
        matches!(
            self,
            Self::Doctor
                | Self::Paths
                | Self::Job(
                    JobCommand::Validate
                        | JobCommand::List
                        | JobCommand::Show(_)
                        | JobCommand::Runs(_)
                        | JobCommand::Reviews(_)
                )
        )
    }
}

impl Args {
    fn resolved_config_path(&self) -> Result<String> {
        if let Some(path) = &self.config_path {
            return Ok(path.clone());
        }
        paths::PushPaths::discover()?
            .config
            .into_os_string()
            .into_string()
            .map_err(|path| {
                anyhow::anyhow!(
                    "Push config path is not valid UTF-8: {}",
                    std::path::PathBuf::from(path).display()
                )
            })
    }

    fn parse(args: Vec<String>) -> Result<Self> {
        let json = args.iter().any(|arg| arg == "--json");
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
        {
            return Ok(Self {
                command: Command::Help,
                config_path: None,
                json,
            });
        }
        if args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-V" | "--version"))
        {
            return Ok(Self {
                command: Command::Version,
                config_path: None,
                json,
            });
        }

        let mut config_path = None;
        let mut positional = Vec::new();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--config" => {
                    let Some(path) = args.get(i + 1) else {
                        bail!("--config requires a path");
                    };
                    config_path = Some(path.clone());
                    i += 2;
                }
                "--json" => {
                    i += 1;
                }
                value => {
                    positional.push(value.to_string());
                    i += 1;
                }
            }
        }
        let command = match positional.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
            [] => Command::Run,
            ["help"] => Command::Help,
            ["version"] => Command::Version,
            ["init"] => Command::Init("./assistant".to_string()),
            ["init", path] => Command::Init((*path).to_string()),
            ["doctor"] => Command::Doctor,
            ["status"] => Command::Status,
            ["paths"] => Command::Paths,
            ["reload" | "restart"] => Command::Restart,
            ["job", "validate"] => Command::Job(JobCommand::Validate),
            ["job", "list"] => Command::Job(JobCommand::List),
            ["job", "show", name] => Command::Job(JobCommand::Show((*name).to_string())),
            ["job", "run", name] => Command::Job(JobCommand::Run((*name).to_string())),
            ["job", "runs"] => Command::Job(JobCommand::Runs(None)),
            ["job", "runs", name] => Command::Job(JobCommand::Runs(Some((*name).to_string()))),
            ["job", "reviews"] => Command::Job(JobCommand::Reviews(None)),
            ["job", "reviews", name] => {
                Command::Job(JobCommand::Reviews(Some((*name).to_string())))
            }
            _ => bail!(
                "unknown command; expected help, version, init [path], doctor, status, paths, reload, restart, job validate, job list, job show <name>, job run <name>, job runs [<name>], job reviews [<name>], --config <path>, or --json"
            ),
        };
        Ok(Self {
            command,
            config_path,
            json,
        })
    }
}

async fn run_job_command(config_path: &str, command: JobCommand) -> Result<()> {
    let cfg = load_run_config(config_path)?;
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
            print!("{}", jobs::format_catalog_table(&catalog));
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
            let ledger = jobs::Ledger::open(&cfg.paths.database)?;
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
                let evaluation_detail =
                    if run.evaluation_result.is_none() && run.evaluation_error.is_none() {
                        String::new()
                    } else {
                        jobs::format_evaluation_detail(
                            run.evaluation_result.as_deref(),
                            run.evaluation_error.as_deref(),
                        )
                        .replace('\n', " ")
                    };
                let delivery_error = run.delivery_error.unwrap_or_default().replace('\n', " ");
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
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
                    run.evaluation_state,
                    evaluation_detail,
                    delivery_error,
                );
            }
            Ok(())
        }
        JobCommand::Reviews(name) => {
            if let Some(name) = name.as_deref() {
                jobs::validate_job_name(name)?;
            }
            let ledger = jobs::Ledger::open(&cfg.paths.database)?;
            for review in ledger.schedule_reviews(name.as_deref())? {
                let schedules = review
                    .schedules
                    .iter()
                    .map(|trigger| {
                        format!("{}:{:?}:{}", trigger.id, trigger.schedule, trigger.timezone)
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}:{}\t{}\t{}",
                    review.review_id,
                    review.job_name,
                    review.status,
                    review.content_hash,
                    schedules,
                    review.backend,
                    review.timeout_ms,
                    review.workdir,
                    review.delivery_channel,
                    review.delivery_target,
                    review.reviewed_by.unwrap_or_else(|| "-".to_string()),
                    review.reason.unwrap_or_else(|| "-".to_string()),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Config;
    use crate::test_support::{temp_dir, temp_path, test_config};

    fn write_config_with_assistant(path: &Path, body: &str) -> std::path::PathBuf {
        let assistant = temp_dir("config-assistant");
        std::fs::write(path, format!("assistant_root = {:?}\n{body}", assistant)).unwrap();
        assistant
    }

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
                config_path: Some("custom.toml".to_string()),
                json: false,
            }
        );
    }

    #[test]
    fn parses_restart_with_config_path() {
        let args = Args::parse(vec![
            "--config".to_string(),
            "custom.toml".to_string(),
            "restart".to_string(),
        ])
        .unwrap();

        assert_eq!(
            args,
            Args {
                command: Command::Restart,
                config_path: Some("custom.toml".to_string()),
                json: false,
            }
        );
    }

    #[test]
    fn parses_reload_as_restart() {
        assert_eq!(
            Args::parse(vec!["reload".into()]).unwrap().command,
            Command::Restart
        );
    }

    #[test]
    fn parses_version_command_and_flag() {
        for args in [
            vec!["version".into()],
            vec!["--version".into()],
            vec!["-V".into()],
        ] {
            assert_eq!(Args::parse(args).unwrap().command, Command::Version);
        }
    }

    #[test]
    fn parses_help_without_treating_it_as_a_command_argument() {
        assert_eq!(
            Args::parse(vec!["help".into()]).unwrap(),
            Args {
                command: Command::Help,
                config_path: None,
                json: false,
            }
        );
        assert_eq!(
            Args::parse(vec!["--help".into()]).unwrap(),
            Args {
                command: Command::Help,
                config_path: None,
                json: false,
            }
        );
        assert_eq!(
            Args::parse(vec!["job".into(), "--help".into()])
                .unwrap()
                .command,
            Command::Help
        );
    }

    #[test]
    fn cli_reference_covers_every_help_command() {
        let reference = include_str!("../docs/reference/cli.md").replace(['<', '>'], "");
        let commands = HELP
            .split("Commands:\n")
            .nth(1)
            .unwrap()
            .split("\n\nOptions:")
            .next()
            .unwrap();

        for line in commands.lines().filter(|line| !line.trim().is_empty()) {
            let command = line
                .trim()
                .split("  ")
                .next()
                .unwrap()
                .replace(['<', '>'], "");
            assert!(
                reference.contains(&format!("push {command}")),
                "docs/reference/cli.md does not document `push {command}`"
            );
        }
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
                config_path: Some("x.toml".to_string()),
                json: false,
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
        assert_eq!(
            Args::parse(vec!["job".into(), "reviews".into(), "daily".into()])
                .unwrap()
                .command,
            Command::Job(JobCommand::Reviews(Some("daily".to_string())))
        );
    }

    #[test]
    fn parses_init_path_and_default() {
        assert_eq!(
            Args::parse(vec!["init".into()]).unwrap().command,
            Command::Init("./assistant".to_string())
        );
        assert_eq!(
            Args::parse(vec![
                "init".into(),
                "~/Code/assistant".into(),
                "--config".into(),
                "custom.toml".into(),
            ])
            .unwrap(),
            Args {
                command: Command::Init("~/Code/assistant".to_string()),
                config_path: Some("custom.toml".to_string()),
                json: false,
            }
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
    fn defaults_to_user_config_path() {
        assert_eq!(
            Args::parse(Vec::new()).unwrap(),
            Args {
                command: Command::Run,
                config_path: None,
                json: false,
            }
        );
    }

    #[test]
    fn parses_json_as_a_global_option() {
        assert_eq!(
            Args::parse(vec!["job".into(), "--json".into(), "list".into()]).unwrap(),
            Args {
                command: Command::Job(JobCommand::List),
                config_path: None,
                json: true,
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
        assert_eq!(
            cfg.telegram_bot_token.as_deref(),
            Some("replace-with-the-token-from-BotFather")
        );
        assert_eq!(cfg.telegram_allow_user_ids, [123456789]);
        assert!(cfg.telegram_allow_chat_ids.is_empty());
        assert_eq!(cfg.jobs_agent, None);
        assert_eq!(cfg.jobs_max_timeout, "30m");
        assert_eq!(cfg.jobs_max_workers, 2);
        assert_eq!(
            cfg.jobs_dir,
            Path::new(&std::env::var("HOME").unwrap())
                .join("Code/assistant/jobs")
                .to_string_lossy()
        );
        assert_eq!(
            cfg.paths.database,
            Path::new(&std::env::var("HOME").unwrap()).join(".push/push.db")
        );
        assert_eq!(
            cfg.assistant_root,
            Path::new(&std::env::var("HOME").unwrap())
                .join("Code/assistant")
                .to_string_lossy()
        );
        assert_eq!(cfg.assistant_dir, cfg.assistant_root);
    }

    #[test]
    fn assistant_root_is_canonical_and_derives_identity_context_and_jobs() {
        let root = temp_dir("assistant-root-config");
        std::fs::create_dir(root.join("context")).unwrap();
        let path = temp_path("assistant-root-config-file");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\n",
                root
            ),
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        let canonical = std::fs::canonicalize(&root).unwrap();
        assert_eq!(Path::new(&cfg.assistant_root), canonical);
        assert_eq!(cfg.assistant_dir, cfg.assistant_root);
        assert_eq!(Path::new(&cfg.jobs_dir), canonical.join("jobs"));
        assert_eq!(
            cfg.backend_context_dir().unwrap().unwrap(),
            canonical.join("context")
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compatible_legacy_assistant_and_jobs_paths_still_load() {
        let root = temp_dir("legacy-assistant-config");
        let path = temp_path("legacy-assistant-config-file");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_dir = {:?}\njobs_dir = {:?}\n",
                root,
                root.join("jobs")
            ),
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(
            Path::new(&cfg.assistant_root),
            std::fs::canonicalize(&root).unwrap()
        );
        assert_eq!(
            Path::new(&cfg.jobs_dir),
            std::fs::canonicalize(&root).unwrap().join("jobs")
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_layout_with_inline_token_remains_compatible() {
        let root = temp_dir("legacy-assistant-inline-token");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "channel = 'telegram'\nassistant_dir = {:?}\njobs_dir = {:?}\n[telegram]\nbot_token = 'legacy-secret'\nallow_user_ids = [1]\n",
                root,
                root.join("jobs")
            ),
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.telegram_bot_token.as_deref(), Some("legacy-secret"));
        assert_eq!(Path::new(&cfg.assistant_root), root.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn divergent_legacy_paths_report_actionable_migration() {
        let root = temp_dir("divergent-legacy-assistant");
        let jobs = temp_dir("divergent-legacy-jobs");
        let path = temp_path("divergent-legacy-config");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_dir = {:?}\njobs_dir = {:?}\n",
                root, jobs
            ),
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error
            .to_string()
            .contains("do not form one assistant repository"));
        assert!(error.to_string().contains("assistant_root"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(jobs);
    }

    #[test]
    fn relative_assistant_root_resolves_from_config_directory() {
        let root = temp_dir("relative-assistant-root");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            "self_handles = [\"me@icloud.com\"]\nassistant_root = \".\"\n",
        )
        .unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(
            Path::new(&cfg.assistant_root),
            std::fs::canonicalize(&root).unwrap()
        );
        assert_eq!(
            Path::new(&cfg.jobs_dir),
            std::fs::canonicalize(&root).unwrap().join("jobs")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn new_and_legacy_assistant_keys_cannot_be_mixed() {
        let root = temp_dir("mixed-assistant-config");
        let path = temp_path("mixed-assistant-config-file");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\nassistant_dir = {:?}\n",
                root, root
            ),
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("replaces legacy"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_assistant_root_rejects_runtime_state_inside_repository() {
        let root = temp_dir("assistant-runtime-boundary");
        let path = temp_path("assistant-runtime-boundary-config");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\ndatabase_path = {:?}\n",
                root,
                root.join("push.db")
            ),
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error
            .to_string()
            .contains("database_path must stay outside assistant_root"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_runtime_paths_take_precedence_over_push_home_defaults() {
        let runtime = temp_dir("runtime-path-defaults");
        let assistant = temp_dir("runtime-path-assistant");
        let legacy = temp_dir("runtime-path-overrides");
        let path = temp_path("runtime-path-config");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\nstate_path = {:?}\ndatabase_path = {:?}\naudit_log_path = {:?}\njobs_run_dir = {:?}\n",
                assistant,
                legacy.join("state.json"),
                legacy.join("push.db"),
                legacy.join("audit.jsonl"),
                legacy.join("run"),
            ),
        )
        .unwrap();

        let cfg = Config::load_with_paths(
            path.to_str().unwrap(),
            crate::paths::PushPaths::from_root(runtime.clone()).unwrap(),
        )
        .unwrap();

        assert_eq!(cfg.paths.root, runtime);
        assert_eq!(cfg.paths.config, cfg.paths.root.join("config.toml"));
        assert_eq!(cfg.paths.state, legacy.join("state.json"));
        assert_eq!(cfg.paths.database, legacy.join("push.db"));
        assert_eq!(cfg.paths.audit, legacy.join("audit.jsonl"));
        assert_eq!(cfg.paths.jobs_run, legacy.join("run"));
        assert_eq!(cfg.paths.inbox, legacy.join("state.json.slack-inbox.db"));
        assert_eq!(cfg.paths.cache, cfg.paths.root.join("cache"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(assistant);
        let _ = std::fs::remove_dir_all(legacy);
        let _ = std::fs::remove_dir_all(cfg.paths.root);
    }

    #[test]
    fn assistant_root_cannot_overlap_push_home() {
        let runtime = temp_dir("runtime-overlap");
        let assistant = runtime.join("assistant");
        std::fs::create_dir(&assistant).unwrap();
        let path = temp_path("runtime-overlap-config");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\n",
                assistant
            ),
        )
        .unwrap();

        let error = Config::load_with_paths(
            path.to_str().unwrap(),
            crate::paths::PushPaths::from_root(runtime.clone()).unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must stay outside Push home"));
        assert!(error.to_string().contains("set PUSH_HOME"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(runtime);
    }

    #[test]
    fn legacy_assistant_layout_cannot_overlap_push_home() {
        let runtime = temp_dir("legacy-runtime-overlap");
        let path = temp_path("legacy-runtime-overlap-config");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_dir = {:?}\njobs_dir = {:?}\n",
                runtime,
                runtime.join("jobs")
            ),
        )
        .unwrap();

        let error = Config::load_with_paths(
            path.to_str().unwrap(),
            crate::paths::PushPaths::from_root(runtime.clone()).unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must stay outside Push home"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(runtime);
    }

    #[test]
    fn implicit_legacy_assistant_layout_cannot_overlap_push_home() {
        let runtime = std::path::PathBuf::from(crate::util::expand_home("~/.push"));
        let path = temp_path("implicit-legacy-runtime-overlap");
        std::fs::write(&path, "self_handles = [\"me@icloud.com\"]\n").unwrap();

        let error = Config::load_with_paths(
            path.to_str().unwrap(),
            crate::paths::PushPaths::from_root(runtime.clone()).unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must stay outside Push home"));
        assert!(error.to_string().contains("separate assistant repository"));
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_push_home_cannot_hide_assistant_overlap() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("symlink-runtime-overlap");
        let runtime = root.join("runtime");
        let linked_runtime = root.join("linked-runtime");
        let assistant = runtime.join("assistant");
        std::fs::create_dir_all(&assistant).unwrap();
        symlink(&runtime, &linked_runtime).unwrap();
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "self_handles = [\"me@icloud.com\"]\nassistant_root = {:?}\n",
                assistant
            ),
        )
        .unwrap();

        let error = Config::load_with_paths(
            path.to_str().unwrap(),
            crate::paths::PushPaths::from_root(linked_runtime).unwrap(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("must stay outside Push home"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn config_load_rejects_an_inline_token_added_inside_the_assistant() {
        let root = temp_dir("assistant-inline-token");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            "channel = 'telegram'\nassistant_root = '.'\n[telegram]\nbot_token = 'committed-secret'\nallow_user_ids = [1]\n",
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("inline Telegram token"));
        assert!(error.to_string().contains("TELEGRAM_BOT_TOKEN"));
        let _ = std::fs::remove_dir_all(root);
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

        assert!(error.to_string().contains("assistant_root/SOUL.md"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn provider_sections_load_channel_settings() {
        let path = temp_path("provider-section-config");
        let assistant = write_config_with_assistant(
            &path,
            r#"channel = "telegram"
agent = "codex"

[imessage]
db_path = "/tmp/messages.db"
self_handles = ["me@example.com"]

[telegram]
allow_user_ids = [7]
allow_chat_ids = [9]

[slack]
app_token = "xapp-config"
bot_token = "xoxb-config"
allow_user_ids = ["U1"]

[voice]
openai_api_key = "config-openai-key"
name = "onyx"
"#,
        );

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert_eq!(cfg.db_path, "/tmp/messages.db");
        assert_eq!(cfg.self_handles, ["me@example.com"]);
        assert_eq!(cfg.telegram_allow_user_ids, [7]);
        assert_eq!(cfg.telegram_allow_chat_ids, [9]);
        assert_eq!(cfg.slack_app_token.as_deref(), Some("xapp-config"));
        assert_eq!(cfg.slack_bot_token.as_deref(), Some("xoxb-config"));
        assert_eq!(cfg.slack_allow_user_ids, ["U1"]);
        assert_eq!(
            cfg.voice_openai_api_key.as_deref(),
            Some("config-openai-key")
        );
        assert_eq!(cfg.voice_name, "onyx");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(assistant);
    }

    #[test]
    fn slack_config_requires_an_explicit_user_allowlist() {
        let path = temp_path("slack-allowlist-config");
        let assistant = write_config_with_assistant(
            &path,
            r#"channel = "slack"
[slack]
app_token = "xapp-config"
bot_token = "xoxb-config"
allow_user_ids = []
"#,
        );

        let error = Config::load(path.to_str().unwrap()).unwrap_err();
        assert!(error
            .to_string()
            .contains("set slack.allow_user_ids to explicit Slack user IDs"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(assistant);
    }

    #[test]
    fn voice_config_defaults_to_cedar_and_rejects_unknown_names() {
        let default_path = temp_path("default-voice-config");
        let default_assistant =
            write_config_with_assistant(&default_path, "self_handles = ['me@icloud.com']\n");

        let cfg = Config::load(default_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.voice_name, "cedar");

        let invalid_path = temp_path("invalid-voice-config");
        let invalid_assistant = write_config_with_assistant(
            &invalid_path,
            "self_handles = ['me@icloud.com']\n[voice]\nname = 'unknown'\n",
        );

        let error = Config::load(invalid_path.to_str().unwrap()).unwrap_err();
        assert!(error.to_string().contains("invalid voice.name \"unknown\""));
        let _ = std::fs::remove_file(default_path);
        let _ = std::fs::remove_file(invalid_path);
        let _ = std::fs::remove_dir_all(default_assistant);
        let _ = std::fs::remove_dir_all(invalid_assistant);
    }

    #[test]
    fn voice_config_rejects_an_empty_openai_key() {
        let path = temp_path("empty-voice-key-config");
        let assistant = write_config_with_assistant(
            &path,
            r#"self_handles = ["me@icloud.com"]

[voice]
openai_api_key = " "
"#,
        );

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error
            .to_string()
            .contains("voice.openai_api_key cannot be empty"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(assistant);
    }

    #[test]
    fn config_load_rejects_an_inline_voice_key_inside_the_assistant() {
        let root = temp_dir("assistant-inline-voice-key");
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            "self_handles = ['me@icloud.com']\nassistant_root = '.'\n[voice]\nopenai_api_key = 'committed-secret'\n",
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("inline OpenAI API key"));
        assert!(error.to_string().contains("OPENAI_API_KEY"));
        let _ = std::fs::remove_dir_all(root);
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
    fn removed_runtime_settings_fail_with_migration_help() {
        let path = temp_path("removed-runtime-setting-config");
        for (key, replacement) in [
            ("claude_bin", "service PATH"),
            ("codex_bin", "service PATH"),
            ("pi_bin", "service PATH"),
            ("codex_model", "configure the model in Codex"),
            ("sessions_dir", "remove this key"),
            ("reply_marker", "remove this key"),
        ] {
            std::fs::write(&path, format!("{key} = 'legacy-value'\n")).unwrap();

            let error = Config::load(path.to_str().unwrap()).unwrap_err();

            assert!(error.to_string().contains(key));
            assert!(error.to_string().contains("no longer configurable"));
            assert!(error.to_string().contains(replacement));
        }
        std::fs::write(
            &path,
            "[telegram]\nbot_token_env = 'LEGACY_TOKEN'\nallow_user_ids = [7]\n",
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("telegram.bot_token_env"));
        assert!(error.to_string().contains("TELEGRAM_BOT_TOKEN"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_missing_config_path_arg() {
        let err = Args::parse(vec!["--config".to_string()]).unwrap_err();
        assert!(err.to_string().contains("--config requires a path"));
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

        assert!(error.to_string().contains("selected agent"));
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

        assert!(error.to_string().contains("selected agent"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn removed_permission_profiles_have_an_actionable_error() {
        let path = temp_path("inherit-profile-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
permission_profile = "inherit"

[permission_profiles.trusted]
capability = "inherit"

[[routes]]
thread = "imessage:self:me@icloud.com"
agent = "codex"
permission_profile = "trusted"
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("selected agent"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn removed_permission_profile_tables_have_an_actionable_error() {
        let path = temp_path("inherit-redefined-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]

[permission_profiles.inherit]
capability = "read-only"
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("selected agent"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn config_rejects_removed_job_permission_profiles_key() {
        let path = temp_path("job-permission-profiles-config");
        std::fs::write(
            &path,
            r#"self_handles = ["me@icloud.com"]
job_permission_profiles = ["restricted"]
"#,
        )
        .unwrap();

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("job_permission_profiles"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn loaded_config_file_is_shielded_from_job_workdirs() {
        let dir = temp_dir("config-shield-load");
        let path = dir.join("config.toml");
        let assistant = write_config_with_assistant(&path, "self_handles = [\"me@icloud.com\"]\n");

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert!(cfg
            .validate_job_workdir(&dir)
            .unwrap_err()
            .to_string()
            .contains("config file"));
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(assistant);
    }

    #[test]
    fn multi_channel_config_is_opt_in_and_defers_primary_resolution() {
        let path = temp_path("multi-channel-config");
        let assistant = write_config_with_assistant(
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
        );

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
        let _ = std::fs::remove_dir_all(assistant);
    }

    #[test]
    fn duplicate_enabled_channels_are_rejected() {
        let path = temp_path("duplicate-channel-config");
        let assistant = write_config_with_assistant(
            &path,
            r#"channels = ["telegram", "telegram"]
[telegram]
bot_token = "secret"
allow_user_ids = [7]
"#,
        );

        let error = Config::load(path.to_str().unwrap()).unwrap_err();

        assert!(error.to_string().contains("duplicate enabled channel"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(assistant);
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
            },
            config::RouteRule {
                thread: Some("telegram:dm:7".to_string()),
                channel: None,
                agent: "claude".to_string(),
            },
            config::RouteRule {
                thread: Some("telegram:dm:7:topic:99".to_string()),
                channel: None,
                agent: "codex".to_string(),
            },
            config::RouteRule {
                thread: Some("self:me@icloud.com".to_string()),
                channel: None,
                agent: "codex".to_string(),
            },
            config::RouteRule {
                thread: Some("imessage:self:me@icloud.com".to_string()),
                channel: None,
                agent: "claude".to_string(),
            },
        ];

        assert_eq!(
            cfg.route_for_message("telegram", &[vec!["telegram:dm:7".to_string()]])
                .unwrap()
                .backend,
            config::AgentBackend::Claude
        );
        assert_eq!(
            cfg.route_for_message("telegram", &[vec!["telegram:dm:8".to_string()]])
                .unwrap()
                .backend,
            config::AgentBackend::Codex
        );
        assert_eq!(
            cfg.route_for_message(
                "telegram",
                &[
                    vec!["telegram:dm:7:topic:99".to_string()],
                    vec!["telegram:dm:7".to_string()],
                ],
            )
            .unwrap()
            .backend,
            config::AgentBackend::Codex
        );
        assert_eq!(
            cfg.route_for_message(
                "telegram",
                &[
                    vec!["telegram:dm:7:topic:100".to_string()],
                    vec!["telegram:dm:7".to_string()],
                ],
            )
            .unwrap()
            .backend,
            config::AgentBackend::Claude
        );
        assert_eq!(
            cfg.route_for_message(
                "imessage",
                &[vec![
                    "imessage:self:me@icloud.com".to_string(),
                    "self:me@icloud.com".to_string(),
                ]],
            )
            .unwrap()
            .backend,
            config::AgentBackend::Codex
        );
    }

    #[test]
    fn removed_route_permission_profile_has_an_actionable_error() {
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

        assert!(error.to_string().contains("selected agent"));
        let _ = std::fs::remove_file(path);
    }
}
