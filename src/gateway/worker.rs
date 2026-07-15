//! Per-thread worker: runs one message through an agent backend, records the
//! outcome in canonical history, and delivers the reply.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::agent::{Request, RunError};
use crate::approval::{Choice, DeliveryStatus as ApprovalDeliveryStatus, Question};
use crate::history::{DeliveryStatus, OutboundMessage, OutboundOrigin};
use crate::rehydration::{self, RehydrationPrompt};
use crate::soul;
use crate::util::now_ms;
use crate::voice::MAX_AUDIO_BYTES;

use super::{audit, complete_row, reply_to, Ctx, Job};

const TYPING_REFRESH: Duration = Duration::from_secs(4);
pub(super) const DELIVERY_ATTEMPTS: usize = 3;
pub(super) const SESSION_SETUP_FAILURE: &str =
    "Push could not prepare this conversation. Check the local logs, then resend.";
pub(super) const DRAFT_SETUP_FAILURE: &str =
    "Push could not prepare its draft workspace. Check the local logs, then resend.";

/// Processes one thread's jobs strictly in order, exiting when the queue closes.
pub(super) async fn run(ctx: Ctx, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        handle(&ctx, job).await;
    }
}

pub(super) async fn handle(ctx: &Ctx, mut job: Job) {
    let existing_outbound = match ctx.history.lock().unwrap().outbound_for(job.inbound_id) {
        Ok(outbound) => outbound,
        Err(error) => {
            history_error(ctx, &job, "read outbound", error);
            return;
        }
    };
    // Push exposes the draft boundary. The selected agent's own configuration
    // decides whether it may write there.
    let draft_directory = match crate::drafts::origin_directory(&ctx.cfg, &job.approval_origin) {
        Ok(directory) => Some(directory),
        Err(error) => {
            error!("[{}] draft boundary error: {error:#}", job.thread);
            if let Some(outbound) = &existing_outbound {
                report_delivery(
                    ctx,
                    &job,
                    deliver_stored(ctx, &job, outbound).await,
                    &outbound.content,
                    "recovered_outbound",
                    "recover outbound",
                );
            } else {
                complete_setup_failure(ctx, &job, DRAFT_SETUP_FAILURE).await;
            }
            return;
        }
    };
    if let Some(outbound) = existing_outbound {
        if let Some(directory) = &draft_directory {
            if let Err(error) = present_drafts(ctx, &job, directory).await {
                error!(
                    "[{}] recovered draft presentation failed: {error:#}",
                    job.thread
                );
                return;
            }
        }
        report_delivery(
            ctx,
            &job,
            deliver_stored(ctx, &job, &outbound).await,
            &outbound.content,
            "recovered_outbound",
            "recover outbound",
        );
        return;
    }

    if job.voice_attachment.is_some() {
        match prepare_voice(ctx, &job).await {
            Ok(transcript) => job.text = transcript,
            Err(VoicePreparationError::History(error)) => {
                history_error(ctx, &job, "store voice transcript", error);
                return;
            }
            Err(VoicePreparationError::User {
                event,
                reply,
                detail,
            }) => {
                warn!("[{}] {detail}", job.thread);
                audit(
                    ctx,
                    ctx.audit
                        .failed(event, job.row_id, &job.thread, Some(job.backend), detail),
                );
                let delivery = record_and_deliver(ctx, &job, OutboundOrigin::Gateway, reply).await;
                report_delivery(ctx, &job, delivery, reply, event, "deliver voice fallback");
                return;
            }
        }
    }

    if let Some(reply) = command(ctx, &job) {
        let delivery = record_and_deliver(ctx, &job, OutboundOrigin::Gateway, &reply).await;
        if delivery.is_ok() {
            info!(
                "[{}] command reply sent via {}",
                job.thread,
                ctx.channel.id()
            );
        }
        report_delivery(
            ctx,
            &job,
            delivery,
            &reply,
            "command",
            "record command reply",
        );
        return;
    }

    let Some(runner) = ctx.runners.get(&job.backend) else {
        error!(
            "[{}] no runner configured for {}",
            job.thread,
            job.backend.as_str()
        );
        audit(
            ctx,
            ctx.audit.failed(
                "backend_run_failed",
                job.row_id,
                &job.thread,
                Some(job.backend),
                "no runner configured",
            ),
        );
        complete_job(ctx, &job, "missing_runner");
        return;
    };

    let session_result = {
        let initial_session_id = runner.initial_session_id();
        ctx.store.lock().unwrap().session_for(
            &job.thread,
            runner.backend().as_str(),
            initial_session_id,
        )
    };
    let (session_id, is_new) = match session_result {
        Ok(v) => v,
        Err(e) => {
            error!("[{}] session error: {e}", job.thread);
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_setup_failed",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    e.to_string(),
                ),
            );
            complete_setup_failure(ctx, &job, SESSION_SETUP_FAILURE).await;
            return;
        }
    };

    let work_dir = match std::fs::canonicalize(&ctx.assistant_dir) {
        Ok(path) => path.to_string_lossy().to_string(),
        Err(error) => {
            error!("[{}] assistant workspace error: {error}", job.thread);
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_setup_failed",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    error.to_string(),
                ),
            );
            complete_setup_failure(ctx, &job, SESSION_SETUP_FAILURE).await;
            return;
        }
    };

    let mut instructions = match soul::load(&work_dir) {
        Ok(instructions) => instructions,
        Err(error) => {
            error!("[{}] assistant identity error: {error}", job.thread);
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_setup_failed",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    error.to_string(),
                ),
            );
            complete_setup_failure(ctx, &job, SESSION_SETUP_FAILURE).await;
            return;
        }
    };
    let draft_write_dir = draft_directory.as_ref().and_then(|path| path.to_str());
    if let Err(error) = ctx.cfg.backend_context_dir() {
        error!("[{}] assistant context error: {error:#}", job.thread);
        audit(
            ctx,
            ctx.audit.failed(
                "backend_setup_failed",
                job.row_id,
                &job.thread,
                Some(job.backend),
                error.to_string(),
            ),
        );
        complete_setup_failure(ctx, &job, SESSION_SETUP_FAILURE).await;
        return;
    }
    if let Some(draft_write_dir) = draft_write_dir {
        instructions.push_str(&format!(
            "\n\nTo propose a recurring job, write one complete validated Markdown runbook to {}/<lowercase-slug>.md. This drafts directory is the only Push-owned path you may write. Your agent configuration decides whether the user-owned context directory is editable. A draft remains inactive until the allowlisted user approves its exact revision. Never write to the installed jobs directory, Push configuration, or Push state.",
            draft_write_dir
        ));
    }

    let run = async {
        let mut session_id = session_id;
        let mut rehydration = if is_new {
            match rehydration_prompt(ctx, &job) {
                Ok(prompt) => Some(prompt),
                Err(error) => {
                    return Err(RunError::Failed(format!(
                        "load canonical history for rehydration: {error}"
                    )));
                }
            }
        } else {
            None
        };

        // Some backends let push choose the session id. Mark those before the
        // run so a post-create failure does not retry the same create call.
        if is_new && runner.mark_started_before_run() {
            let _ = ctx.store.lock().unwrap().mark_started(&job.thread, None);
        }
        let prompt = rehydration
            .as_ref()
            .filter(|prompt| prompt.message_count > 0)
            .map_or(job.text.as_str(), |prompt| prompt.text.as_str());
        audit(
            ctx,
            ctx.audit.backend_started(
                job.row_id,
                &job.thread,
                job.backend,
                is_new,
                rehydration
                    .as_ref()
                    .map_or(0, |prompt| prompt.message_count),
            ),
        );
        info!(
            "[{}] sending message to {} (new_session={is_new}, rehydrated_messages={})",
            job.thread,
            runner.label(),
            rehydration
                .as_ref()
                .map_or(0, |prompt| prompt.message_count)
        );
        let mut result = runner
            .run(
                backend_request(&session_id, is_new, &work_dir, &instructions, prompt),
                ctx.run_timeout,
            )
            .await;
        // If the session id already exists (e.g. left over from a previous run
        // or a different build), resume it instead of trying to create it again.
        if is_new {
            if let Err(RunError::Failed(msg)) = &result {
                if msg.to_lowercase().contains("already in use") {
                    warn!("[{}] session id already existed, resuming", job.thread);
                    result = runner
                        .run(
                            backend_request(
                                &session_id,
                                false,
                                &work_dir,
                                &instructions,
                                &job.text,
                            ),
                            ctx.run_timeout,
                        )
                        .await;
                }
            }
        } else if matches!(&result, Err(RunError::SessionMissing(_))) {
            warn!(
                "[{}] backend session is missing; rotating and rehydrating",
                job.thread
            );
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_session_missing",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    "backend could not resume the stored session",
                ),
            );
            session_id = runner.initial_session_id();
            if let Err(error) = ctx.store.lock().unwrap().rotate(
                &job.thread,
                runner.backend().as_str(),
                session_id.clone(),
            ) {
                return Err(RunError::Failed(format!(
                    "rotate missing backend session: {error}"
                )));
            }
            rehydration = match rehydration_prompt(ctx, &job) {
                Ok(prompt) => Some(prompt),
                Err(error) => {
                    return Err(RunError::Failed(format!(
                        "load canonical history for rehydration: {error}"
                    )));
                }
            };
            if runner.mark_started_before_run() {
                let _ = ctx.store.lock().unwrap().mark_started(&job.thread, None);
            }
            let count = rehydration
                .as_ref()
                .map_or(0, |prompt| prompt.message_count);
            audit(
                ctx,
                ctx.audit
                    .backend_started(job.row_id, &job.thread, job.backend, true, count),
            );
            let prompt = rehydration
                .as_ref()
                .filter(|prompt| prompt.message_count > 0)
                .map_or(job.text.as_str(), |prompt| prompt.text.as_str());
            result = runner
                .run(
                    backend_request(&session_id, true, &work_dir, &instructions, prompt),
                    ctx.run_timeout,
                )
                .await;
        }
        result
    };
    let result = if ctx.channel.supports_typing() {
        let channel = ctx.channel.clone();
        let target = job.target.clone();
        let thread = job.thread.clone();
        run_with_periodic_activity(run, TYPING_REFRESH, move || {
            let channel = channel.clone();
            let target = target.clone();
            let thread = thread.clone();
            async move {
                if let Err(e) = channel.send_typing(&target).await {
                    warn!("[{thread}] Telegram typing update failed: {e}");
                }
            }
        })
        .await
    } else {
        run.await
    };

    match result {
        Ok(out) => {
            info!(
                "[{}] {} completed; reply_chars={}",
                job.thread,
                runner.label(),
                out.reply.chars().count()
            );
            audit(
                ctx,
                ctx.audit
                    .backend_completed(job.row_id, &job.thread, job.backend, &out.reply),
            );
            let outbound = match ctx.history.lock().unwrap().record_outbound(
                job.inbound_id,
                OutboundOrigin::Backend,
                Some(job.backend.as_str()),
                &out.reply,
            ) {
                Ok(outbound) => outbound,
                Err(error) => {
                    history_error(ctx, &job, "record backend reply", error);
                    return;
                }
            };
            if let Err(e) = ctx
                .store
                .lock()
                .unwrap()
                .mark_started(&job.thread, out.session_id.as_deref())
            {
                error!("[{}] session save error: {e}", job.thread);
                audit(
                    ctx,
                    ctx.audit.failed(
                        "backend_session_save_failed",
                        job.row_id,
                        &job.thread,
                        Some(job.backend),
                        e.to_string(),
                    ),
                );
                return;
            }
            // Deliver the reply before presenting drafts so the user reads the
            // answer first, then any job proposal it produced.
            let delivery = deliver_stored(ctx, &job, &outbound).await;
            if let Some(directory) = &draft_directory {
                if let Err(error) = present_drafts(ctx, &job, directory).await {
                    error!("[{}] draft presentation failed: {error:#}", job.thread);
                    audit(
                        ctx,
                        ctx.audit.failed(
                            "draft_presentation_failed",
                            job.row_id,
                            &job.thread,
                            Some(job.backend),
                            error.to_string(),
                        ),
                    );
                    return;
                }
            }
            if delivery.is_ok() {
                info!("[{}] reply sent via {}", job.thread, ctx.channel.id());
            }
            report_delivery(
                ctx,
                &job,
                delivery,
                &out.reply,
                "completed",
                "deliver backend reply",
            );
        }
        Err(RunError::Timeout) => {
            warn!("[{}] {} run timed out", job.thread, runner.label());
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_run_failed",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    format!("{} run timed out", runner.label()),
                ),
            );
            let reply = "That took too long and was stopped. Try again or simplify the request.";
            finish_run_with_gateway_reply(
                ctx,
                &job,
                draft_directory.as_deref(),
                reply,
                ReplyLabels {
                    record: "record timeout reply",
                    present: "present timed-out run drafts",
                    deliver: "deliver timeout reply",
                    completion: "timeout",
                },
            )
            .await;
        }
        Err(RunError::Failed(msg) | RunError::SessionMissing(msg)) => {
            error!("[{}] {} error: {msg}", job.thread, runner.label());
            audit(
                ctx,
                ctx.audit.failed(
                    "backend_run_failed",
                    job.row_id,
                    &job.thread,
                    Some(job.backend),
                    msg.clone(),
                ),
            );
            let reply = format!("⚠️ {}", short(&msg));
            finish_run_with_gateway_reply(
                ctx,
                &job,
                draft_directory.as_deref(),
                &reply,
                ReplyLabels {
                    record: "record failure reply",
                    present: "present failed run drafts",
                    deliver: "deliver failure reply",
                    completion: "backend_failed",
                },
            )
            .await;
        }
    }
}

