//! Stable JSON output for automation-facing CLI commands.

use std::io::{self, Write};

use anyhow::anyhow;
use serde::Serialize;
use serde_json::{json, Value};

use crate::{config, doctor, jobs, restart, Command, JobCommand, HELP};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // The public contract reserves categories not yet emitted by read-only commands.
pub(crate) enum ErrorCategory {
    InvalidInput,
    Configuration,
    UnavailableDependency,
    TransientTransport,
    Conflict,
    Unexpected,
}

impl ErrorCategory {
    fn exit_code(self) -> i32 {
        match self {
            Self::InvalidInput => 2,
            Self::Configuration => 3,
            Self::UnavailableDependency => 4,
            Self::TransientTransport => 5,
            Self::Conflict => 6,
            Self::Unexpected => 70,
        }
    }

    fn retryable(self) -> Option<bool> {
        match self {
            Self::InvalidInput
            | Self::Configuration
            | Self::UnavailableDependency
            | Self::Conflict => Some(false),
            Self::TransientTransport => Some(true),
            Self::Unexpected => None,
        }
    }
}

pub(crate) struct CliError {
    category: ErrorCategory,
    message: String,
    source: anyhow::Error,
    details: Option<Value>,
}

impl CliError {
    pub(crate) fn invalid_input(source: anyhow::Error) -> Self {
        Self::new(ErrorCategory::InvalidInput, source.to_string(), source)
    }

    pub(crate) fn configuration(message: impl Into<String>, source: anyhow::Error) -> Self {
        Self::new(ErrorCategory::Configuration, message, source)
    }

    pub(crate) fn unavailable_dependency(
        message: impl Into<String>,
        source: anyhow::Error,
    ) -> Self {
        Self::new(ErrorCategory::UnavailableDependency, message, source)
    }

    pub(crate) fn unexpected(source: anyhow::Error) -> Self {
        let message = source.to_string();
        Self::new(ErrorCategory::Unexpected, message, source)
    }

    fn new(category: ErrorCategory, message: impl Into<String>, source: anyhow::Error) -> Self {
        Self {
            category,
            message: message.into(),
            source,
            details: None,
        }
    }

    fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub(crate) fn exit_code(&self) -> i32 {
        self.category.exit_code()
    }

    pub(crate) fn source(&self) -> &anyhow::Error {
        &self.source
    }
}

#[derive(Serialize)]
struct SuccessEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    command: &'a str,
    data: Value,
}

#[derive(Serialize)]
struct ErrorEnvelope<'a> {
    schema_version: u32,
    ok: bool,
    error: ErrorBody<'a>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    category: ErrorCategory,
    message: &'a str,
    exit_code: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

