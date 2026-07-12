//! Validated Markdown runbooks and the durable manual-run runtime.

use std::collections::{BTreeMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::agent::{Request, RunError, Runner};
use crate::config::{AgentBackend, Config, PermissionProfile};
use crate::{claude, codex, history::History, soul};

const MAX_STORED_RESULT_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frontmatter {
    version: u32,
    permission_profile: String,
    timeout: String,
    workdir: String,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    triggers: Vec<Trigger>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trigger {
    pub id: String,
    pub kind: String,
    pub schedule: String,
    pub timezone: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Job {
    pub name: String,
    pub path: PathBuf,
    pub body: String,
    pub permission: PermissionProfile,
    pub timeout: Duration,
    pub workdir: PathBuf,
    pub backend: AgentBackend,
    pub snapshot_hash: String,
}

#[derive(Debug, Clone)]
pub struct JobError {
    pub name: String,
    pub path: PathBuf,
    pub message: String,
}

pub struct Catalog {
    pub jobs: BTreeMap<String, Job>,
    pub errors: Vec<JobError>,
}

impl Catalog {
    pub fn load(cfg: &Config) -> Result<Self> {
        let dir = Path::new(&cfg.jobs_dir);
        if !dir.exists() {
            return Ok(Self {
                jobs: BTreeMap::new(),
                errors: Vec::new(),
            });
        }
        let mut entries = std::fs::read_dir(dir)
            .with_context(|| format!("read jobs directory {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut jobs = BTreeMap::new();
        let mut errors = Vec::new();
        let mut canonical_paths = HashSet::new();
        for entry in entries {
            let path = entry.path();
            let display_name = entry.file_name().to_string_lossy().to_string();
            let file_type = match entry.file_type() {
                Ok(value) => value,
                Err(error) => {
                    errors.push(job_error(&display_name, path, error));
                    continue;
                }
            };
            if file_type.is_symlink() || !file_type.is_file() {
                errors.push(JobError {
                    name: display_name,
                    path,
                    message: "jobs must be regular Markdown files; symlinks and subdirectories are rejected".to_string(),
                });
                continue;
            }
            let name = match job_name(&path) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(job_error(&display_name, path, error));
                    continue;
                }
            };
            let canonical = match std::fs::canonicalize(&path) {
                Ok(value) => value,
                Err(error) => {
                    errors.push(job_error(&name, path, error));
                    continue;
                }
            };
            if !canonical_paths.insert(canonical) {
                errors.push(JobError {
                    name,
                    path,
                    message: "duplicate canonical job path".to_string(),
                });
                continue;
            }
            match load_file(cfg, &name, &path) {
                Ok(job) => {
                    jobs.insert(name, job);
                }
                Err(error) => errors.push(job_error(&name, path, error)),
            }
        }
        Ok(Self { jobs, errors })
    }

    pub fn load_named(cfg: &Config, name: &str) -> Result<Job> {
        validate_slug(name)?;
        let path = Path::new(&cfg.jobs_dir).join(format!("{name}.md"));
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("job {name:?} is not installed at {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("job {name:?} must be a regular file, not a symlink or directory");
        }
        load_file(cfg, name, &path)
    }
}

fn load_file(cfg: &Config, name: &str, path: &Path) -> Result<Job> {
    let expected = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect job {}", path.display()))?;
    if expected.file_type().is_symlink() || !expected.is_file() {
        bail!("job {name:?} must be a regular file, not a symlink or directory");
    }
    let mut file = File::open(path).with_context(|| format!("open job {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspect opened job {}", path.display()))?;
    if !same_file(&expected, &opened) {
        bail!("job file changed while it was being opened; retry the operation");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("read job {}", path.display()))?;
    let text = std::str::from_utf8(&bytes).context("job must be valid UTF-8")?;
    let (frontmatter, body) = split_runbook(text)?;
    let metadata: Frontmatter = toml::from_str(frontmatter).context("parse TOML frontmatter")?;
    if metadata.version != 1 {
        bail!("unsupported job version {}; expected 1", metadata.version);
    }
    if body.trim().is_empty() {
        bail!("job instruction body cannot be empty");
    }
    validate_triggers(&metadata.triggers)?;
    let permission = cfg.permission_for_job(&metadata.permission_profile)?;
    let timeout = humantime::parse_duration(&metadata.timeout)
        .with_context(|| format!("invalid job timeout {:?}", metadata.timeout))?;
    if timeout.is_zero() || timeout > cfg.jobs_max_timeout_dur()? {
        bail!(
            "job timeout must be positive and no greater than {}",
            cfg.jobs_max_timeout
        );
    }
    let backend = metadata
        .backend
        .as_deref()
        .map(AgentBackend::parse)
        .transpose()?
        .unwrap_or(cfg.jobs_backend()?);
    let workdir = canonical_workdir(&metadata.workdir)?;
    let snapshot_hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(Job {
        name: name.to_string(),
        path: path.to_path_buf(),
        body: body.to_string(),
        permission,
        timeout,
        workdir,
        backend,
        snapshot_hash,
    })
}

#[cfg(unix)]
fn same_file(expected: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    expected.dev() == opened.dev() && expected.ino() == opened.ino()
}

#[cfg(not(unix))]
fn same_file(expected: &std::fs::Metadata, opened: &std::fs::Metadata) -> bool {
    expected.len() == opened.len()
        && expected.modified().ok() == opened.modified().ok()
        && opened.is_file()
}

fn split_runbook(text: &str) -> Result<(&str, &str)> {
    let rest = text
        .strip_prefix("+++\n")
        .or_else(|| text.strip_prefix("+++\r\n"))
        .context("job must start with a +++ frontmatter delimiter")?;
    let marker = rest
        .find("\n+++\n")
        .map(|index| (index, 5))
        .or_else(|| rest.find("\r\n+++\r\n").map(|index| (index, 9)))
        .context("job frontmatter must end with a +++ delimiter on its own line")?;
    Ok((&rest[..marker.0], &rest[marker.0 + marker.1..]))
}

fn job_name(path: &Path) -> Result<String> {
    if path.extension().and_then(|value| value.to_str()) != Some("md") {
        bail!("installed job filename must end in .md");
    }
    let name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .context("job filename must be valid UTF-8")?;
    validate_slug(name)?;
    Ok(name.to_string())
}

fn validate_slug(value: &str) -> Result<()> {
    if value.is_empty()
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("job name must be a lowercase ASCII slug of letters, digits, and hyphens");
    }
    Ok(())
}

pub fn validate_job_name(value: &str) -> Result<()> {
    validate_slug(value)
}

fn validate_triggers(triggers: &[Trigger]) -> Result<()> {
    let mut ids = HashSet::new();
    for trigger in triggers {
        validate_slug(&trigger.id).context("invalid trigger id")?;
        if !ids.insert(&trigger.id) {
            bail!("duplicate trigger id {:?}", trigger.id);
        }
        if trigger.kind != "cron" {
            bail!("invalid trigger kind {:?}; expected cron", trigger.kind);
        }
        let fields = trigger.schedule.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 {
            bail!("cron schedule must contain exactly five fields");
        }
        for (field, minimum, maximum, label) in [
            (fields[0], 0, 59, "minute"),
            (fields[1], 0, 23, "hour"),
            (fields[2], 1, 31, "day of month"),
            (fields[3], 1, 12, "month"),
            (fields[4], 0, 7, "day of week"),
        ] {
            validate_cron_field(field, minimum, maximum)
                .with_context(|| format!("invalid cron {label} field {field:?}"))?;
        }
        trigger
            .timezone
            .parse::<chrono_tz::Tz>()
            .with_context(|| format!("invalid IANA timezone {:?}", trigger.timezone))?;
        let _ = trigger.enabled;
    }
    Ok(())
}

fn validate_cron_field(field: &str, minimum: u32, maximum: u32) -> Result<()> {
    if field.is_empty() {
        bail!("field is empty");
    }
    for part in field.split(',') {
        if part.is_empty() {
            bail!("empty list item");
        }
        let mut step_parts = part.split('/');
        let range = step_parts.next().unwrap_or_default();
        let step = step_parts
            .next()
            .map(|value| parse_cron_number(value, 1, maximum - minimum + 1, "step"))
            .transpose()?;
        if step_parts.next().is_some() {
            bail!("too many step separators");
        }
        if range == "*" {
            continue;
        }
        let mut bounds = range.split('-');
        let start =
            parse_cron_number(bounds.next().unwrap_or_default(), minimum, maximum, "value")?;
        if let Some(end) = bounds.next() {
            let end = parse_cron_number(end, minimum, maximum, "range end")?;
            if bounds.next().is_some() {
                bail!("too many range separators");
            }
            if start > end {
                bail!("range start exceeds range end");
            }
        } else if step.is_some() {
            bail!("a step requires * or a range");
        }
    }
    Ok(())
}

fn parse_cron_number(value: &str, minimum: u32, maximum: u32, label: &str) -> Result<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("{label} must be an integer");
    }
    let parsed = value
        .parse::<u32>()
        .with_context(|| format!("parse {label}"))?;
    if !(minimum..=maximum).contains(&parsed) {
        bail!("{label} must be between {minimum} and {maximum}");
    }
    Ok(parsed)
}