enum VoicePreparationError {
    User {
        event: &'static str,
        reply: &'static str,
        detail: String,
    },
    History(anyhow::Error),
}

async fn prepare_voice(ctx: &Ctx, job: &Job) -> std::result::Result<String, VoicePreparationError> {
    let attachment = job
        .voice_attachment
        .as_ref()
        .expect("voice preparation requires an attachment");
    if attachment
        .file_size
        .is_some_and(|size| size > MAX_AUDIO_BYTES)
    {
        return Err(VoicePreparationError::User {
            event: "voice_too_large",
            reply: "That voice message is too large. The limit is 20 MB.",
            detail: "voice message exceeds the 20 MB limit".to_string(),
        });
    }
    let Some(voice) = &ctx.voice else {
        return Err(VoicePreparationError::User {
            event: "voice_not_configured",
            reply: "Voice messages are unavailable. Set OPENAI_API_KEY and restart Push, or send text instead.",
            detail: "OPENAI_API_KEY is not configured".to_string(),
        });
    };
    let clip = ctx
        .channel
        .download_voice(attachment)
        .await
        .map_err(|error| VoicePreparationError::User {
            event: "voice_download_failed",
            reply: "I could not download that voice message. Please try again or send text.",
            detail: format!("voice download failed: {error:#}"),
        })?;
    let transcript = voice
        .transcribe(clip)
        .await
        .map_err(|error| VoicePreparationError::User {
            event: "voice_transcription_failed",
            reply: "I could not transcribe that voice message. Please try again or send text.",
            detail: format!("voice transcription failed: {error:#}"),
        })?;
    ctx.history
        .lock()
        .unwrap()
        .replace_inbound_content(job.inbound_id, &transcript)
        .map_err(VoicePreparationError::History)?;
    Ok(transcript)
}

