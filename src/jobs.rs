//! Validated Markdown runbooks and the durable manual-run runtime.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions, TryLockError};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use chrono::{Datelike, LocalResult, TimeZone, Timelike, Utc};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::task::JoinSet;
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

#[derive(Debug, Deserialize, Clone)]
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
    pub triggers: Vec<Trigger>,
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
        triggers: metadata.triggers,
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

#[derive(Clone)]
struct CronSpec {
    fields: [Vec<bool>; 5],
    day_of_month_wildcard: bool,
    day_of_week_wildcard: bool,
    timezone: chrono_tz::Tz,
}

impl Trigger {
    pub fn next_after_ms(&self, after_ms: i64) -> Result<i64> {
        let spec = CronSpec::parse(self)?;
        spec.next_after_ms(after_ms)
            .context("cron schedule has no occurrence within two years")
    }
}

impl CronSpec {
    fn parse(trigger: &Trigger) -> Result<Self> {
        let parts = trigger.schedule.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 5 {
            bail!("cron schedule must contain exactly five fields");
        }
        Ok(Self {
            fields: [
                expand_cron_field(parts[0], 0, 59)?,
                expand_cron_field(parts[1], 0, 23)?,
                expand_cron_field(parts[2], 1, 31)?,
                expand_cron_field(parts[3], 1, 12)?,
                expand_cron_field(parts[4], 0, 7)?,
            ],
            day_of_month_wildcard: parts[2].starts_with('*'),
            day_of_week_wildcard: parts[4].starts_with('*'),
            timezone: trigger.timezone.parse()?,
        })
    }

    fn next_after_ms(&self, after_ms: i64) -> Option<i64> {
        let mut candidate_ms = after_ms.div_euclid(60_000) * 60_000 + 60_000;
        let limit = candidate_ms.saturating_add(2 * 366 * 24 * 60 * 60_000);
        while candidate_ms <= limit {
            let utc = chrono::DateTime::<Utc>::from_timestamp_millis(candidate_ms)?;
            let local = utc.with_timezone(&self.timezone);
            if self.matches(&local) && self.is_first_ambiguous_instant(&local) {
                return Some(candidate_ms);
            }
            candidate_ms = candidate_ms.saturating_add(60_000);
        }
        None
    }

    fn matches(&self, local: &chrono::DateTime<chrono_tz::Tz>) -> bool {
        let minute = self.fields[0][local.minute() as usize];
        let hour = self.fields[1][local.hour() as usize];
        let month = self.fields[3][local.month() as usize];
        let dom = self.fields[2][local.day() as usize];
        let dow_value = local.weekday().num_days_from_sunday() as usize;
        let dow = self.fields[4][dow_value]
            || (dow_value == 0 && self.fields[4].get(7).copied().unwrap_or(false));
        let day_matches = match (self.day_of_month_wildcard, self.day_of_week_wildcard) {
            (true, true) => true,
            (true, false) => dow,
            (false, true) => dom,
            (false, false) => dom || dow,
        };
        minute && hour && month && day_matches
    }

    fn is_first_ambiguous_instant(&self, local: &chrono::DateTime<chrono_tz::Tz>) -> bool {
        match self.timezone.from_local_datetime(&local.naive_local()) {
            LocalResult::Ambiguous(first, second) => {
                local.timestamp_millis() == first.timestamp_millis().min(second.timestamp_millis())
            }
            LocalResult::Single(_) => true,
            LocalResult::None => false,
        }
    }
}