fn canonical_workdir(value: &str) -> Result<PathBuf> {
    let expanded = expand_home(value);
    let path = std::fs::canonicalize(&expanded)
        .with_context(|| format!("canonicalize job workdir {expanded}"))?;
    if !path.is_dir() {
        bail!("job workdir {} is not a directory", path.display());
    }
    Ok(path)
}

fn expand_home(value: &str) -> String {
    if value == "~" || value.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return format!("{}{}", home.to_string_lossy(), &value[1..]);
        }
    }
    value.to_string()
}

fn job_error(name: &str, path: PathBuf, error: impl std::fmt::Display) -> JobError {
    JobError {
        name: name.to_string(),
        path,
        message: error.to_string(),
    }
}

pub enum StartOutcome {
    Claimed {
        run_id: String,
        lock: JobLock,
        job: Job,
    },
    Skipped {
        run_id: String,
    },
}

pub struct JobLock {
    _file: File,
}

impl JobLock {
    fn try_acquire(run_dir: &str, job_name: &str) -> Result<Option<Self>> {
        let lock_dir = Path::new(run_dir).join("locks");
        std::fs::create_dir_all(&lock_dir)
            .with_context(|| format!("create job lock directory {}", lock_dir.display()))?;
        restrict_lock_permissions(&lock_dir, true)?;
        let path = lock_dir.join(format!("{job_name}.lock"));
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open job lock {}", path.display()))?;
        restrict_lock_permissions(&path, false)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("lock job file {}", path.display()))
            }
        }
    }
}