/// Error and completion labels for one gateway-authored reply flow.
struct ReplyLabels {
    record: &'static str,
    present: &'static str,
    deliver: &'static str,
    completion: &'static str,
}

/// Records a gateway-authored reply, presents any pending job drafts, then
/// delivers the reply and completes the row. Shared by the timeout and
/// failure arms of `handle`.
async fn finish_run_with_gateway_reply(
    ctx: &Ctx,
    job: &Job,
    draft_directory: Option<&Path>,
    reply: &str,
    labels: ReplyLabels,
) {
    let outbound = match ctx.history.lock().unwrap().record_outbound(
        job.inbound_id,
        OutboundOrigin::Gateway,
        Some(job.backend.as_str()),
        reply,
    ) {
        Ok(outbound) => outbound,
        Err(error) => return history_error(ctx, job, labels.record, error),
    };
    if let Some(directory) = draft_directory {
        if let Err(error) = present_drafts(ctx, job, directory).await {
            return history_error(ctx, job, labels.present, error);
        }
    }
    report_delivery(
        ctx,
        job,
        deliver_stored(ctx, job, &outbound).await,
        reply,
        labels.completion,
        labels.deliver,
    );
}

/// Audits `reply_sent` and completes the row when the reply reached the
/// channel; reports a canonical-history failure otherwise.
fn report_delivery(
    ctx: &Ctx,
    job: &Job,
    delivery: Result<DeliveryOutcome>,
    reply: &str,
    completion: &str,
    deliver_action: &str,
) {
    match delivery {
        Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
            audit(
                ctx,
                ctx.audit.reply_sent(
                    job.row_id,
                    &job.thread,
                    &job.target,
                    Some(job.backend),
                    reply,
                ),
            );
            complete_job(ctx, job, completion);
        }
        Err(error) => history_error(ctx, job, deliver_action, error),
    }
}

