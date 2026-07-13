//! Agent-authored job proposals and exact-revision installation.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::approval::AnswerOrigin;
use crate::config::Config;
use crate::history::{DraftProposal, History};
use crate::jobs;
use crate::util::{restrict_permissions, same_file};

const MAX_DRAFT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub name: String,
    pub path: PathBuf,
    pub snapshot_hash: String,
    pub contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionOutcome {
    Installed(String),
    Rejected(String),
    Invalidated(String),
    Failed(String),
    AlreadyHandled,
}

pub fn prepare(cfg: &Config) -> Result<()> {
    let drafts = prepare_directory("drafts", Path::new(&cfg.drafts_dir))?;
    let jobs = prepare_directory("jobs", Path::new(&cfg.jobs_dir))?;
    let canonical_drafts = std::fs::canonicalize(&drafts)?;
    let canonical_jobs = std::fs::canonicalize(&jobs)?;
    if canonical_drafts.starts_with(&canonical_jobs)
        || canonical_jobs.starts_with(&canonical_drafts)
    {
        bail!("canonical drafts and jobs directories must not overlap");
    }
    Ok(())
}

pub fn origin_directory(cfg: &Config, origin: &AnswerOrigin) -> Result<PathBuf> {
    prepare(cfg)?;
    let identity = format!(
        "{}\0{}\0{}\0{}",
        origin.channel, origin.thread_key, origin.sender_key, origin.chat_key
    );
    let path = Path::new(&cfg.drafts_dir).join(hash(identity.as_bytes()));
    prepare_directory("origin draft", &path)?;
    let root = std::fs::canonicalize(&cfg.drafts_dir)?;
    let canonical = std::fs::canonicalize(&path)?;
    if canonical.parent() != Some(root.as_path()) {
        bail!("origin draft directory escaped configured drafts_dir");
    }
    Ok(path)
}

pub fn candidates(cfg: &Config, directory: &Path) -> Result<Vec<(String, Result<Candidate>)>> {
    prepare(cfg)?;
    let root = std::fs::canonicalize(&cfg.drafts_dir)?;
    let directory = std::fs::canonicalize(directory)?;
    if directory.parent() != Some(root.as_path()) {
        bail!("draft inbox escaped configured drafts_dir");
    }
    let mut candidates = Vec::new();
    for entry in sorted_entries(&directory)? {
        let display = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        candidates.push((display, read_candidate(cfg, &path)));
    }
    Ok(candidates)
}

pub fn proposal(question_id: String, candidate: &Candidate, proposed_by: String) -> DraftProposal {
    DraftProposal {
        question_id,
        name: candidate.name.clone(),
        path: candidate.path.to_string_lossy().to_string(),
        snapshot_hash: candidate.snapshot_hash.clone(),
        contents: candidate.contents.clone(),
        proposed_by,
        approved_by: None,
        status: "pending".to_string(),
    }
}