fn expand_cron_field(field: &str, minimum: u32, maximum: u32) -> Result<Vec<bool>> {
    validate_cron_field(field, minimum, maximum)?;
    let mut values = vec![false; maximum as usize + 1];
    for part in field.split(',') {
        let (range, step) = if let Some((range, step)) = part.split_once('/') {
            (range, step.parse::<usize>().context("parse cron step")?)
        } else {
            (part, 1)
        };
        let (start, end) = if range == "*" {
            (minimum, maximum)
        } else if let Some((start, end)) = range.split_once('-') {
            (start.parse()?, end.parse()?)
        } else {
            let value = range.parse()?;
            (value, value)
        };
        for value in (start..=end).step_by(step) {
            values[value as usize] = true;
        }
    }
    Ok(values)
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
        job: Box<Job>,
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
    pub trigger_kind: String,
    pub trigger_id: Option<String>,
    pub scheduled_at_ms: Option<i64>,
    pub delivery_state: String,
    pub delivery_attempts: i64,
    pub delivery_error: Option<String>,
    pub delivery_channel: Option<String>,
    pub delivery_target: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueuedRun {
    pub id: String,
    pub job_name: String,
    pub snapshot_hash: String,
    pub trigger_id: String,
}

#[derive(Debug)]
pub struct DeliveryRun {
    pub id: String,
    pub job_name: String,
    pub state: String,
    pub result: Option<String>,
    pub error: Option<String>,
    pub channel: String,
    pub target: String,
    pub attempts: i64,
    pub last_attempt_ms: Option<i64>,
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
        let queued = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM job_runs WHERE job_name = ?1 AND state = 'queued')",
            [&job.name],
            |row| row.get::<_, bool>(0),
        )?;
        if queued {
            insert_run(
                &tx,
                &run_id,
                &job,
                now,
                "skipped_overlap",
                Some("a scheduled run is already queued"),
            )?;
            tx.commit()?;
            return Ok(StartOutcome::Skipped { run_id });
        }
        tx.execute(
            "UPDATE job_runs
             SET state = 'failed', finished_at_ms = ?2,
                 error = 'previous executor exited before completion',
                 delivery_state = CASE WHEN owner_kind = 'gateway_scheduler'
                     THEN 'pending' ELSE delivery_state END
             WHERE job_name = ?1 AND state = 'running'",
            params![job.name, now],
        )?;
        insert_run(&tx, &run_id, &job, now, "running", None)?;
        tx.commit()?;
        Ok(StartOutcome::Claimed {
            run_id,
            lock,
            job: Box::new(job),
        })
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
            "SELECT id, job_name, state, backend, queued_at_ms, result, error,
                    trigger_kind, trigger_id, scheduled_at_ms, delivery_state,
                    delivery_attempts, delivery_error, delivery_channel, delivery_target
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
                    trigger_kind: row.get(7)?,
                    trigger_id: row.get(8)?,
                    scheduled_at_ms: row.get(9)?,
                    delivery_state: row.get(10)?,
                    delivery_attempts: row.get(11)?,
                    delivery_error: row.get(12)?,
                    delivery_channel: row.get(13)?,
                    delivery_target: row.get(14)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn enqueue_scheduled(
        &mut self,
        job: &Job,
        trigger: &Trigger,
        scheduled_at_ms: i64,
        queued_at_ms: i64,
        delivery_channel: &str,
        delivery_target: &str,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM job_runs WHERE job_name = ?1 AND state IN ('queued','running'))",
            [&job.name],
            |row| row.get::<_, bool>(0),
        )?;
        let state = if active { "skipped_overlap" } else { "queued" };
        let error = active.then_some("another run of this job is active");
        tx.execute(
            "INSERT INTO job_runs (
                id, job_name, snapshot_hash, trigger_kind, trigger_id, owner_kind,
                scheduled_at_ms, queued_at_ms, finished_at_ms, backend,
                permission_profile, timeout_ms, workdir, state, error,
                delivery_state, delivery_channel, delivery_target
             ) VALUES (?1, ?2, ?3, 'cron', ?4, 'gateway_scheduler', ?5, ?6,
                CASE WHEN ?7 = 'skipped_overlap' THEN ?6 ELSE NULL END,
                ?8, ?9, ?10, ?11, ?7, ?12,
                CASE WHEN ?7 = 'skipped_overlap' THEN 'pending' ELSE 'not_requested' END,
                ?13, ?14)
             ON CONFLICT DO NOTHING",
            params![
                id,
                job.name,
                job.snapshot_hash,
                trigger.id,
                scheduled_at_ms,
                queued_at_ms,
                state,
                job.backend.as_str(),
                job.permission.name,
                duration_ms(job.timeout),
                job.workdir.to_string_lossy(),
                error,
                delivery_channel,
                delivery_target,
            ],
        )?;
        let existing = tx.query_row(
            "SELECT id FROM job_runs WHERE job_name = ?1 AND trigger_id = ?2 AND scheduled_at_ms = ?3",
            params![job.name, trigger.id, scheduled_at_ms],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(existing)
    }

    pub fn queued_runs(&self, limit: usize) -> Result<Vec<QueuedRun>> {
        let mut statement = self.conn.prepare(
            "SELECT id, job_name, snapshot_hash, trigger_id
             FROM job_runs WHERE state = 'queued'
             ORDER BY scheduled_at_ms, queued_at_ms LIMIT ?1",
        )?;
        let rows = statement
            .query_map([limit as i64], |row| {
                Ok(QueuedRun {
                    id: row.get(0)?,
                    job_name: row.get(1)?,
                    snapshot_hash: row.get(2)?,
                    trigger_id: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn claim_scheduled(
        &mut self,
        cfg: &Config,
        queued: &QueuedRun,
        now: i64,
    ) -> Result<Option<(String, Job, JobLock)>> {
        let Some(lock) = JobLock::try_acquire(&cfg.jobs_run_dir, &queued.job_name)? else {
            self.conn.execute(
                "UPDATE job_runs SET state = 'skipped_overlap', finished_at_ms = ?2,
                    error = 'another local executor holds the job lock', delivery_state = 'pending'
                 WHERE id = ?1 AND state = 'queued'",
                params![queued.id, now],
            )?;
            return Ok(None);
        };
        let job = match Catalog::load_named(cfg, &queued.job_name) {
            Ok(job)
                if job.snapshot_hash == queued.snapshot_hash
                    && job
                        .triggers
                        .iter()
                        .any(|trigger| trigger.enabled && trigger.id == queued.trigger_id) =>
            {
                job
            }
            Ok(_) | Err(_) => {
                self.conn.execute(
                    "UPDATE job_runs SET state = 'cancelled', finished_at_ms = ?2,
                        error = 'installed job or trigger changed before execution',
                        delivery_state = 'pending'
                     WHERE id = ?1 AND state = 'queued'",
                    params![queued.id, now],
                )?;
                return Ok(None);
            }
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE job_runs SET state = 'failed', finished_at_ms = ?2,
                error = 'previous executor exited before completion',
                delivery_state = CASE WHEN owner_kind = 'gateway_scheduler'
                    THEN 'pending' ELSE delivery_state END
             WHERE job_name = ?1 AND state = 'running'",
            params![queued.job_name, now],
        )?;
        let changed = tx.execute(
            "UPDATE job_runs SET state = 'running', started_at_ms = ?2
             WHERE id = ?1 AND state = 'queued'",
            params![queued.id, now],
        )?;
        tx.commit()?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some((queued.id.clone(), job, lock)))
    }

    pub fn finish_scheduled(
        &mut self,
        id: &str,
        state: &str,
        result: Option<&str>,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if !matches!(state, "succeeded" | "failed" | "timed_out") {
            bail!("invalid terminal scheduled run state {state:?}");
        }
        let changed = self.conn.execute(
            "UPDATE job_runs SET state = ?2, finished_at_ms = ?3, result = ?4,
                error = ?5, delivery_state = 'pending'
             WHERE id = ?1 AND state = 'running'",
            params![
                id,
                state,
                now,
                result.map(bound_result),
                error.map(bound_result)
            ],
        )?;
        if changed != 1 {
            bail!("running scheduled job {id:?} does not exist");
        }
        Ok(())
    }

    pub fn recover_stale_runs(&mut self, cfg: &Config, now: i64) -> Result<()> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT job_name FROM job_runs WHERE state = 'running' ORDER BY job_name",
        )?;
        let names = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for name in names {
            let Some(_lock) = JobLock::try_acquire(&cfg.jobs_run_dir, &name)? else {
                continue;
            };
            self.conn.execute(
                "UPDATE job_runs SET state = 'failed', finished_at_ms = ?2,
                    error = 'executor exited before completion',
                    delivery_state = CASE WHEN owner_kind = 'gateway_scheduler'
                        THEN 'pending' ELSE delivery_state END
                 WHERE job_name = ?1 AND state = 'running'",
                params![name, now],
            )?;
        }
        Ok(())
    }

    pub fn due_deliveries(&self, now: i64) -> Result<Vec<DeliveryRun>> {
        let mut statement = self.conn.prepare(
            "SELECT id, job_name, state, result, error, delivery_channel,
                    delivery_target, delivery_attempts, delivery_last_attempt_ms
             FROM job_runs WHERE delivery_state = 'pending'
             ORDER BY finished_at_ms, id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(DeliveryRun {
                    id: row.get(0)?,
                    job_name: row.get(1)?,
                    state: row.get(2)?,
                    result: row.get(3)?,
                    error: row.get(4)?,
                    channel: row.get(5)?,
                    target: row.get(6)?,
                    attempts: row.get(7)?,
                    last_attempt_ms: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                row.attempts < 3
                    && row.last_attempt_ms.is_none_or(|last| {
                        now.saturating_sub(last) >= delivery_backoff_ms(row.attempts)
                    })
            })
            .collect())
    }

    pub fn record_delivery(
        &mut self,
        id: &str,
        delivered: bool,
        error: Option<&str>,
        now: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE job_runs SET delivery_attempts = delivery_attempts + 1,
                delivery_last_attempt_ms = ?2, delivery_error = ?3,
                delivery_state = CASE WHEN ?4 THEN 'delivered'
                    WHEN delivery_attempts + 1 >= 3 THEN 'failed' ELSE 'pending' END
             WHERE id = ?1 AND delivery_state = 'pending'",
            params![id, now, error.map(bound_result), delivered],
        )?;
        Ok(())
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
            duration_ms(job.timeout),
            job.workdir.to_string_lossy(),
            error,
        ],
    )?;
    Ok(())
}