pub(super) async fn present_drafts(ctx: &Ctx, job: &Job, directory: &Path) -> Result<()> {
    for (display_name, result) in crate::drafts::candidates(&ctx.cfg, directory)? {
        let candidate = match result {
            Ok(candidate) => candidate,
            Err(error) => {
                let message = format!(
                    "Job draft `{display_name}` is inactive and cannot be approved: {error:#}"
                );
                if !reply_to(ctx, &job.target, &message).await {
                    warn!("[{}] invalid draft notice delivery failed", job.thread);
                }
                continue;
            }
        };
        let existing = ctx
            .history
            .lock()
            .unwrap()
            .draft_revision(&candidate.path, &candidate.snapshot_hash)?;
        let proposal = if let Some(proposal) = existing {
            proposal
        } else {
            let question = Question::new(
                job.approval_origin.clone(),
                job.target.clone(),
                format!(
                    "Install job draft `{}` at exact revision `{}`?",
                    candidate.name, candidate.snapshot_hash
                ),
                vec![
                    Choice {
                        label: "Approve and install".to_string(),
                        value: "approve".to_string(),
                    },
                    Choice {
                        label: "Reject and leave inactive".to_string(),
                        value: "reject".to_string(),
                    },
                ],
                now_ms() + 86_400_000,
            )?;
            let proposed_by = format!(
                "channel={} thread={} sender={} chat={}",
                job.approval_origin.channel,
                job.approval_origin.thread_key,
                job.approval_origin.sender_key,
                job.approval_origin.chat_key
            );
            let proposal = crate::drafts::proposal(question.id.clone(), &candidate, proposed_by);
            let _ = ctx.history.lock().unwrap().create_draft_question(
                &question,
                &proposal,
                now_ms(),
            )?;
            ctx.history
                .lock()
                .unwrap()
                .draft_revision(&candidate.path, &candidate.snapshot_hash)?
                .context("persisted draft revision disappeared")?
        };
        let Some(question) = ctx
            .history
            .lock()
            .unwrap()
            .pending_draft_question(&proposal.question_id, now_ms())?
        else {
            continue;
        };
        let full = format!(
            "Proposed job `{}`\nRevision: `{}`\n\n{}",
            candidate.name, candidate.snapshot_hash, candidate.contents
        );
        let contents_delivered = reply_to(ctx, &question.target, &full).await;
        let delivered =
            contents_delivered && reply_to(ctx, &question.target, &question.render_text()).await;
        ctx.history.lock().unwrap().mark_question_delivery(
            &proposal.question_id,
            if delivered {
                ApprovalDeliveryStatus::Delivered
            } else {
                ApprovalDeliveryStatus::Failed
            },
        )?;
        if !delivered {
            warn!("[{}] draft proposal delivery failed", job.thread);
            anyhow::bail!("draft proposal delivery failed; retry before expiry");
        }
    }
    Ok(())
}