pub fn decide(
    cfg: &Config,
    history: &mut History,
    question_id: &str,
    approved_by: &str,
    now_ms: i64,
) -> Result<DecisionOutcome> {
    let Some(proposal) = history.draft_proposal(question_id)? else {
        return Ok(DecisionOutcome::AlreadyHandled);
    };
    if proposal.status != "pending" {
        return Ok(DecisionOutcome::AlreadyHandled);
    }
    let Some(answer) = history.draft_answer(question_id)? else {
        return Ok(DecisionOutcome::AlreadyHandled);
    };
    if answer.value == "reject" {
        history.finish_draft_decision(question_id, "rejected", Some(approved_by), None, now_ms)?;
        return Ok(DecisionOutcome::Rejected(proposal.name));
    }
    if answer.value != "approve" {
        bail!("unsupported draft decision {:?}", answer.value);
    }

    let expected_path = PathBuf::from(&proposal.path);
    if !safe_proposal_path(cfg, &proposal, &expected_path)? {
        let message = "stored draft path is outside the configured drafts directory";
        history.finish_draft_decision(
            question_id,
            "invalidated",
            Some(approved_by),
            Some(message),
            now_ms,
        )?;
        return Ok(DecisionOutcome::Invalidated(message.to_string()));
    }
    let current = match read_candidate(cfg, &expected_path) {
        Ok(candidate) => candidate,
        Err(error) => {
            if std::fs::symlink_metadata(&expected_path)
                .is_err_and(|read_error| read_error.kind() == std::io::ErrorKind::NotFound)
                && installed_revision(cfg, &proposal)?
            {
                history.finish_draft_decision(
                    question_id,
                    "installed",
                    Some(approved_by),
                    None,
                    now_ms,
                )?;
                return Ok(DecisionOutcome::Installed(proposal.name));
            }
            let message = format!("draft is no longer valid: {error:#}");
            history.finish_draft_decision(
                question_id,
                "invalidated",
                Some(approved_by),
                Some(&message),
                now_ms,
            )?;
            return Ok(DecisionOutcome::Invalidated(message));
        }
    };
    if current.snapshot_hash != proposal.snapshot_hash {
        let message = "draft changed after it was presented; request approval for the new revision";
        history.finish_draft_decision(
            question_id,
            "invalidated",
            Some(approved_by),
            Some(message),
            now_ms,
        )?;
        return Ok(DecisionOutcome::Invalidated(message.to_string()));
    }
    let stored_hash = hash(proposal.contents.as_bytes());
    if stored_hash != proposal.snapshot_hash {
        let message = "stored draft revision failed its integrity check";
        history.finish_draft_decision(
            question_id,
            "failed",
            Some(approved_by),
            Some(message),
            now_ms,
        )?;
        return Ok(DecisionOutcome::Failed(message.to_string()));
    }
    if let Err(error) = jobs::validate_contents(
        cfg,
        &proposal.name,
        &expected_path,
        proposal.contents.as_bytes(),
    ) {
        let message =
            format!("draft no longer satisfies the configured capability ceiling: {error:#}");
        history.finish_draft_decision(
            question_id,
            "invalidated",
            Some(approved_by),
            Some(&message),
            now_ms,
        )?;
        return Ok(DecisionOutcome::Invalidated(message));
    }

    match install_exact(cfg, &proposal) {
        Ok(()) => {
            remove_if_revision(&expected_path, &proposal.snapshot_hash)?;
            history.finish_draft_decision(
                question_id,
                "installed",
                Some(approved_by),
                None,
                now_ms,
            )?;
            Ok(DecisionOutcome::Installed(proposal.name))
        }
        Err(error) => {
            let message = format!("install draft: {error:#}");
            history.finish_draft_decision(
                question_id,
                "failed",
                Some(approved_by),
                Some(&message),
                now_ms,
            )?;
            Ok(DecisionOutcome::Failed(message))
        }
    }
}