#[cfg(unix)]
fn restrict_lock_permissions(path: &Path, directory: bool) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if directory { 0o700 } else { 0o600 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restrict job lock permissions {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_lock_permissions(_path: &Path, _directory: bool) -> Result<()> {
    Ok(())
}

pub struct Ledger {
    conn: Connection,
}

#[derive(Debug)]
pub struct RunRow {
    pub id: String,
    pub job_name: String,
    pub state: String,
    pub backend: String,
    pub queued_at_ms: i64,
    pub result: Option<String>,
    pub error: Option<String>,
}

impl Ledger {
    pub fn open(database_path: &str) -> Result<Self> {
        drop(History::open(database_path)?);
        let conn = Connection::open(database_path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn start_manual(&mut self, cfg: &Config, job: &Job) -> Result<StartOutcome> {
        let now = now_ms();
        let Some(lock) = JobLock::try_acquire(&cfg.jobs_run_dir, &job.name)? else {
            let run_id = Uuid::new_v4().to_string();
            self.insert_skipped(
                &run_id,
                job,
                now,
                "another local executor holds the job lock",
            )?;
            return Ok(StartOutcome::Skipped { run_id });
        };
        let job = Catalog::load_named(cfg, &job.name)
            .context("reread and validate job after acquiring its run lock")?;
        let run_id = Uuid::new_v4().to_string();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE job_runs
             SET state = 'failed', finished_at_ms = ?2,
                 error = 'previous manual executor exited before completion'
             WHERE job_name = ?1 AND state = 'running' AND owner_kind = 'manual_cli'",
            params![job.name, now],
        )?;
        insert_run(&tx, &run_id, &job, now, "running", None)?;
        tx.commit()?;
        Ok(StartOutcome::Claimed { run_id, lock, job })
    }

    fn insert_skipped(&mut self, id: &str, job: &Job, now: i64, reason: &str) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        insert_run(&tx, id, job, now, "skipped_overlap", Some(reason))?;
        tx.commit()?;
        Ok(())
    }

    pub fn finish(
        &mut self,
        id: &str,
        state: &str,
        result: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        if !matches!(state, "succeeded" | "failed" | "timed_out") {
            bail!("invalid terminal manual run state {state:?}");
        }
        let changed = self.conn.execute(
            "UPDATE job_runs SET state = ?2, finished_at_ms = ?3, result = ?4, error = ?5
             WHERE id = ?1 AND state = 'running'",
            params![
                id,
                state,
                now_ms(),
                result.map(bound_result),
                error.map(bound_result)
            ],
        )?;
        if changed != 1 {
            bail!("running job run {id:?} does not exist");
        }
        Ok(())
    }

    pub fn runs(&self, name: Option<&str>) -> Result<Vec<RunRow>> {
        let mut statement = self.conn.prepare(
            "SELECT id, job_name, state, backend, queued_at_ms, result, error
             FROM job_runs WHERE (?1 IS NULL OR job_name = ?1)
             ORDER BY queued_at_ms DESC, id DESC LIMIT 100",
        )?;
        let rows = statement
            .query_map([name], |row| {
                Ok(RunRow {
                    id: row.get(0)?,
                    job_name: row.get(1)?,
                    state: row.get(2)?,
                    backend: row.get(3)?,
                    queued_at_ms: row.get(4)?,
                    result: row.get(5)?,
                    error: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    #[cfg(test)]
    fn state(&self, id: &str) -> String {
        self.conn
            .query_row("SELECT state FROM job_runs WHERE id = ?1", [id], |row| {
                row.get(0)
            })
            .unwrap()
    }
}

fn insert_run(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    job: &Job,
    now: i64,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO job_runs (
            id, job_name, snapshot_hash, trigger_kind, owner_kind,
            queued_at_ms, started_at_ms, finished_at_ms, backend, permission_profile,
            timeout_ms, workdir, state, error
         ) VALUES (?1, ?2, ?3, 'manual', 'manual_cli', ?4,
                   CASE WHEN ?5 = 'running' THEN ?4 ELSE NULL END,
                   CASE WHEN ?5 = 'skipped_overlap' THEN ?4 ELSE NULL END,
                   ?6, ?7, ?8, ?9, ?5, ?10)",
        params![
            id,
            job.name,
            job.snapshot_hash,
            now,
            state,
            job.backend.as_str(),
            job.permission.name,
            job.timeout.as_millis().min(i64::MAX as u128) as i64,
            job.workdir.to_string_lossy(),
            error,
        ],
    )?;
    Ok(())
}

pub async fn run_manual(cfg: &Config, job: Job) -> Result<(String, String)> {
    let mut ledger = Ledger::open(&cfg.database_path)?;
    let outcome = ledger.start_manual(cfg, &job)?;
    let StartOutcome::Claimed { run_id, lock, job } = outcome else {
        let StartOutcome::Skipped { run_id } = outcome else {
            unreachable!()
        };
        return Ok((run_id, "skipped_overlap".to_string()));
    };
    let _lock = lock;

    match execute(cfg, &job).await {
        Ok(reply) => {
            ledger.finish(&run_id, "succeeded", Some(&reply), None)?;
            Ok((run_id, reply))
        }
        Err(ExecutionError::Timeout) => {
            ledger.finish(&run_id, "timed_out", None, Some("backend run timed out"))?;
            bail!(
                "job timed out after {}",
                humantime::format_duration(job.timeout)
            );
        }
        Err(ExecutionError::Failed(error)) => {
            ledger.finish(&run_id, "failed", None, Some(&error))?;
            bail!("job failed: {error}");
        }
    }
}

enum ExecutionError {
    Timeout,
    Failed(String),
}

async fn execute(cfg: &Config, job: &Job) -> std::result::Result<String, ExecutionError> {
    let current_workdir = std::fs::canonicalize(&job.workdir)
        .map_err(|error| ExecutionError::Failed(format!("recheck job workdir: {error}")))?;
    if current_workdir != job.workdir {
        return Err(ExecutionError::Failed(
            "job workdir changed after validation".to_string(),
        ));
    }
    let instructions = soul::load(&cfg.assistant_dir)
        .map_err(|error| ExecutionError::Failed(format!("load SOUL.md: {error}")))?;
    let runner = match job.backend {
        AgentBackend::Claude => Runner::Claude(claude::Runner {
            bin: cfg.claude_bin.clone(),
        }),
        AgentBackend::Codex => Runner::Codex(codex::Runner {
            bin: cfg.codex_bin.clone(),
            model: cfg.codex_model.clone(),
        }),
    };
    let session_id = runner.initial_session_id();
    let workdir = job.workdir.to_string_lossy().to_string();
    let request = Request {
        session_id: &session_id,
        is_new: true,
        work_dir: &workdir,
        instructions: &instructions,
        permission: job.permission.capability,
        prompt: &job.body,
    };
    match runner.run(request, job.timeout).await {
        Ok(output) => Ok(output.reply),
        Err(RunError::Timeout) => Err(ExecutionError::Timeout),
        Err(RunError::Failed(error) | RunError::SessionMissing(error)) => {
            Err(ExecutionError::Failed(format!("backend: {error}")))
        }
    }
}

fn bound_result(value: &str) -> String {
    if value.len() <= MAX_STORED_RESULT_BYTES {
        return value.to_string();
    }
    const SUFFIX: &str = "\n[truncated by push]";
    let mut boundary = MAX_STORED_RESULT_BYTES.saturating_sub(SUFFIX.len());
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}{SUFFIX}", &value[..boundary])
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

pub fn format_job(job: &Job) -> String {
    format!(
        "name: {}\npath: {}\nbackend: {}\npermission_profile: {}\ntimeout: {}\nworkdir: {}\nsnapshot: {}\n\n{}\n",
        job.name,
        job.path.display(),
        job.backend.as_str(),
        job.permission.name,
        humantime::format_duration(job.timeout),
        job.workdir.display(),
        job.snapshot_hash,
        job.body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{sh_arg, temp_dir, temp_path, FakeCli};

    fn cfg(jobs_dir: &Path, database: &Path, run_dir: &Path) -> Config {
        let mut cfg = crate::gateway::tests::test_config_for_jobs(
            temp_path("jobs-state").to_str().unwrap(),
            temp_dir("jobs-sessions").to_str().unwrap(),
            temp_dir("jobs-assistant").to_str().unwrap(),
        );
        cfg.jobs_dir = jobs_dir.to_string_lossy().to_string();
        cfg.database_path = database.to_string_lossy().to_string();
        cfg.jobs_run_dir = run_dir.to_string_lossy().to_string();
        cfg.job_permission_profiles = vec!["restricted".to_string(), "workspace".to_string()];
        cfg
    }

    fn write_job(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(format!("{name}.md")), body).unwrap();
    }

    fn valid_job(workdir: &Path) -> String {
        format!(
            "+++\nversion = 1\npermission_profile = \"restricted\"\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n+++\n\nInspect this directory.\n",
            workdir.to_string_lossy()
        )
    }

    #[test]
    fn loads_valid_jobs_and_isolates_invalid_files() {
        let jobs_dir = temp_dir("jobs-load");
        let workdir = temp_dir("jobs-work");
        let database = temp_path("jobs-load-db");
        let run_dir = temp_dir("jobs-run");
        write_job(&jobs_dir, "valid-job", &valid_job(&workdir));
        write_job(&jobs_dir, "Bad_Name", "not frontmatter");
        let cfg = cfg(&jobs_dir, &database, &run_dir);

        let catalog = Catalog::load(&cfg).unwrap();

        assert!(catalog.jobs.contains_key("valid-job"));
        assert_eq!(catalog.errors.len(), 1);
        assert!(catalog.errors[0].message.contains("lowercase ASCII slug"));
    }

    #[test]
    fn validation_enforces_permission_timeout_backend_and_workdir() {
        let jobs_dir = temp_dir("jobs-validation");
        let workdir = temp_dir("jobs-validation-work");
        let database = temp_path("jobs-validation-db");
        let run_dir = temp_dir("jobs-validation-run");
        write_job(
            &jobs_dir,
            "too-powerful",
            &format!(
                "+++\nversion = 1\npermission_profile = \"full-access\"\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"other\"\n+++\nbody",
                workdir.to_string_lossy()
            ),
        );
        write_job(
            &jobs_dir,
            "too-slow",
            &format!(
                "+++\nversion = 1\npermission_profile = \"restricted\"\ntimeout = \"31m\"\nworkdir = {:?}\n+++\nbody",
                workdir.to_string_lossy()
            ),
        );
        write_job(
            &jobs_dir,
            "bad-backend",
            &format!(
                "+++\nversion = 1\npermission_profile = \"restricted\"\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"other\"\n+++\nbody",
                workdir.to_string_lossy()
            ),
        );
        write_job(
            &jobs_dir,
            "bad-workdir",
            "+++\nversion = 1\npermission_profile = \"restricted\"\ntimeout = \"5s\"\nworkdir = \"/definitely/missing/push-job\"\n+++\nbody",
        );
        let cfg = cfg(&jobs_dir, &database, &run_dir);

        let catalog = Catalog::load(&cfg).unwrap();

        assert!(catalog.jobs.is_empty());
        let messages = catalog
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>();
        assert!(messages
            .iter()
            .any(|message| message.contains("not included")));
        assert!(messages
            .iter()
            .any(|message| message.contains("no greater than")));
        assert!(messages
            .iter()
            .any(|message| message.contains("invalid agent")));
        assert!(messages
            .iter()
            .any(|message| message.contains("canonicalize job workdir")));
    }

    #[test]
    fn cron_validation_rejects_malformed_fields_and_ranges() {
        for schedule in [
            "nonsense * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 8",
            "10-5 * * * *",
            "*/0 * * * *",
            "1//2 * * * *",
            "1/2 * * * *",
            "1,,2 * * * *",
        ] {
            let trigger = Trigger {
                id: "test".to_string(),
                kind: "cron".to_string(),
                schedule: schedule.to_string(),
                timezone: "Europe/London".to_string(),
                enabled: true,
            };
            assert!(
                validate_triggers(&[trigger]).is_err(),
                "schedule should be invalid: {schedule}"
            );
        }

        let valid = Trigger {
            id: "weekday".to_string(),
            kind: "cron".to_string(),
            schedule: "0,30 8-18/2 * * 1-5".to_string(),
            timezone: "Europe/London".to_string(),
            enabled: true,
        };
        validate_triggers(&[valid]).unwrap();
    }

    #[test]
    fn winning_claim_rereads_and_records_the_installed_snapshot() {
        let jobs_dir = temp_dir("jobs-snapshot");
        let workdir = temp_dir("jobs-snapshot-work");
        let database = temp_path("jobs-snapshot-db");
        let run_dir = temp_dir("jobs-snapshot-run");
        write_job(&jobs_dir, "snapshot", &valid_job(&workdir));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let stale = Catalog::load_named(&cfg, "snapshot").unwrap();
        let updated = valid_job(&workdir).replace("Inspect this directory.", "Use the new body.");
        write_job(&jobs_dir, "snapshot", &updated);

        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed {
            run_id,
            job,
            lock: _,
        } = ledger.start_manual(&cfg, &stale).unwrap()
        else {
            panic!("run should claim");
        };

        assert_ne!(job.snapshot_hash, stale.snapshot_hash);
        assert_eq!(job.body, "\nUse the new body.\n");
        let recorded: String = ledger
            .conn
            .query_row(
                "SELECT snapshot_hash FROM job_runs WHERE id = ?1",
                [&run_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(recorded, job.snapshot_hash);
    }

    #[test]
    fn preserves_the_instruction_body_verbatim() {
        let jobs_dir = temp_dir("jobs-verbatim");
        let workdir = temp_dir("jobs-verbatim-work");
        let database = temp_path("jobs-verbatim-db");
        let run_dir = temp_dir("jobs-verbatim-run");
        let expected = "\n    indented code\n\nTrailing space:  \n";
        let runbook = valid_job(&workdir).replace("\nInspect this directory.\n", expected);
        write_job(&jobs_dir, "verbatim", &runbook);
        let cfg = cfg(&jobs_dir, &database, &run_dir);

        let job = Catalog::load_named(&cfg, "verbatim").unwrap();

        assert_eq!(job.body, expected);
    }

    #[test]
    fn lock_and_ledger_prevent_overlap_and_recover_stale_claim() {
        let jobs_dir = temp_dir("jobs-ledger");
        let workdir = temp_dir("jobs-ledger-work");
        let database = temp_path("jobs-ledger-db");
        let run_dir = temp_dir("jobs-ledger-run");
        write_job(&jobs_dir, "daily-check", &valid_job(&workdir));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let job = Catalog::load_named(&cfg, "daily-check").unwrap();
        let mut first = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed {
            run_id: first_id,
            lock,
            ..
        } = first.start_manual(&cfg, &job).unwrap()
        else {
            panic!("first run should claim");
        };
        let mut second = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Skipped { run_id: skipped } = second.start_manual(&cfg, &job).unwrap()
        else {
            panic!("overlap should skip");
        };
        assert_eq!(second.state(&skipped), "skipped_overlap");
        assert_eq!(first.state(&first_id), "running");
        drop(lock);

        let StartOutcome::Claimed { run_id: next, .. } = second.start_manual(&cfg, &job).unwrap()
        else {
            panic!("released stale claim should recover");
        };
        assert_eq!(second.state(&first_id), "failed");
        assert_eq!(second.state(&next), "running");
    }

    #[test]
    fn run_rows_and_results_survive_reopen() {
        let jobs_dir = temp_dir("jobs-persist");
        let workdir = temp_dir("jobs-persist-work");
        let database = temp_path("jobs-persist-db");
        let run_dir = temp_dir("jobs-persist-run");
        write_job(&jobs_dir, "persist", &valid_job(&workdir));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let job = Catalog::load_named(&cfg, "persist").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed { run_id, lock, .. } = ledger.start_manual(&cfg, &job).unwrap()
        else {
            panic!("run should claim");
        };
        ledger
            .finish(&run_id, "failed", None, Some("boom"))
            .unwrap();
        drop(lock);
        drop(ledger);

        let reopened = Ledger::open(&cfg.database_path).unwrap();
        let rows = reopened.runs(Some("persist")).unwrap();
        assert_eq!(rows[0].state, "failed");
        assert_eq!(rows[0].error.as_deref(), Some("boom"));
    }

    #[test]
    fn stored_results_are_bounded() {
        let jobs_dir = temp_dir("jobs-result-bound");
        let workdir = temp_dir("jobs-result-bound-work");
        let database = temp_path("jobs-result-bound-db");
        let run_dir = temp_dir("jobs-result-bound-run");
        write_job(&jobs_dir, "bounded", &valid_job(&workdir));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let job = Catalog::load_named(&cfg, "bounded").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed { run_id, lock, .. } = ledger.start_manual(&cfg, &job).unwrap()
        else {
            panic!("run should claim");
        };
        ledger
            .finish(
                &run_id,
                "succeeded",
                Some(&"x".repeat(MAX_STORED_RESULT_BYTES * 2)),
                None,
            )
            .unwrap();
        drop(lock);

        let result = ledger.runs(Some("bounded")).unwrap()[0]
            .result
            .clone()
            .unwrap();
        assert!(result.len() <= MAX_STORED_RESULT_BYTES);
        assert!(result.ends_with("[truncated by push]"));
    }

    #[tokio::test]
    async fn manual_codex_runs_use_fresh_sessions_and_persist_results() {
        let jobs_dir = temp_dir("jobs-execute");
        let workdir = temp_dir("jobs-execute-work");
        let database = temp_path("jobs-execute-db");
        let run_dir = temp_dir("jobs-execute-run");
        let args_path = temp_path("jobs-execute-args");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' "$@" >> {}
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'manual result' > "$out"
printf '%s\n' '{{"type":"thread.started","thread_id":"fresh-thread"}}'
"#,
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("codex", &script);
        write_job(&jobs_dir, "execute", &valid_job(&workdir));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();

        let first = run_manual(&cfg, Catalog::load_named(&cfg, "execute").unwrap())
            .await
            .unwrap();
        let second = run_manual(&cfg, Catalog::load_named(&cfg, "execute").unwrap())
            .await
            .unwrap();

        assert_eq!(first.1, "manual result");
        assert_eq!(second.1, "manual result");
        let args = std::fs::read_to_string(args_path).unwrap();
        assert_eq!(args.lines().filter(|line| *line == "exec").count(), 2);
        assert!(!args.lines().any(|line| line == "resume"));
        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("execute"))
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.state == "succeeded"));
        assert!(rows
            .iter()
            .all(|row| row.result.as_deref() == Some("manual result")));
    }

    #[tokio::test]
    async fn timeout_is_terminal_and_does_not_block_a_later_run() {
        let jobs_dir = temp_dir("jobs-timeout");
        let workdir = temp_dir("jobs-timeout-work");
        let database = temp_path("jobs-timeout-db");
        let run_dir = temp_dir("jobs-timeout-run");
        let slow = FakeCli::new("codex", "#!/bin/sh\nsleep 2\n");
        write_job(
            &jobs_dir,
            "timeout",
            &valid_job(&workdir).replace("timeout = \"5s\"", "timeout = \"10ms\""),
        );
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = slow.bin();
        let job = Catalog::load_named(&cfg, "timeout").unwrap();

        assert!(run_manual(&cfg, job).await.is_err());
        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("timeout"))
            .unwrap();
        assert_eq!(rows[0].state, "timed_out");

        let args_path = temp_path("jobs-timeout-retry-args");
        let script = format!(
            r#"#!/bin/sh
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'recovered' > "$out"
printf '%s\n' '{{"type":"thread.started","thread_id":"retry-thread"}}'
printf '%s\n' ok > {}
"#,
            sh_arg(&args_path)
        );
        let success = FakeCli::new("codex", &script);
        cfg.codex_bin = success.bin();
        write_job(&jobs_dir, "timeout", &valid_job(&workdir));
        let output = run_manual(&cfg, Catalog::load_named(&cfg, "timeout").unwrap())
            .await
            .unwrap();
        assert_eq!(output.1, "recovered");
        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("timeout"))
            .unwrap();
        assert_eq!(rows[0].state, "succeeded");
        assert_eq!(rows[1].state, "timed_out");
    }

    #[tokio::test]
    async fn backend_override_selects_claude_with_a_fresh_session() {
        let jobs_dir = temp_dir("jobs-claude");
        let workdir = temp_dir("jobs-claude-work");
        let database = temp_path("jobs-claude-db");
        let run_dir = temp_dir("jobs-claude-run");
        let args_path = temp_path("jobs-claude-args");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{{\"result\":\"claude result\",\"session_id\":\"claude-session\"}}'\n",
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("claude", &script);
        let runbook = valid_job(&workdir).replace("backend = \"codex\"", "backend = \"claude\"");
        write_job(&jobs_dir, "claude-job", &runbook);
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.claude_bin = cli.bin();

        let output = run_manual(&cfg, Catalog::load_named(&cfg, "claude-job").unwrap())
            .await
            .unwrap();

        assert_eq!(output.1, "claude result");
        let args = std::fs::read_to_string(args_path).unwrap();
        assert!(args.lines().any(|line| line == "--session-id"));
        assert!(!args.lines().any(|line| line == "--resume"));
        assert!(args.lines().any(|line| line == "Inspect this directory."));
    }
}