pub(super) async fn run_with_periodic_activity<O, A, AF>(
    operation: O,
    refresh: Duration,
    mut activity: A,
) -> O::Output
where
    O: Future,
    A: FnMut() -> AF,
    AF: Future<Output = ()>,
{
    tokio::pin!(operation);
    let mut ticker = tokio::time::interval(refresh);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            output = &mut operation => return output,
            _ = ticker.tick() => {
                let activity = activity();
                tokio::pin!(activity);
                tokio::select! {
                    output = &mut operation => return output,
                    _ = &mut activity => {}
                }
            }
        }
    }
}

/// Setup failures are terminal for the current row. Try to tell the user what
/// happened, then complete the row so one bad setup step cannot wedge ack state.
/// The persisted notification is retried in bounded batches until delivery or
/// shutdown.
async fn complete_setup_failure(ctx: &Ctx, job: &Job, reply: &str) {
    #[cfg(test)]
    ctx.setup_failure_replies
        .lock()
        .unwrap()
        .push(reply.to_string());

    match record_and_deliver(ctx, job, OutboundOrigin::Gateway, reply).await {
        Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
            audit(
                ctx,
                ctx.audit.reply_sent(
                    job.row_id,
                    &job.thread,
                    &job.target,
                    Some(job.backend),
                    reply,
                ),
            );
        }
        Err(error) => {
            history_error(ctx, job, "record setup failure reply", error);
            return;
        }
    }
    audit(ctx, ctx.audit.completed(job.row_id, "setup_failed"));
    complete_row(&ctx.store, &ctx.ack, ctx.channel.id(), job.row_id);
}