fn duration_ms(duration: Duration) -> i64 {
    duration.as_millis().min(i64::MAX as u128) as i64
}

fn delivery_backoff_ms(attempts: i64) -> i64 {
    match attempts {
        0 => 0,
        1 => 1_000,
        _ => 5_000,
    }
}

#[derive(Clone)]
struct NextOccurrence {
    schedule: String,
    timezone: String,
    snapshot_hash: String,
    at_ms: i64,
}

pub struct Scheduler {
    cfg: Config,
    delivery_channel: String,
    delivery_target: String,
    next: HashMap<(String, String), NextOccurrence>,
    workers: JoinSet<Result<()>>,
    scheduling_enabled: bool,
}

impl Scheduler {
    pub fn new(cfg: Config, delivery_channel: String, delivery_target: String) -> Self {
        Self {
            cfg,
            delivery_channel,
            delivery_target,
            next: HashMap::new(),
            workers: JoinSet::new(),
            scheduling_enabled: true,
        }
    }

    pub fn delivery_only(cfg: Config) -> Self {
        Self {
            cfg,
            delivery_channel: String::new(),
            delivery_target: String::new(),
            next: HashMap::new(),
            workers: JoinSet::new(),
            scheduling_enabled: false,
        }
    }

    pub async fn tick<F, Fut>(&mut self, now: i64, deliver: F) -> Result<()>
    where
        F: Fn(String, String, String) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        while let Some(result) = self.workers.try_join_next() {
            result.context("scheduled worker task failed")??;
        }
        let mut ledger = Ledger::open(&self.cfg.database_path)?;
        ledger.recover_stale_runs(&self.cfg, now)?;

