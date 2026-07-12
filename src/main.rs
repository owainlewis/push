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
mod doctor;
mod drafts;
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
mod util;

use anyhow::{bail, Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_target(false).init();

    let args = Args::parse(std::env::args().skip(1).collect())?;
    match args.command {
        Command::Doctor => doctor::doctor(&args.config_path),
        Command::Job(command) => run_job_command(&args.config_path, command).await,
        Command::Run => {
            let cfg = config::Config::load(&args.config_path).context("config")?;
            doctor::preflight(&cfg).context("preflight")?;
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
                println!("{}\tvalid\t{}", job.name, job.backend.as_str());
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::config::Config;
    use crate::test_support::{temp_dir, temp_path, test_config};

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
        std::fs::write(&path, "self_handles = [\"me@icloud.com\"]\n").unwrap();

        let cfg = Config::load(path.to_str().unwrap()).unwrap();

        assert!(cfg
            .validate_job_workdir(&dir)
            .unwrap_err()
            .to_string()
            .contains("config file"));
        let _ = std::fs::remove_dir_all(dir);
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
}