fn complete_job(ctx: &Ctx, job: &Job, reason: &str) {
    audit(ctx, ctx.audit.completed(job.row_id, reason));
    complete_row(&ctx.store, &ctx.ack, ctx.channel.id(), job.row_id);
}

/// Handles gateway-level slash commands before anything reaches the agent.
fn command(ctx: &Ctx, job: &Job) -> Option<String> {
    match job.text.trim().to_lowercase().as_str() {
        "/clear" | "/new" | "/reset" => match ctx.store.lock().unwrap().rotate(
            &job.thread,
            job.backend.as_str(),
            ctx.runners
                .get(&job.backend)
                .map(|r| r.initial_session_id())
                .unwrap_or_default(),
        ) {
            Ok(()) => Some("Started a fresh conversation.".to_string()),
            Err(_) => Some("Couldn't reset the conversation.".to_string()),
        },
        "/help" => {
            Some("Commands:\n/clear - start a fresh conversation\n/help - this message".to_string())
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryOutcome {
    Delivered,
    AlreadyDelivered,
}

pub(super) async fn record_and_deliver(
    ctx: &Ctx,
    job: &Job,
    origin: OutboundOrigin,
    text: &str,
) -> Result<DeliveryOutcome> {
    let outbound = ctx.history.lock().unwrap().record_outbound(
        job.inbound_id,
        origin,
        Some(job.backend.as_str()),
        text,
    )?;
    deliver_stored(ctx, job, &outbound).await
}

async fn deliver_stored(
    ctx: &Ctx,
    job: &Job,
    outbound: &OutboundMessage,
) -> Result<DeliveryOutcome> {
    if outbound.status == DeliveryStatus::Delivered {
        return Ok(DeliveryOutcome::AlreadyDelivered);
    }
    let mut attempt = 0;
    loop {
        attempt += 1;
        let delivered = reply_to(ctx, &job.target, &outbound.content).await;
        let status = if delivered {
            DeliveryStatus::Delivered
        } else {
            DeliveryStatus::Failed
        };
        ctx.history
            .lock()
            .unwrap()
            .mark_delivery(outbound.id, status)?;
        if delivered {
            deliver_voice_reply(ctx, job, &outbound.content).await;
            return Ok(DeliveryOutcome::Delivered);
        }
        audit(
            ctx,
            ctx.audit.reply_failed(
                job.row_id,
                &job.thread,
                &job.target,
                Some(job.backend),
                "stored outbound delivery attempt failed",
            ),
        );
        if attempt < DELIVERY_ATTEMPTS {
            warn!(
                "delivery attempt {attempt}/{DELIVERY_ATTEMPTS} failed; retrying stored outbound"
            );
            #[cfg(not(test))]
            tokio::time::sleep(Duration::from_millis(250)).await;
        } else {
            warn!("delivery attempts exhausted; stored outbound remains failed and will retry");
            attempt = 0;
            #[cfg(not(test))]
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        #[cfg(test)]
        tokio::task::yield_now().await;
    }
}

async fn deliver_voice_reply(ctx: &Ctx, job: &Job, text: &str) {
    if !job.reply_with_voice {
        return;
    }
    let Some(voice) = &ctx.voice else {
        return;
    };
    let clip = match voice.synthesize(text).await {
        Ok(clip) => clip,
        Err(error) => {
            warn!(
                "[{}] voice reply synthesis failed; text reply was delivered: {error:#}",
                job.thread
            );
            return;
        }
    };
    #[cfg(test)]
    {
        ctx.sent_voice_replies
            .lock()
            .unwrap()
            .push((job.target.clone(), clip.bytes));
    }
    #[cfg(not(test))]
    if let Err(error) = ctx.channel.send_voice(&job.target, &clip).await {
        warn!(
            "[{}] voice reply delivery failed; text reply was delivered: {error:#}",
            job.thread
        );
    }
}

fn history_error(ctx: &Ctx, job: &Job, action: &str, error: anyhow::Error) {
    error!(
        "[{}] canonical history {action} failed: {error}; refusing unrecorded delivery",
        job.thread
    );
    audit(
        ctx,
        ctx.audit.failed(
            "message_history_failed",
            job.row_id,
            &job.thread,
            Some(job.backend),
            format!("{action}: {error}"),
        ),
    );
}

/// Extracts a short, user-facing reason from an error message.
fn short(msg: &str) -> String {
    let s = msg.rsplit(": ").next().unwrap_or(msg).trim();
    if s.is_empty() {
        "couldn't reach the agent".to_string()
    } else {
        s.to_string()
    }
}

fn rehydration_prompt(ctx: &Ctx, job: &Job) -> Result<RehydrationPrompt> {
    let messages = ctx.history.lock().unwrap().recent_messages_before(
        ctx.channel.id(),
        &job.thread,
        job.inbound_id,
        rehydration::MAX_HISTORY_MESSAGES,
    )?;
    Ok(rehydration::compose(&messages, &job.text))
}

fn backend_request<'a>(
    session_id: &'a str,
    is_new: bool,
    work_dir: &'a str,
    instructions: &'a str,
    prompt: &'a str,
) -> Request<'a> {
    Request {
        session_id,
        is_new,
        work_dir,
        instructions,
        prompt,
    }
}