        let catalog = Catalog::load(&self.cfg)?;
        let mut seen = HashSet::new();
        for job in catalog.jobs.values().filter(|_| self.scheduling_enabled) {
            for trigger in job.triggers.iter().filter(|trigger| trigger.enabled) {
                let key = (job.name.clone(), trigger.id.clone());
                seen.insert(key.clone());
                let changed = self.next.get(&key).is_none_or(|existing| {
                    existing.schedule != trigger.schedule
                        || existing.timezone != trigger.timezone
                        || existing.snapshot_hash != job.snapshot_hash
                });
                if changed {
                    self.next.insert(
                        key,
                        NextOccurrence {
                            schedule: trigger.schedule.clone(),
                            timezone: trigger.timezone.clone(),
                            snapshot_hash: job.snapshot_hash.clone(),
                            at_ms: trigger.next_after_ms(now)?,
                        },
                    );
                    continue;
                }
                let due = self
                    .next
                    .get(&key)
                    .map(|next| next.at_ms <= now)
                    .unwrap_or(false);
                if due {
                    let scheduled_at = self.next[&key].at_ms;
                    ledger.enqueue_scheduled(
                        job,
                        trigger,
                        scheduled_at,
                        now,
                        &self.delivery_channel,
                        &self.delivery_target,
                    )?;
                    if let Some(next) = self.next.get_mut(&key) {
                        next.at_ms = trigger.next_after_ms(now)?;
                    }
                }
            }
        }
        self.next.retain(|key, _| seen.contains(key));

        let available = self.cfg.jobs_max_workers.saturating_sub(self.workers.len());
        for queued in ledger.queued_runs(available)? {
            let cfg = self.cfg.clone();
            self.workers
                .spawn(async move { run_scheduled(cfg, queued).await });
        }

        for row in ledger.due_deliveries(now)? {
            let text = format_delivery(&row);
            match deliver(row.channel.clone(), row.target.clone(), text).await {
                Ok(()) => ledger.record_delivery(&row.id, true, None, now)?,
                Err(error) => {
                    ledger.record_delivery(&row.id, false, Some(&error.to_string()), now)?
                }
            }
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        while let Some(result) = self.workers.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::error!("scheduled worker failed during shutdown: {error:#}")
                }
                Err(error) => {
                    tracing::error!("scheduled worker task failed during shutdown: {error}")
                }
            }
        }
    }
}