pub(crate) async fn run(config_path: &str, command: Command) -> Result<(), CliError> {
    match command {
        Command::Help => write_success("help", json!({ "text": HELP })),
        Command::Version => write_success(
            "version",
            json!({
                "name": "push",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        ),
        Command::Doctor => run_doctor(config_path),
        Command::Status => {
            let status = restart::gateway_status().map_err(|error| {
                CliError::unavailable_dependency(
                    "the gateway service manager is unavailable",
                    error,
                )
            })?;
            write_success(
                "status",
                serde_json::to_value(status).expect("status serializes"),
            )
        }
        Command::Paths => {
            let cfg = load_config(config_path)?;
            write_success("paths", paths_value(&cfg))
        }
        Command::Job(command) => run_job_command(config_path, command),
        Command::Run | Command::Init(_) | Command::Restart => Err(CliError::invalid_input(
            anyhow!("--json is not supported for commands that start or mutate runtime state"),
        )),
    }
}

fn run_doctor(config_path: &str) -> Result<(), CliError> {
    let cfg = load_config(config_path)?;
    let report = doctor::report(&cfg);
    let data = serde_json::to_value(&report).expect("doctor report serializes");
    if report.is_ok() {
        write_success("doctor", data)
    } else {
        let failed = report.failed_count();
        let category = if report.has_unavailable_dependency() {
            ErrorCategory::UnavailableDependency
        } else {
            ErrorCategory::Configuration
        };
        Err(CliError::new(
            category,
            format!("doctor found {failed} failed check(s)"),
            anyhow!("doctor found {failed} failed check(s)"),
        )
        .with_details(data))
    }
}

fn run_job_command(config_path: &str, command: JobCommand) -> Result<(), CliError> {
    let cfg = load_config(config_path)?;
    match command {
        JobCommand::Validate => {
            let catalog = jobs::Catalog::load(&cfg).map_err(|error| {
                CliError::configuration("the installed jobs could not be inspected", error)
            })?;
            let data = catalog_value(&catalog);
            if catalog.errors.is_empty() {
                write_success("job.validate", data)
            } else {
                let count = catalog.errors.len();
                Err(CliError::new(
                    ErrorCategory::InvalidInput,
                    format!("{count} installed job(s) are invalid"),
                    anyhow!("{count} installed job(s) are invalid"),
                )
                .with_details(data))
            }
        }
        JobCommand::List => {
            let catalog = jobs::Catalog::load(&cfg).map_err(|error| {
                CliError::configuration("the installed jobs could not be inspected", error)
            })?;
            write_success("job.list", catalog_value(&catalog))
        }
        JobCommand::Show(name) => {
            jobs::validate_job_name(&name).map_err(CliError::invalid_input)?;
            let job = jobs::Catalog::load_named(&cfg, &name).map_err(|error| {
                CliError::configuration(
                    format!("job {name:?} could not be loaded or validated"),
                    error,
                )
            })?;
            write_success("job.show", job_value(&job))
        }
        JobCommand::Runs(name) => {
            if let Some(name) = name.as_deref() {
                jobs::validate_job_name(name).map_err(CliError::invalid_input)?;
            }
            let ledger = jobs::Ledger::open(&cfg.database_path).map_err(|error| {
                CliError::configuration("the job run ledger could not be opened", error)
            })?;
            let rows = ledger.runs(name.as_deref()).map_err(|error| {
                CliError::configuration("the job run ledger could not be read", error)
            })?;
            let runs = rows
                .into_iter()
                .map(|run| {
                    json!({
                        "id": run.id,
                        "job_name": run.job_name,
                        "state": run.state,
                        "backend": run.backend,
                        "queued_at_ms": run.queued_at_ms,
                        "trigger": {
                            "kind": run.trigger_kind,
                            "id": run.trigger_id,
                            "scheduled_at_ms": run.scheduled_at_ms,
                        },
                        "execution": {
                            "has_result": run.result.is_some(),
                            "has_error": run.error.is_some(),
                        },
                        "evaluation": {
                            "state": run.evaluation_state,
                            "has_result": run.evaluation_result.is_some(),
                            "has_error": run.evaluation_error.is_some(),
                        },
                        "delivery": {
                            "state": run.delivery_state,
                            "attempts": run.delivery_attempts,
                            "has_error": run.delivery_error.is_some(),
                            "channel": run.delivery_channel,
                            "target": run.delivery_target,
                        },
                    })
                })
                .collect::<Vec<_>>();
            write_success(
                "job.runs",
                json!({
                    "job_name": name,
                    "runs": runs,
                }),
            )
        }
        JobCommand::Run(_) => Err(CliError::invalid_input(anyhow!(
            "--json is not supported for `job run` because an interrupted mutation can have an unknown outcome"
        ))),
    }
}

fn load_config(path: &str) -> Result<config::Config, CliError> {
    if crate::missing_config_message(path).is_some() {
        return Err(CliError::configuration(
            format!("configuration not found at {path}"),
            anyhow!("configuration not found at {path}"),
        ));
    }
    config::Config::load(path).map_err(|error| {
        CliError::configuration(
            format!("configuration at {path} could not be loaded; run `push doctor` for details"),
            error,
        )
    })
}

fn catalog_value(catalog: &jobs::Catalog) -> Value {
    let valid = catalog
        .jobs
        .values()
        .map(|job| {
            json!({
                "name": job.name,
                "status": "valid",
                "path": job.path.to_string_lossy(),
                "backend": job.backend.as_str(),
            })
        })
        .collect::<Vec<_>>();
    let invalid = catalog
        .errors
        .iter()
        .map(|error| {
            json!({
                "name": error.name,
                "status": "invalid",
                "path": error.path.to_string_lossy(),
                "message": error.message,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "valid_count": valid.len(),
        "invalid_count": invalid.len(),
        "valid": valid,
        "invalid": invalid,
    })
}

fn job_value(job: &jobs::Job) -> Value {
    let triggers = job
        .triggers
        .iter()
        .map(|trigger| {
            json!({
                "id": trigger.id,
                "kind": trigger.kind,
                "schedule": trigger.schedule,
                "timezone": trigger.timezone,
                "enabled": trigger.enabled,
            })
        })
        .collect::<Vec<_>>();
    json!({
        "name": job.name,
        "path": job.path.to_string_lossy(),
        "backend": job.backend.as_str(),
        "timeout_ms": u64::try_from(job.timeout.as_millis()).unwrap_or(u64::MAX),
        "workdir": job.workdir.to_string_lossy(),
        "snapshot_hash": job.snapshot_hash,
        "evals": job.evals.iter().map(|eval| &eval.name).collect::<Vec<_>>(),
        "triggers": triggers,
        "body": job.body,
    })
}

fn paths_value(cfg: &config::Config) -> Value {
    json!({
        "config": cfg.config_path,
        "assistant_root": cfg.assistant_root,
        "assistant_context": std::path::Path::new(&cfg.assistant_root).join("context").to_string_lossy(),
        "assistant_evals": std::path::Path::new(&cfg.assistant_root).join("evals").to_string_lossy(),
        "jobs": cfg.jobs_dir,
        "jobs_run": cfg.jobs_run_dir,
        "state": cfg.state_path,
        "audit_log": cfg.audit_log_path,
        "database": cfg.database_path,
        "imessage_database": cfg.db_path,
    })
}

pub(crate) fn format_paths(cfg: &config::Config) -> String {
    let paths = paths_value(cfg);
    let data = paths.as_object().expect("paths are an object");
    let mut output = String::from("push paths\n");
    for key in [
        "config",
        "assistant_root",
        "assistant_context",
        "assistant_evals",
        "jobs",
        "jobs_run",
        "state",
        "audit_log",
        "database",
        "imessage_database",
    ] {
        let value = data[key].as_str().expect("path is a string");
        output.push_str(&format!("{key}: {value}\n"));
    }
    output
}

fn write_success(command: &str, data: Value) -> Result<(), CliError> {
    let envelope = SuccessEnvelope {
        schema_version: SCHEMA_VERSION,
        ok: true,
        command,
        data,
    };
    write_json(&envelope).map_err(|error| CliError::unexpected(error.into()))
}

pub(crate) fn write_error(error: &CliError) {
    let envelope = ErrorEnvelope {
        schema_version: SCHEMA_VERSION,
        ok: false,
        error: ErrorBody {
            category: error.category,
            message: &error.message,
            exit_code: error.exit_code(),
            retryable: error.category.retryable(),
            details: error.details.as_ref(),
        },
    };
    let mut stderr = io::stderr().lock();
    let _ = serde_json::to_writer(&mut stderr, &envelope);
    let _ = writeln!(stderr);
}

fn write_json(value: &impl Serialize) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_stable_and_distinct() {
        assert_eq!(ErrorCategory::InvalidInput.exit_code(), 2);
        assert_eq!(ErrorCategory::Configuration.exit_code(), 3);
        assert_eq!(ErrorCategory::UnavailableDependency.exit_code(), 4);
        assert_eq!(ErrorCategory::TransientTransport.exit_code(), 5);
        assert_eq!(ErrorCategory::Conflict.exit_code(), 6);
        assert_eq!(ErrorCategory::Unexpected.exit_code(), 70);
    }

    #[test]
    fn retryability_is_only_claimed_for_known_categories() {
        assert_eq!(ErrorCategory::InvalidInput.retryable(), Some(false));
        assert_eq!(ErrorCategory::TransientTransport.retryable(), Some(true));
        assert_eq!(ErrorCategory::Unexpected.retryable(), None);
    }
}