fn read_candidate(cfg: &Config, path: &Path) -> Result<Candidate> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect draft {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("drafts must be regular Markdown files; symlinks and directories are rejected");
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .context("draft filename must be valid UTF-8")?;
    let name = file_name
        .strip_suffix(".md")
        .context("draft filename must end in .md")?;
    jobs::validate_slug(name)?;
    let mut file = File::open(path).with_context(|| format!("open draft {}", path.display()))?;
    let opened = file.metadata()?;
    if !same_file(&metadata, &opened) {
        bail!("draft changed while it was being opened; retry the proposal");
    }
    if opened.len() > MAX_DRAFT_BYTES as u64 {
        bail!("draft exceeds {MAX_DRAFT_BYTES} bytes");
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DRAFT_BYTES {
        bail!("draft exceeds {MAX_DRAFT_BYTES} bytes");
    }
    jobs::validate_contents(cfg, name, path, &bytes)?;
    let contents = String::from_utf8(bytes).context("draft must be valid UTF-8")?;
    Ok(Candidate {
        name: name.to_string(),
        path: path.to_path_buf(),
        snapshot_hash: hash(contents.as_bytes()),
        contents,
    })
}

fn install_exact(cfg: &Config, proposal: &DraftProposal) -> Result<()> {
    prepare(cfg)?;
    let jobs_dir = Path::new(&cfg.jobs_dir);
    let destination = jobs_dir.join(format!("{}.md", proposal.name));
    if destination.exists() {
        let installed = read_regular_bytes(&destination)?;
        if hash(&installed) == proposal.snapshot_hash {
            return Ok(());
        }
        bail!(
            "installed job {:?} already exists with different contents",
            proposal.name
        );
    }
    let temporary = jobs_dir.join(format!(".{}.{}.tmp", proposal.name, Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create install staging file {}", temporary.display()))?;
        restrict_permissions(&temporary, false)?;
        file.write_all(proposal.contents.as_bytes())?;
        file.sync_all()?;
        jobs::validate_contents(
            cfg,
            &proposal.name,
            &temporary,
            proposal.contents.as_bytes(),
        )?;
        std::fs::hard_link(&temporary, &destination)
            .with_context(|| format!("atomically install job at {}", destination.display()))?;
        File::open(jobs_dir)?.sync_all()?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

fn prepare_directory(label: &str, path: &Path) -> Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            bail!(
                "{label} directory {} must not be a symlink or file",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path)
                .with_context(|| format!("create {label} directory {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect {label} directory {}", path.display()))
        }
    }
    restrict_permissions(path, true)?;
    Ok(path.to_path_buf())
}

fn installed_revision(cfg: &Config, proposal: &DraftProposal) -> Result<bool> {
    let destination = Path::new(&cfg.jobs_dir).join(format!("{}.md", proposal.name));
    if !destination.exists() {
        return Ok(false);
    }
    Ok(hash(&read_regular_bytes(&destination)?) == proposal.snapshot_hash)
}

fn remove_if_revision(path: &Path, expected_hash: &str) -> Result<()> {
    remove_if_revision_after_inspection(path, expected_hash, || {})
}

fn remove_if_revision_after_inspection(
    path: &Path,
    expected_hash: &str,
    after_inspection: impl FnOnce(),
) -> Result<()> {
    let expected = std::fs::symlink_metadata(path)?;
    let bytes = read_regular_bytes(path)?;
    if hash(&bytes) != expected_hash {
        return Ok(());
    }
    after_inspection();
    let quarantine = path.with_file_name(format!(".approved-{}.tmp", Uuid::new_v4()));
    std::fs::rename(path, &quarantine)
        .with_context(|| format!("quarantine installed draft {}", path.display()))?;
    let quarantined = std::fs::symlink_metadata(&quarantine)?;
    let unchanged = same_file(&expected, &quarantined)
        && hash(&read_regular_bytes(&quarantine)?) == expected_hash;
    if unchanged {
        std::fs::remove_file(&quarantine)?;
        return Ok(());
    }
    if std::fs::hard_link(&quarantine, path).is_ok() {
        std::fs::remove_file(&quarantine)?;
    }
    Ok(())
}

fn safe_proposal_path(cfg: &Config, proposal: &DraftProposal, path: &Path) -> Result<bool> {
    let expected_name = format!("{}.md", proposal.name);
    if path.file_name().and_then(|value| value.to_str()) != Some(expected_name.as_str()) {
        return Ok(false);
    }
    let Some(parent) = path.parent() else {
        return Ok(false);
    };
    let metadata = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(false);
    }
    let root = std::fs::canonicalize(&cfg.drafts_dir)?;
    let parent = std::fs::canonicalize(parent)?;
    Ok(parent.parent() == Some(root.as_path()))
}

fn read_regular_bytes(path: &Path) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let file = File::open(path)?;
    let opened = file.metadata()?;
    if !same_file(&metadata, &opened) {
        bail!("{} changed while opening", path.display());
    }
    let mut bytes = Vec::new();
    file.take(MAX_DRAFT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DRAFT_BYTES {
        bail!("{} exceeds {MAX_DRAFT_BYTES} bytes", path.display());
    }
    Ok(bytes)
}

fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::approval::{AnswerOrigin, AnswerOutcome, Choice, Question};
    use crate::config::Config;
    use crate::test_support::{temp_dir, temp_path};

    fn cfg() -> (Config, PathBuf) {
        let root = temp_dir("drafts-root");
        let workdir = temp_dir("drafts-workdir");
        (
            Config {
                channel: "telegram".to_string(),
                channels: vec!["telegram".to_string()],
                primary_delivery: None,
                db_path: temp_path("drafts-chat").to_string_lossy().to_string(),
                poll_interval: "1s".to_string(),
                run_timeout: "1s".to_string(),
                self_handles: Vec::new(),
                allow_from: Vec::new(),
                telegram_bot_token: Some("token".to_string()),
                telegram_bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
                telegram_allow_user_ids: vec![7],
                telegram_allow_chat_ids: Vec::new(),
                agent: "codex".to_string(),
                routes: Vec::new(),
                permission_profile: "workspace".to_string(),
                permission_profiles: HashMap::new(),
                assistant_root: root.to_string_lossy().to_string(),
                jobs_dir: root.join("jobs").to_string_lossy().to_string(),
                drafts_dir: root.join("drafts").to_string_lossy().to_string(),
                jobs_agent: None,
                jobs_max_timeout: "30m".to_string(),
                jobs_run_dir: root.join("run").to_string_lossy().to_string(),
                jobs_max_workers: 2,
                claude_bin: "claude".to_string(),
                codex_bin: "codex".to_string(),
                codex_model: None,
                sessions_dir: root.join("sessions").to_string_lossy().to_string(),
                state_path: root.join("state.json").to_string_lossy().to_string(),
                audit_log_path: root.join("audit.jsonl").to_string_lossy().to_string(),
                database_path: root.join("push.db").to_string_lossy().to_string(),
                audit_log_content: false,
                config_path: String::new(),
                assistant_dir: root.to_string_lossy().to_string(),
                reply_marker: String::new(),
            },
            workdir,
        )
    }

    fn runbook(workdir: &Path) -> String {
        format!(
            "+++\nversion = 1\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n+++\n\nInspect safely.\n",
            workdir.to_string_lossy()
        )
    }

    fn origin() -> AnswerOrigin {
        AnswerOrigin {
            channel: "telegram".to_string(),
            thread_key: "telegram:dm:7:topic:9".to_string(),
            sender_key: "7".to_string(),
            chat_key: "7".to_string(),
        }
    }

    fn question() -> Question {
        Question::new(
            origin(),
            "7:9",
            "Install exact draft?",
            vec![
                Choice {
                    label: "Approve".to_string(),
                    value: "approve".to_string(),
                },
                Choice {
                    label: "Reject".to_string(),
                    value: "reject".to_string(),
                },
            ],
            10_000,
        )
        .unwrap()
    }

    fn candidate(cfg: &Config, workdir: &Path, name: &str) -> Candidate {
        let directory = origin_directory(cfg, &origin()).unwrap();
        let path = directory.join(format!("{name}.md"));
        std::fs::write(&path, runbook(workdir)).unwrap();
        read_candidate(cfg, &path).unwrap()
    }

    fn record(history: &mut History, candidate: &Candidate, question: &Question) {
        let proposal = proposal(
            question.id.clone(),
            candidate,
            "channel=telegram thread=telegram:dm:7:topic:9 sender=7 chat=7".to_string(),
        );
        history
            .create_draft_question(question, &proposal, 1_000)
            .unwrap();
    }

    #[test]
    fn approval_survives_restart_and_installs_once_for_exact_channel_identity() {
        let (cfg, workdir) = cfg();
        let candidate = candidate(&cfg, &workdir, "morning-note");
        let question = question();
        let mut history = History::open(&cfg.database_path).unwrap();
        record(&mut history, &candidate, &question);
        drop(history);

        let mut history = History::open(&cfg.database_path).unwrap();
        let mut wrong = origin();
        wrong.thread_key = "telegram:dm:7".to_string();
        assert_eq!(
            history
                .answer_question(&wrong, &format!("{} 1", question.id), 2_000)
                .unwrap(),
            AnswerOutcome::Mismatched(question.id.clone())
        );
        assert!(matches!(
            history
                .answer_question(&origin(), &format!("{} 1", question.id), 2_100)
                .unwrap(),
            AnswerOutcome::Selected(_)
        ));
        assert_eq!(
            decide(&cfg, &mut history, &question.id, "sender=7", 2_200).unwrap(),
            DecisionOutcome::Installed("morning-note".to_string())
        );
        let installed = Path::new(&cfg.jobs_dir).join("morning-note.md");
        assert_eq!(
            std::fs::read_to_string(&installed).unwrap(),
            candidate.contents
        );
        assert!(!candidate.path.exists());
        assert!(matches!(
            history
                .answer_question(&origin(), &format!("{} 1", question.id), 2_300)
                .unwrap(),
            AnswerOutcome::Duplicate(_)
        ));
        assert_eq!(
            decide(&cfg, &mut history, &question.id, "sender=7", 2_400).unwrap(),
            DecisionOutcome::AlreadyHandled
        );
        assert_eq!(std::fs::read_dir(&cfg.jobs_dir).unwrap().count(), 1);
    }

    #[test]
    fn edit_after_presentation_invalidates_approval_and_reject_keeps_draft_inactive() {
        let (cfg, workdir) = cfg();
        let raced = candidate(&cfg, &workdir, "race");
        let race_question = question();
        let mut history = History::open(&cfg.database_path).unwrap();
        record(&mut history, &raced, &race_question);
        std::fs::write(
            &raced.path,
            runbook(&workdir).replace("Inspect safely.", "Inspect after the edit."),
        )
        .unwrap();
        history
            .answer_question(&origin(), &format!("{} 1", race_question.id), 2_000)
            .unwrap();
        assert!(matches!(
            decide(&cfg, &mut history, &race_question.id, "sender=7", 2_100).unwrap(),
            DecisionOutcome::Invalidated(_)
        ));
        assert!(!Path::new(&cfg.jobs_dir).join("race.md").exists());

        let rejected = candidate(&cfg, &workdir, "rejected");
        let reject_question = question();
        record(&mut history, &rejected, &reject_question);
        history
            .answer_question(&origin(), &format!("{} 2", reject_question.id), 3_000)
            .unwrap();
        assert_eq!(
            decide(&cfg, &mut history, &reject_question.id, "sender=7", 3_100,).unwrap(),
            DecisionOutcome::Rejected("rejected".to_string())
        );
        assert!(rejected.path.exists());
        assert!(!Path::new(&cfg.jobs_dir).join("rejected.md").exists());
    }

    #[test]
    fn invalid_escape_symlink_and_permission_escalation_never_become_candidates() {
        let (cfg, workdir) = cfg();
        assert!(cfg
            .validate_job_workdir(Path::new(&cfg.assistant_dir))
            .is_err());
        let parsed = jobs::validate_contents(
            &cfg,
            "push-state",
            Path::new(&cfg.drafts_dir).join("push-state.md").as_path(),
            runbook(Path::new(&cfg.assistant_dir)).as_bytes(),
        );
        assert!(parsed.is_err(), "parsed={parsed:?}");
        let directory = origin_directory(&cfg, &origin()).unwrap();
        std::fs::write(directory.join("../escape.md"), runbook(&workdir)).unwrap();
        std::fs::write(
            directory.join("too-powerful.md"),
            runbook(&workdir).replace(
                "version = 1\n",
                "version = 1\npermission_profile = \"full-access\"\n",
            ),
        )
        .unwrap();
        std::fs::write(
            directory.join("push-state.md"),
            runbook(Path::new(&cfg.assistant_dir)),
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            directory.join("too-powerful.md"),
            directory.join("linked.md"),
        )
        .unwrap();

        let changed = candidates(&cfg, &directory).unwrap();
        assert!(changed.iter().any(|(name, result)| {
            name == "too-powerful.md"
                && format!("{:#}", result.as_ref().unwrap_err()).contains("permission_profile")
        }));
        assert!(changed.iter().any(|(name, result)| {
            name == "push-state.md"
                && result
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("overlaps Push-owned")
        }));
        #[cfg(unix)]
        assert!(changed.iter().any(|(name, result)| {
            name == "linked.md"
                && result
                    .as_ref()
                    .unwrap_err()
                    .to_string()
                    .contains("symlinks")
        }));
        assert!(!changed.iter().any(|(name, _)| name == "escape.md"));
    }

    #[test]
    fn restart_finalizes_an_exact_install_completed_before_database_update() {
        let (cfg, workdir) = cfg();
        let candidate = candidate(&cfg, &workdir, "crash-window");
        let question = question();
        let mut history = History::open(&cfg.database_path).unwrap();
        record(&mut history, &candidate, &question);
        history
            .answer_question(&origin(), &format!("{} 1", question.id), 2_000)
            .unwrap();
        let proposal = history.draft_proposal(&question.id).unwrap().unwrap();
        install_exact(&cfg, &proposal).unwrap();
        std::fs::remove_file(&candidate.path).unwrap();
        drop(history);

        let mut restarted = History::open(&cfg.database_path).unwrap();
        assert_eq!(
            decide(&cfg, &mut restarted, &question.id, "sender=7", 2_100).unwrap(),
            DecisionOutcome::Installed("crash-window".to_string())
        );
        assert_eq!(
            restarted
                .draft_proposal(&question.id)
                .unwrap()
                .unwrap()
                .status,
            "installed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_drafts_directory_cannot_be_a_symlink_to_installed_jobs() {
        let (cfg, _) = cfg();
        std::fs::create_dir_all(&cfg.jobs_dir).unwrap();
        std::os::unix::fs::symlink(&cfg.jobs_dir, &cfg.drafts_dir).unwrap();

        assert!(prepare(&cfg)
            .unwrap_err()
            .to_string()
            .contains("must not be a symlink"));
    }

    #[test]
    fn cleanup_does_not_delete_a_replacement_revision() {
        let directory = temp_dir("draft-cleanup-race");
        let path = directory.join("race.md");
        let approved = b"approved revision";
        std::fs::write(&path, approved).unwrap();
        let replacement_path = path.clone();

        remove_if_revision_after_inspection(&path, &hash(approved), move || {
            std::fs::remove_file(&replacement_path).unwrap();
            std::fs::write(&replacement_path, "new unapproved revision").unwrap();
        })
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new unapproved revision"
        );
    }
}