async fn run_scheduled(cfg: Config, queued: QueuedRun) -> Result<()> {
    let mut ledger = Ledger::open(&cfg.database_path)?;
    let Some((run_id, job, _lock)) = ledger.claim_scheduled(&cfg, &queued, now_ms())? else {
        return Ok(());
    };
    match execute(&cfg, &job).await {
        Ok(reply) => ledger.finish_scheduled(&run_id, "succeeded", Some(&reply), None, now_ms()),
        Err(ExecutionError::Timeout) => ledger.finish_scheduled(
            &run_id,
            "timed_out",
            None,
            Some("backend run timed out"),
            now_ms(),
        ),
        Err(ExecutionError::Failed(error)) => {
            ledger.finish_scheduled(&run_id, "failed", None, Some(&error), now_ms())
        }
    }
}

fn format_delivery(row: &DeliveryRun) -> String {
    let detail = row
        .result
        .as_deref()
        .or(row.error.as_deref())
        .unwrap_or("No result details were recorded.");
    format!("Job `{}` {}.\n\n{}", row.job_name, row.state, detail)
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
    let triggers = if job.triggers.is_empty() {
        "none".to_string()
    } else {
        job.triggers
            .iter()
            .map(|trigger| {
                format!(
                    "{} cron {:?} {} enabled={}",
                    trigger.id, trigger.schedule, trigger.timezone, trigger.enabled
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "name: {}\npath: {}\nbackend: {}\npermission_profile: {}\ntimeout: {}\nworkdir: {}\nsnapshot: {}\ntriggers:\n{}\n\n{}\n",
        job.name,
        job.path.display(),
        job.backend.as_str(),
        job.permission.name,
        humantime::format_duration(job.timeout),
        job.workdir.display(),
        job.snapshot_hash,
        triggers,
        job.body
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{sh_arg, temp_dir, temp_path, FakeCli};
    use std::sync::Arc;

    #[cfg(unix)]
    #[test]
    fn manual_claim_process_helper() {
        let Ok(jobs_dir) = std::env::var("PUSH_TEST_CLAIM_JOBS_DIR") else {
            return;
        };
        let database = PathBuf::from(std::env::var("PUSH_TEST_CLAIM_DATABASE").unwrap());
        let run_dir = PathBuf::from(std::env::var("PUSH_TEST_CLAIM_RUN_DIR").unwrap());
        let ready = PathBuf::from(std::env::var("PUSH_TEST_CLAIM_READY").unwrap());
        let cfg = cfg(Path::new(&jobs_dir), &database, &run_dir);
        let job = Catalog::load_named(&cfg, "cli-live").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed { lock: _lock, .. } = ledger.start_manual(&cfg, &job).unwrap()
        else {
            panic!("helper manual run should claim");
        };
        std::fs::write(ready, "ready").unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

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

    fn scheduled_job(workdir: &Path, enabled: bool) -> String {
        format!(
            "+++\nversion = 1\npermission_profile = \"restricted\"\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n\n[[triggers]]\nid = \"every-minute\"\nkind = \"cron\"\nschedule = \"* * * * *\"\ntimezone = \"UTC\"\nenabled = {enabled}\n+++\n\nRun once.\n",
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
    fn cron_skips_nonexistent_dst_time_and_uses_first_ambiguous_instant() {
        let trigger = |schedule: &str| Trigger {
            id: "dst".to_string(),
            kind: "cron".to_string(),
            schedule: schedule.to_string(),
            timezone: "Europe/London".to_string(),
            enabled: true,
        };
        let utc = |year, month, day, hour, minute| {
            Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
                .single()
                .unwrap()
                .timestamp_millis()
        };

        let spring = trigger("30 1 * * *");
        assert_eq!(
            spring.next_after_ms(utc(2026, 3, 29, 0, 0)).unwrap(),
            utc(2026, 3, 30, 0, 30)
        );

        let autumn = trigger("30 1 * * *");
        let first = autumn.next_after_ms(utc(2026, 10, 24, 23, 59)).unwrap();
        assert_eq!(first, utc(2026, 10, 25, 0, 30));
        assert_eq!(
            autumn.next_after_ms(first).unwrap(),
            utc(2026, 10, 26, 1, 30)
        );
    }

    #[tokio::test]
    async fn scheduler_skips_missed_ticks_and_retries_stored_output_without_rerunning() {
        let jobs_dir = temp_dir("scheduler-jobs");
        let workdir = temp_dir("scheduler-work");
        let database = temp_path("scheduler-db");
        let run_dir = temp_dir("scheduler-run");
        let args_path = temp_path("scheduler-args");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' run >> {}
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'scheduled output' > "$out"
printf '%s\n' '{{"type":"thread.started","thread_id":"scheduled"}}'
"#,
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("codex", &script);
        write_job(&jobs_dir, "scheduled", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();
        let mut scheduler = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());
        let start = Utc
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis();

        scheduler
            .tick(start, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        scheduler
            .tick(start + 3 * 60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        scheduler.shutdown().await;
        write_job(&jobs_dir, "scheduled", &scheduled_job(&workdir, false));

        scheduler
            .tick(start + 3 * 60_000, |_, _, _| async {
                bail!("temporary delivery failure")
            })
            .await
            .unwrap();
        drop(scheduler);
        let mut scheduler = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());
        scheduler
            .tick(start + 3 * 60_000 + 1_000, |_, _, _| async {
                bail!("temporary delivery failure")
            })
            .await
            .unwrap();
        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = delivered.clone();
        scheduler
            .tick(start + 3 * 60_000 + 6_000, move |channel, target, text| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push((channel, target, text));
                    Ok(())
                }
            })
            .await
            .unwrap();

        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("scheduled"))
            .unwrap();
        assert_eq!(rows.len(), 1, "missed minutes must not be caught up");
        assert_eq!(rows[0].state, "succeeded");
        assert_eq!(rows[0].scheduled_at_ms, Some(start + 60_000));
        assert_eq!(rows[0].delivery_state, "delivered");
        assert_eq!(rows[0].delivery_attempts, 3);
        assert_eq!(
            std::fs::read_to_string(args_path).unwrap().lines().count(),
            1
        );
        assert_eq!(delivered.lock().unwrap().len(), 1);
        assert!(delivered.lock().unwrap()[0].2.contains("scheduled output"));
    }

    #[tokio::test]
    async fn queued_run_resumes_after_restart_and_exhausted_delivery_stays_failed() {
        let jobs_dir = temp_dir("queued-restart-jobs");
        let workdir = temp_dir("queued-restart-work");
        let database = temp_path("queued-restart-db");
        let run_dir = temp_dir("queued-restart-run");
        let args_path = temp_path("queued-restart-args");
        let script = format!(
            r#"#!/bin/sh
printf '%s\n' run >> {}
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' 'restart output' > "$out"
printf '%s\n' '{{"type":"thread.started","thread_id":"restart"}}'
"#,
            sh_arg(&args_path)
        );
        let cli = FakeCli::new("codex", &script);
        write_job(&jobs_dir, "restart", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();
        let job = Catalog::load_named(&cfg, "restart").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let first_id = ledger
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let duplicate_id = ledger
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_001, "telegram", "7")
            .unwrap();
        assert_eq!(first_id, duplicate_id);
        assert_eq!(ledger.runs(Some("restart")).unwrap().len(), 1);
        drop(ledger);

        let mut restarted = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());
        restarted
            .tick(60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        restarted.shutdown().await;
        for now in [60_000, 61_000, 66_000] {
            restarted
                .tick(now, |_, _, _| async {
                    bail!("delivery remains unavailable")
                })
                .await
                .unwrap();
        }

        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("restart"))
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "succeeded");
        assert_eq!(rows[0].delivery_state, "failed");
        assert_eq!(rows[0].delivery_attempts, 3);
        assert_eq!(
            std::fs::read_to_string(args_path).unwrap().lines().count(),
            1
        );
    }

    #[tokio::test]
    async fn pending_delivery_replays_without_a_current_primary_destination() {
        let jobs_dir = temp_dir("delivery-only-jobs");
        let workdir = temp_dir("delivery-only-work");
        let database = temp_path("delivery-only-db");
        let run_dir = temp_dir("delivery-only-run");
        let script = r#"#!/bin/sh
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' stored > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"delivery-only"}'
"#;
        let cli = FakeCli::new("codex", script);
        write_job(&jobs_dir, "delivery-only", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();
        let job = Catalog::load_named(&cfg, "delivery-only").unwrap();
        Ledger::open(&cfg.database_path)
            .unwrap()
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let mut scheduler = Scheduler::delivery_only(cfg.clone());
        scheduler
            .tick(60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        scheduler.shutdown().await;

        let delivered = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = delivered.clone();
        scheduler
            .tick(61_000, move |channel, target, text| {
                let captured = captured.clone();
                async move {
                    captured.lock().unwrap().push((channel, target, text));
                    Ok(())
                }
            })
            .await
            .unwrap();

        let row = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("delivery-only"))
            .unwrap()
            .remove(0);
        assert_eq!(row.delivery_state, "delivered");
        assert_eq!(delivered.lock().unwrap()[0].0, "telegram");
        assert_eq!(delivered.lock().unwrap()[0].1, "7");
    }

    #[tokio::test]
    async fn scheduled_timeout_is_persisted_before_delivery() {
        let jobs_dir = temp_dir("scheduled-timeout-jobs");
        let workdir = temp_dir("scheduled-timeout-work");
        let database = temp_path("scheduled-timeout-db");
        let run_dir = temp_dir("scheduled-timeout-run");
        let slow = FakeCli::new("codex", "#!/bin/sh\nsleep 2\n");
        write_job(
            &jobs_dir,
            "timeout",
            &scheduled_job(&workdir, true).replace("timeout = \"5s\"", "timeout = \"10ms\""),
        );
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = slow.bin();
        let job = Catalog::load_named(&cfg, "timeout").unwrap();
        Ledger::open(&cfg.database_path)
            .unwrap()
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let mut scheduler = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());

        scheduler
            .tick(60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        scheduler.shutdown().await;

        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("timeout"))
            .unwrap();
        assert_eq!(rows[0].state, "timed_out");
        assert_eq!(rows[0].delivery_state, "pending");
        assert!(rows[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out")));
    }

    #[tokio::test]
    async fn scheduled_execution_failure_is_distinct_from_delivery_failure() {
        let jobs_dir = temp_dir("scheduled-failure-jobs");
        let workdir = temp_dir("scheduled-failure-work");
        let database = temp_path("scheduled-failure-db");
        let run_dir = temp_dir("scheduled-failure-run");
        let failed = FakeCli::new("codex", "#!/bin/sh\nprintf '%s\n' boom >&2\nexit 1\n");
        write_job(&jobs_dir, "failure", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = failed.bin();
        let job = Catalog::load_named(&cfg, "failure").unwrap();
        Ledger::open(&cfg.database_path)
            .unwrap()
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let mut scheduler = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());

        scheduler
            .tick(60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        scheduler.shutdown().await;

        let row = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("failure"))
            .unwrap()
            .remove(0);
        assert_eq!(row.state, "failed");
        assert_eq!(row.delivery_state, "pending");
        assert_eq!(row.delivery_attempts, 0);
        assert!(row
            .error
            .as_deref()
            .is_some_and(|error| error.contains("boom")));
    }

    #[tokio::test]
    async fn scheduled_workers_obey_the_configured_limit() {
        let jobs_dir = temp_dir("worker-limit-jobs");
        let workdir = temp_dir("worker-limit-work");
        let database = temp_path("worker-limit-db");
        let run_dir = temp_dir("worker-limit-run");
        let script = r#"#!/bin/sh
sleep 0.1
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' done > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"limited"}'
"#;
        let cli = FakeCli::new("codex", script);
        write_job(&jobs_dir, "first", &scheduled_job(&workdir, true));
        write_job(&jobs_dir, "second", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();
        cfg.jobs_max_workers = 1;
        let catalog = Catalog::load(&cfg).unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        for job in catalog.jobs.values() {
            ledger
                .enqueue_scheduled(job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
                .unwrap();
        }
        let mut scheduler = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());

        scheduler
            .tick(60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(scheduler.workers.len(), 1);
        for _ in 0..100 {
            if Ledger::open(&cfg.database_path)
                .unwrap()
                .queued_runs(10)
                .unwrap()
                .len()
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            Ledger::open(&cfg.database_path)
                .unwrap()
                .queued_runs(10)
                .unwrap()
                .len(),
            1
        );
        scheduler.shutdown().await;
        scheduler
            .tick(61_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(scheduler.workers.len(), 1);
        scheduler.shutdown().await;

        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(None)
            .unwrap();
        assert_eq!(
            rows.iter().filter(|row| row.state == "succeeded").count(),
            2
        );
    }

    #[test]
    fn live_scheduled_claim_survives_restart_and_stale_claim_becomes_deliverable() {
        let jobs_dir = temp_dir("scheduled-stale-jobs");
        let workdir = temp_dir("scheduled-stale-work");
        let database = temp_path("scheduled-stale-db");
        let run_dir = temp_dir("scheduled-stale-run");
        write_job(&jobs_dir, "stale", &scheduled_job(&workdir, true));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let job = Catalog::load_named(&cfg, "stale").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let id = ledger
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let queued = ledger.queued_runs(1).unwrap().remove(0);
        let Some((_, _, lock)) = ledger.claim_scheduled(&cfg, &queued, 60_000).unwrap() else {
            panic!("scheduled run should claim");
        };

        let mut restarted = Ledger::open(&cfg.database_path).unwrap();
        restarted.recover_stale_runs(&cfg, 61_000).unwrap();
        assert_eq!(restarted.state(&id), "running");
        drop(lock);
        restarted.recover_stale_runs(&cfg, 62_000).unwrap();

        let row = restarted.runs(Some("stale")).unwrap().remove(0);
        assert_eq!(row.state, "failed");
        assert_eq!(row.delivery_state, "pending");
        assert!(row
            .error
            .as_deref()
            .is_some_and(|error| error.contains("exited before completion")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restarted_scheduler_respects_live_cli_process_then_recovers_after_crash() {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let jobs_dir = temp_dir("scheduler-cli-restart-jobs");
        let workdir = temp_dir("scheduler-cli-restart-work");
        let database = temp_path("scheduler-cli-restart-db");
        let run_dir = temp_dir("scheduler-cli-restart-run");
        let ready = temp_path("scheduler-cli-restart-ready");
        let script = r#"#!/bin/sh
out=''
prev=''
for arg in "$@"; do
  if [ "$prev" = '-o' ]; then out="$arg"; fi
  prev="$arg"
done
printf '%s\n' recovered > "$out"
printf '%s\n' '{"type":"thread.started","thread_id":"after-crash"}'
"#;
        let cli = FakeCli::new("codex", script);
        write_job(&jobs_dir, "cli-live", &scheduled_job(&workdir, true));
        let mut cfg = cfg(&jobs_dir, &database, &run_dir);
        cfg.codex_bin = cli.bin();
        let start = 1_800_000_000_000i64;
        let mut before_restart = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());
        before_restart
            .tick(start, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        drop(before_restart);

        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "jobs::tests::manual_claim_process_helper",
                "--nocapture",
            ])
            .env("PUSH_TEST_CLAIM_JOBS_DIR", &jobs_dir)
            .env("PUSH_TEST_CLAIM_DATABASE", &database)
            .env("PUSH_TEST_CLAIM_RUN_DIR", &run_dir)
            .env("PUSH_TEST_CLAIM_READY", &ready)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready.exists(), "manual claim helper did not become ready");
        assert!(child.try_wait().unwrap().is_none());

        let mut restarted = Scheduler::new(cfg.clone(), "telegram".into(), "7".into());
        restarted
            .tick(start + 60_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        restarted
            .tick(start + 120_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        let live_rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("cli-live"))
            .unwrap();
        assert!(live_rows.iter().any(|row| row.state == "running"));
        assert!(live_rows.iter().any(|row| row.state == "skipped_overlap"));

        child.kill().unwrap();
        child.wait().unwrap();
        restarted
            .tick(start + 180_000, |_, _, _| async { Ok(()) })
            .await
            .unwrap();
        restarted.shutdown().await;

        let rows = Ledger::open(&cfg.database_path)
            .unwrap()
            .runs(Some("cli-live"))
            .unwrap();
        assert!(rows
            .iter()
            .any(|row| { row.trigger_kind == "manual" && row.state == "failed" }));
        assert_eq!(
            rows.iter()
                .filter(|row| row.trigger_kind == "cron" && row.state == "succeeded")
                .count(),
            1
        );
        let _ = std::fs::remove_file(ready);
    }

    #[test]
    fn live_claim_is_not_recovered_and_overlap_is_recorded_for_schedule() {
        let jobs_dir = temp_dir("scheduled-overlap-jobs");
        let workdir = temp_dir("scheduled-overlap-work");
        let database = temp_path("scheduled-overlap-db");
        let run_dir = temp_dir("scheduled-overlap-run");
        write_job(&jobs_dir, "overlap", &scheduled_job(&workdir, true));
        let cfg = cfg(&jobs_dir, &database, &run_dir);
        let job = Catalog::load_named(&cfg, "overlap").unwrap();
        let mut ledger = Ledger::open(&cfg.database_path).unwrap();
        let StartOutcome::Claimed { run_id, lock, .. } = ledger.start_manual(&cfg, &job).unwrap()
        else {
            panic!("manual run should hold the lock");
        };
        ledger.recover_stale_runs(&cfg, 10).unwrap();
        assert_eq!(ledger.state(&run_id), "running");

        ledger
            .enqueue_scheduled(&job, &job.triggers[0], 60_000, 60_000, "telegram", "7")
            .unwrap();
        let rows = ledger.runs(Some("overlap")).unwrap();
        assert!(rows.iter().any(|row| row.state == "skipped_overlap"));
        drop(lock);
        ledger.recover_stale_runs(&cfg, 70_000).unwrap();
        assert_eq!(ledger.state(&run_id), "failed");
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
        lock._file.unlock().unwrap();
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
