//! The message loop: poll, filter, and route each message to a per-thread
//! worker task that runs an agent backend and sends the reply.
//!
//! Design: a single loop task owns the queue map and the store. Each thread
//! gets its own task fed by an mpsc channel, which serializes that thread's
//! messages. Workers share only the store, behind a mutex, for tiny bookkeeping
//! writes. Everything else is message passing.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agent::{Request, RunError, Runner};
use crate::audit::{AuditEvent, AuditLog};
use crate::channel::{Channel, RawMessage};
use crate::claude;
use crate::codex;
use crate::config::{AgentBackend, AssistantProfile, Config};
use crate::memory;
use crate::store::Store;

const QUEUE_DEPTH: usize = 32;
#[cfg(not(test))]
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const TYPING_REFRESH: Duration = Duration::from_secs(4);
const SESSION_SETUP_FAILURE: &str =
    "Push could not prepare this conversation. Check the local logs, then resend.";
const SANDBOX_SETUP_FAILURE: &str =
    "Push could not create its session workspace. Check the local logs, then resend.";

struct Job {
    row_id: i64,
    thread: String,
    target: String,
    backend: AgentBackend,
    text: String,
}

/// Shared, cheaply cloneable context handed to each worker task.
#[derive(Clone)]
struct Ctx {
    store: Arc<Mutex<Store>>,
    ack: Arc<Mutex<AckState>>,
    runners: Arc<HashMap<AgentBackend, Runner>>,
    channel: Channel,
    run_timeout: Duration,
    reply_marker: String,
    sessions_dir: String,
    assistant_dir: String,
    assistant: AssistantProfile,
    audit: Arc<AuditLog>,
    #[cfg(test)]
    setup_failure_replies: Arc<Mutex<Vec<String>>>,
    #[cfg(test)]
    sent_replies: Arc<Mutex<Vec<(String, String)>>>,
}

pub struct Gateway {
    channel: Channel,
    store: Arc<Mutex<Store>>,
    ack: Arc<Mutex<AckState>>,
    ctx: Ctx,
    cfg: Config,
    poll_interval: Duration,
    queues: HashMap<String, mpsc::Sender<Job>>,
}

#[derive(Default)]
struct AckState {
    in_flight: BTreeSet<i64>,
    completed: BTreeSet<i64>,
}

impl Gateway {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open(&cfg.state_path)?));
        let ack = Arc::new(Mutex::new(AckState::default()));
        let runners = Arc::new(runners(&cfg));
        let channel = Channel::new(&cfg)?;
        let audit = Arc::new(AuditLog::new(
            cfg.audit_log_path.clone(),
            cfg.audit_log_content,
            channel.id(),
        ));
        let ctx = Ctx {
            store: store.clone(),
            ack: ack.clone(),
            runners,
            channel: channel.clone(),
            run_timeout: cfg.run_timeout_dur()?,
            reply_marker: cfg.reply_marker.clone(),
            sessions_dir: cfg.sessions_dir.clone(),
            assistant_dir: cfg.assistant_dir.clone(),
            assistant: cfg.assistant.clone(),
            audit,
            #[cfg(test)]
            setup_failure_replies: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            sent_replies: Arc::new(Mutex::new(Vec::new())),
        };
        let poll_interval = cfg.poll_interval_dur()?;
        Ok(Self {
            channel,
            store,
            ack,
            ctx,
            cfg,
            poll_interval,
            queues: HashMap::new(),
        })
    }

    /// Runs until SIGINT/SIGTERM, then drains in-flight runs and returns.
    pub async fn run(mut self) -> Result<()> {
        self.skip_backlog().await?;

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        info!(
            "push gateway running, polling every {:?}",
            self.poll_interval
        );

        loop {
            let poll = async {
                ticker.tick().await;
                self.tick(&mut handles).await;
            };
            if wait_for_shutdown_or(shutdown.as_mut(), poll)
                .await
                .is_none()
            {
                info!("signal received, draining in-flight runs");
                break;
            }
        }

        // Dropping the senders lets each worker finish its current job, drain
        // its queue, and exit. Then wait, bounded by a grace period.
        self.queues.clear();
        let drain = async {
            for h in handles {
                let _ = h.await;
            }
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            warn!("shutdown grace expired with runs still in flight");
        }
        info!("shut down cleanly");
        Ok(())
    }

    async fn skip_backlog(&self) -> Result<()> {
        let channel_id = self.channel.id();
        if self.store.lock().unwrap().has_cursor(channel_id) {
            return Ok(());
        }
        let max = self
            .channel
            .latest_cursor()
            .await
            .with_context(|| format!("read initial {channel_id} cursor"))?;
        self.store
            .lock()
            .unwrap()
            .set_cursor(channel_id, max)
            .with_context(|| format!("persist initial {channel_id} cursor"))?;
        info!("starting {channel_id} from cursor {max} (skipping backlog)");
        Ok(())
    }

    async fn tick(&mut self, handles: &mut Vec<JoinHandle<()>>) {
        let since = self.store.lock().unwrap().cursor(self.channel.id());
        let msgs = match self.channel.poll(since).await {
            Ok(messages) => messages,
            Err(e) => {
                error!("poll error: {e}");
                return;
            }
        };

        self.process_messages(msgs, handles).await;
    }

    #[cfg(test)]
    async fn tick_fake(&mut self, msgs: Vec<RawMessage>, handles: &mut Vec<JoinHandle<()>>) {
        self.process_messages(msgs, handles).await;
    }

    async fn process_messages(&mut self, msgs: Vec<RawMessage>, handles: &mut Vec<JoinHandle<()>>) {
        let since = self.store.lock().unwrap().cursor(self.channel.id());
        let mut max_row = 0i64;
        for m in &msgs {
            if m.row_id <= since {
                continue;
            }
            max_row = max_row.max(m.row_id);
            if self.ack.lock().unwrap().is_known(m.row_id) {
                continue;
            }
            if let Some((thread, target)) = self.channel.accept(m) {
                let backend = match self.cfg.agent_for_message(m.channel, &thread) {
                    Ok(v) => v,
                    Err(e) => {
                        error!("[{thread}] route error: {e}");
                        self.audit(self.ctx.audit.failed(
                            "message_route_failed",
                            m.row_id,
                            &thread,
                            None,
                            e.to_string(),
                        ));
                        self.complete_row(m.row_id, "route_error");
                        continue;
                    }
                };
                self.audit(self.ctx.audit.accepted(m, &thread, backend));
                info!(
                    "[{thread}] new message accepted; routing to {}",
                    backend.as_str()
                );
                self.route(
                    m.row_id,
                    thread,
                    target,
                    backend,
                    m.text.trim().to_string(),
                    handles,
                )
                .await;
            } else {
                self.audit(self.ctx.audit.ignored(m, self.channel.reject_reason(m)));
                self.complete_row(m.row_id, "ignored");
            }
        }
        if max_row > 0 && msgs.is_empty() {
            self.complete_row(max_row, "empty_poll");
        }
    }

    async fn route(
        &mut self,
        row_id: i64,
        thread: String,
        target: String,
        backend: AgentBackend,
        text: String,
        handles: &mut Vec<JoinHandle<()>>,
    ) {
        if !self.queues.contains_key(&thread) {
            let (tx, rx) = mpsc::channel::<Job>(QUEUE_DEPTH);
            let ctx = self.ctx.clone();
            handles.push(tokio::spawn(worker(ctx, rx)));
            self.queues.insert(thread.clone(), tx);
        }

        let job = Job {
            row_id,
            thread: thread.clone(),
            target: target.clone(),
            backend,
            text,
        };
        let full = matches!(
            self.queues.get(&thread).unwrap().try_send(job),
            Err(mpsc::error::TrySendError::Full(_))
        );
        if full {
            warn!("[{thread}] queue full, asking sender to resend");
            self.audit(self.ctx.audit.failed(
                "message_queue_failed",
                row_id,
                &thread,
                Some(backend),
                "queue full",
            ));
            if reply_to(
                &self.ctx,
                &target,
                "I'm a bit behind on this thread - resend that in a moment.",
            )
            .await
            {
                self.audit(self.ctx.audit.reply_sent(
                    row_id,
                    &thread,
                    &target,
                    Some(backend),
                    "I'm a bit behind on this thread - resend that in a moment.",
                ));
                self.complete_row(row_id, "queue_full");
            } else {
                self.audit(self.ctx.audit.reply_failed(
                    row_id,
                    &thread,
                    &target,
                    Some(backend),
                    "queue full reply failed",
                ));
                self.complete_row(row_id, "queue_full_reply_failed");
            }
        } else {
            self.ack.lock().unwrap().in_flight.insert(row_id);
        }
    }

    fn complete_row(&self, row_id: i64, reason: &str) {
        self.audit(self.ctx.audit.completed(row_id, reason));
        complete_row(&self.store, &self.ack, self.channel.id(), row_id);
    }

    fn audit(&self, event: AuditEvent) {
        audit(&self.ctx, event);
    }
}

/// Processes one thread's jobs strictly in order, exiting when the queue closes.
async fn worker(ctx: Ctx, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        handle(&ctx, job).await;
    }
}

async fn handle(ctx: &Ctx, job: Job) {
    if let Some(reply) = command(ctx, &job) {
        if reply_to(ctx, &job.target, &reply).await {
            info!(
                "[{}] command reply sent via {}",
                job.thread,
                ctx.channel.id()
            );
            audit(
                ctx,
                ctx.audit.reply_sent(
                    job.row_id,
                    &job.thread,
                    &job.target,
                    Some(job.backend),
                    &reply,
                ),
            );
            complete_job(ctx, &job, "command");
        } else {
            audit(
                ctx,
                ctx.audit.reply_failed(
                    job.row_id,
                    &job.thread,
                    &job.target,
                    Some(job.backend),
                    "command reply failed",
                ),
            );
        }
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

    let work_dir = sandbox(&ctx.sessions_dir, &job.thread);
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        error!("[{}] sandbox error: {e}", job.thread);
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
        complete_setup_failure(ctx, &job, SANDBOX_SETUP_FAILURE).await;
        return;
    }

    let system = memory::load(&ctx.assistant_dir, &ctx.assistant);

    // Some backends let push choose the session id. Mark those before the run
    // so a post-create failure does not retry the same create call.
    if is_new && runner.mark_started_before_run() {
        let _ = ctx.store.lock().unwrap().mark_started(&job.thread, None);
    }

    let req = |is_new| Request {
        session_id: &session_id,
        is_new,
        work_dir: &work_dir,
        system_append: &system,
        prompt: &job.text,
    };

    audit(
        ctx,
        ctx.audit
            .backend_started(job.row_id, &job.thread, job.backend, is_new),
    );
    info!(
        "[{}] sending message to {} (new_session={is_new})",
        job.thread,
        runner.label()
    );
    let run = async {
        let mut result = runner.run(req(is_new), ctx.run_timeout).await;
        // If the session id already exists (e.g. left over from a previous run
        // or a different build), resume it instead of trying to create it again.
        if is_new {
            if let Err(RunError::Failed(msg)) = &result {
                if msg.to_lowercase().contains("already in use") {
                    warn!("[{}] session id already existed, resuming", job.thread);
                    result = runner.run(req(false), ctx.run_timeout).await;
                }
            }
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
            if reply_to(ctx, &job.target, &out.reply).await {
                info!("[{}] reply sent via {}", job.thread, ctx.channel.id());
                audit(
                    ctx,
                    ctx.audit.reply_sent(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        &out.reply,
                    ),
                );
                complete_job(ctx, &job, "completed");
            } else {
                audit(
                    ctx,
                    ctx.audit.reply_failed(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        "backend reply failed",
                    ),
                );
            }
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
            if reply_to(
                ctx,
                &job.target,
                "That took too long and was stopped. Try again or simplify the request.",
            )
            .await
            {
                audit(
                    ctx,
                    ctx.audit.reply_sent(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        "That took too long and was stopped. Try again or simplify the request.",
                    ),
                );
                complete_job(ctx, &job, "timeout");
            } else {
                audit(
                    ctx,
                    ctx.audit.reply_failed(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        "timeout reply failed",
                    ),
                );
            }
        }
        Err(RunError::Failed(msg)) => {
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
            if reply_to(ctx, &job.target, &format!("⚠️ {}", short(&msg))).await {
                audit(
                    ctx,
                    ctx.audit.reply_sent(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        &format!("⚠️ {}", short(&msg)),
                    ),
                );
                complete_job(ctx, &job, "backend_failed");
            } else {
                audit(
                    ctx,
                    ctx.audit.reply_failed(
                        job.row_id,
                        &job.thread,
                        &job.target,
                        Some(job.backend),
                        "failure reply failed",
                    ),
                );
            }
        }
    }
}

async fn run_with_periodic_activity<O, A, AF>(
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
/// If notification delivery also fails, the row is still completed and the
/// failed notification is logged.
async fn complete_setup_failure(ctx: &Ctx, job: &Job, reply: &str) {
    #[cfg(test)]
    ctx.setup_failure_replies
        .lock()
        .unwrap()
        .push(reply.to_string());

    #[cfg(not(test))]
    {
        if reply_to(ctx, &job.target, reply).await {
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
        } else {
            audit(
                ctx,
                ctx.audit.reply_failed(
                    job.row_id,
                    &job.thread,
                    &job.target,
                    Some(job.backend),
                    "setup failure reply failed",
                ),
            );
            warn!(
                "[{}] setup failure reply was not sent; completing row {}",
                job.thread, job.row_id
            );
        }
    }
    audit(ctx, ctx.audit.completed(job.row_id, "setup_failed"));
    complete_setup_failure_row(&ctx.store, &ctx.ack, ctx.channel.id(), job.row_id);
}

fn complete_setup_failure_row(
    store: &Arc<Mutex<Store>>,
    ack: &Arc<Mutex<AckState>>,
    channel: &str,
    row_id: i64,
) {
    complete_row(store, ack, channel, row_id);
}

fn complete_job(ctx: &Ctx, job: &Job, reason: &str) {
    audit(ctx, ctx.audit.completed(job.row_id, reason));
    complete_row(&ctx.store, &ctx.ack, ctx.channel.id(), job.row_id);
}

fn audit(ctx: &Ctx, event: AuditEvent) {
    if let Err(e) = ctx.audit.record(event) {
        error!("audit log error: {e}");
    }
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

async fn reply_to(ctx: &Ctx, target: &str, text: &str) -> bool {
    let chunks = ctx.channel.outbound_chunks(text, &ctx.reply_marker);
    #[cfg(test)]
    {
        ctx.sent_replies.lock().unwrap().extend(
            chunks
                .into_iter()
                .map(|chunk| (target.to_string(), chunk.text)),
        );
        true
    }
    #[cfg(not(test))]
    {
        for chunk in chunks {
            match tokio::time::timeout(SEND_TIMEOUT, ctx.channel.send_chunk(target, &chunk)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    error!("send error to {target}: {e}");
                    return false;
                }
                Err(_) => {
                    error!("send to {target} timed out");
                    return false;
                }
            }
        }
        true
    }
}

fn complete_row(store: &Arc<Mutex<Store>>, ack: &Arc<Mutex<AckState>>, channel: &str, row_id: i64) {
    let next = {
        let mut ack = ack.lock().unwrap();
        ack.in_flight.remove(&row_id);
        ack.completed.insert(row_id);
        ack.advance_to()
    };
    if let Some(row_id) = next {
        if let Err(e) = store.lock().unwrap().set_cursor(channel, row_id) {
            error!("save state error: {e}");
        }
    }
}

fn runners(cfg: &Config) -> HashMap<AgentBackend, Runner> {
    let mut runners = HashMap::new();
    runners.insert(
        AgentBackend::Claude,
        Runner::Claude(claude::Runner {
            bin: cfg.claude_bin.clone(),
            permission_mode: cfg.claude_permission_mode.clone(),
            tools: cfg.claude_tools.clone(),
            allowed_tools: cfg.claude_allowed_tools.clone(),
            disallowed_tools: cfg.claude_disallowed_tools.clone(),
        }),
    );
    runners.insert(
        AgentBackend::Codex,
        Runner::Codex(codex::Runner {
            bin: cfg.codex_bin.clone(),
            sandbox: cfg.codex_sandbox.clone(),
            approval_policy: cfg.codex_approval_policy.clone(),
            model: cfg.codex_model.clone(),
        }),
    );
    runners
}

impl AckState {
    fn is_known(&self, row_id: i64) -> bool {
        self.in_flight.contains(&row_id) || self.completed.contains(&row_id)
    }

    fn advance_to(&mut self) -> Option<i64> {
        let limit = self.in_flight.first().copied().unwrap_or(i64::MAX);
        let next = self
            .completed
            .iter()
            .copied()
            .take_while(|id| *id < limit)
            .max()?;
        self.completed.retain(|id| *id > next);
        Some(next)
    }
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

fn sandbox(sessions_dir: &str, thread: &str) -> String {
    let directory_key = thread.strip_prefix("imessage:").unwrap_or(thread);
    format!("{sessions_dir}/{}", sanitize(directory_key))
}

/// Turns a thread key into a filesystem-safe directory name.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

async fn wait_for_shutdown_or<S, O>(shutdown: Pin<&mut S>, operation: O) -> Option<O::Output>
where
    S: Future<Output = ()> + ?Sized,
    O: Future,
{
    tokio::select! {
        _ = shutdown => None,
        output = operation => Some(output),
    }
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{FakeRunCall, FakeRunner};
    use crate::channel::{normalize_handle, thread_handle};
    use crate::imessage::{Poller, Sender};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use uuid::Uuid;

    #[tokio::test]
    async fn periodic_activity_refreshes_until_operation_completes() {
        let (complete, completed) = tokio::sync::oneshot::channel();
        let complete = Arc::new(Mutex::new(Some(complete)));
        let activity_count = Arc::new(AtomicUsize::new(0));
        let count = activity_count.clone();

        let output = tokio::time::timeout(
            Duration::from_secs(1),
            run_with_periodic_activity(completed, Duration::from_millis(1), move || {
                let complete = complete.clone();
                let count = count.clone();
                async move {
                    if count.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                        if let Some(complete) = complete.lock().unwrap().take() {
                            let _ = complete.send("done");
                        }
                    }
                }
            }),
        )
        .await
        .expect("activity loop should finish")
        .expect("operation should complete");

        assert_eq!(output, "done");
        assert!(activity_count.load(Ordering::SeqCst) >= 3);
        let final_count = activity_count.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(activity_count.load(Ordering::SeqCst), final_count);
    }

    #[tokio::test]
    async fn stalled_activity_does_not_delay_operation() {
        let output = tokio::time::timeout(
            Duration::from_millis(100),
            run_with_periodic_activity(
                async {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    "done"
                },
                Duration::from_secs(1),
                std::future::pending::<()>,
            ),
        )
        .await
        .expect("stalled activity should not block the operation");

        assert_eq!(output, "done");
    }

    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_a_pending_poll_operation() {
        let (send_shutdown, receive_shutdown) = tokio::sync::oneshot::channel();
        let shutdown = async {
            let _ = receive_shutdown.await;
        };
        tokio::pin!(shutdown);

        let operation_dropped = Arc::new(AtomicBool::new(false));
        let drop_flag = operation_dropped.clone();
        let operation = async move {
            let _drop_flag = DropFlag(drop_flag);
            std::future::pending::<()>().await;
        };
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = send_shutdown.send(());
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_shutdown_or(shutdown.as_mut(), operation),
        )
        .await
        .expect("shutdown should interrupt a pending poll");

        assert!(result.is_none());
        assert!(operation_dropped.load(Ordering::SeqCst));
    }

    fn filter() -> Channel {
        Channel::IMessage {
            poller: Poller::new("fake-chat.db".to_string()),
            sender: Sender::new(),
            self_set: [("me@icloud.com".to_string(), "me@icloud.com".to_string())]
                .into_iter()
                .collect(),
            allow_set: [("15551234567".to_string(), "+15551234567".to_string())]
                .into_iter()
                .collect(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }

    fn msg(chat: &str, handle: &str, from_me: bool, text: &str) -> RawMessage {
        RawMessage {
            row_id: 1,
            channel: "imessage",
            handle: handle.to_string(),
            chat_identifier: chat.to_string(),
            text: text.to_string(),
            is_from_me: from_me,
            is_group: false,
            is_supported: true,
            thread_id: None,
        }
    }

    fn group_msg(chat: &str, handle: &str, from_me: bool, text: &str) -> RawMessage {
        RawMessage {
            is_group: true,
            ..msg(chat, handle, from_me, text)
        }
    }

    fn temp_state_path() -> String {
        std::env::temp_dir()
            .join(format!("push-gateway-test-{}.json", Uuid::new_v4()))
            .to_string_lossy()
            .to_string()
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("push-gateway-{name}-{}", Uuid::new_v4()))
    }

    fn setup_failure_ctx(
        store: Arc<Mutex<Store>>,
        ack: Arc<Mutex<AckState>>,
        sessions_dir: String,
    ) -> Ctx {
        let mut runners = HashMap::new();
        runners.insert(
            AgentBackend::Claude,
            Runner::Claude(claude::Runner {
                bin: "claude".to_string(),
                permission_mode: "bypassPermissions".to_string(),
                tools: None,
                allowed_tools: Vec::new(),
                disallowed_tools: Vec::new(),
            }),
        );
        Ctx {
            store,
            ack,
            runners: Arc::new(runners),
            channel: filter(),
            run_timeout: Duration::from_secs(1),
            reply_marker: String::new(),
            sessions_dir,
            assistant_dir: String::new(),
            assistant: AssistantProfile::default(),
            audit: Arc::new(AuditLog::new(
                temp_path("setup-failure-audit")
                    .to_string_lossy()
                    .to_string(),
                false,
                "imessage",
            )),
            setup_failure_replies: Arc::new(Mutex::new(Vec::new())),
            sent_replies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn setup_failure_job(row_id: i64) -> Job {
        Job {
            row_id,
            thread: "imessage:self:me".to_string(),
            target: "me@icloud.com".to_string(),
            backend: AgentBackend::Claude,
            text: "hello".to_string(),
        }
    }

    #[test]
    fn self_chat_accepted() {
        let got = filter().accept(&msg("me@icloud.com", "", true, "hi"));
        assert_eq!(
            got,
            Some((
                "imessage:self:me@icloud.com".to_string(),
                "me@icloud.com".to_string()
            ))
        );
    }

    #[test]
    fn allowlisted_dm_accepted() {
        let got = filter().accept(&msg("+15551234567", "+15551234567", false, "hi"));
        assert_eq!(
            got,
            Some((
                "imessage:dm:+15551234567".to_string(),
                "+15551234567".to_string()
            ))
        );
    }

    #[test]
    fn formatted_phone_allowlist_matches_normalized_handle() {
        let got = filter().accept(&msg("+1 (555) 123-4567", "+1 (555) 123-4567", false, "hi"));
        assert_eq!(
            got,
            Some((
                "imessage:dm:+15551234567".to_string(),
                "+1 (555) 123-4567".to_string()
            ))
        );
    }

    #[test]
    fn bare_phone_matches_allowlist_with_plus() {
        let filter = Channel::IMessage {
            poller: Poller::new("fake-chat.db".to_string()),
            sender: Sender::new(),
            self_set: HashMap::new(),
            allow_set: ["+15551234567"]
                .into_iter()
                .map(|s| (normalize_handle(s), thread_handle(s)))
                .collect(),
            reply_marker: String::new(),
        };

        let got = filter.accept(&msg("15551234567", "15551234567", false, "hi"));

        assert_eq!(
            got,
            Some((
                "imessage:dm:+15551234567".to_string(),
                "15551234567".to_string()
            ))
        );
    }

    #[test]
    fn plus_phone_matches_bare_allowlist() {
        let filter = Channel::IMessage {
            poller: Poller::new("fake-chat.db".to_string()),
            sender: Sender::new(),
            self_set: HashMap::new(),
            allow_set: ["15551234567"]
                .into_iter()
                .map(|s| (normalize_handle(s), thread_handle(s)))
                .collect(),
            reply_marker: String::new(),
        };

        let got = filter.accept(&msg("+1 (555) 123-4567", "+1 (555) 123-4567", false, "hi"));

        assert_eq!(
            got,
            Some((
                "imessage:dm:15551234567".to_string(),
                "+1 (555) 123-4567".to_string()
            ))
        );
    }

    #[test]
    fn email_self_chat_matching_is_case_insensitive() {
        let got = filter().accept(&msg("ME@ICLOUD.COM", "", true, "hi"));
        assert_eq!(
            got,
            Some((
                "imessage:self:me@icloud.com".to_string(),
                "ME@ICLOUD.COM".to_string()
            ))
        );
    }

    #[test]
    fn non_allowlisted_dropped() {
        assert_eq!(
            filter().accept(&msg("+19998887777", "+19998887777", false, "hi")),
            None
        );
    }

    #[test]
    fn own_reply_dropped() {
        let m = msg("me@icloud.com", "", true, "an answer\n\n-- sent by push");
        assert_eq!(filter().accept(&m), None);
    }

    #[test]
    fn from_me_to_others_dropped() {
        assert_eq!(
            filter().accept(&msg("+12223334444", "+12223334444", true, "hey")),
            None
        );
    }

    #[test]
    fn empty_text_dropped() {
        assert_eq!(
            filter().accept(&msg("me@icloud.com", "", true, "   ")),
            None
        );
    }

    #[test]
    fn group_chat_dropped_even_from_allowlisted_sender() {
        assert_eq!(
            filter().accept(&group_msg("chat123456789", "+15551234567", false, "hi")),
            None
        );
    }

    #[test]
    fn imessage_keeps_legacy_sandbox_path_while_telegram_is_qualified() {
        assert_eq!(
            sandbox("/sessions", "imessage:dm:+15551234567"),
            "/sessions/dm__15551234567"
        );
        assert_eq!(
            sandbox("/sessions", "telegram:dm:15551234567"),
            "/sessions/telegram_dm_15551234567"
        );
    }

    #[test]
    fn ack_does_not_advance_past_in_flight_row() {
        let mut ack = AckState::default();
        ack.in_flight.insert(10);
        ack.completed.insert(11);

        assert_eq!(ack.advance_to(), None);
    }

    #[test]
    fn ack_advances_completed_rows_below_first_in_flight() {
        let mut ack = AckState::default();
        ack.in_flight.insert(12);
        ack.completed.insert(10);
        ack.completed.insert(11);
        ack.completed.insert(13);

        assert_eq!(ack.advance_to(), Some(11));
        assert!(ack.completed.contains(&13));
        assert!(!ack.completed.contains(&10));
        assert!(!ack.completed.contains(&11));
    }

    #[test]
    fn ack_advances_to_highest_completed_when_nothing_in_flight() {
        let mut ack = AckState::default();
        ack.completed.insert(10);
        ack.completed.insert(14);

        assert_eq!(ack.advance_to(), Some(14));
        assert!(ack.completed.is_empty());
    }

    #[test]
    fn setup_failure_completion_unblocks_later_completed_rows() {
        let path = temp_state_path();
        let store = Arc::new(Mutex::new(Store::open(&path).unwrap()));
        let ack = Arc::new(Mutex::new(AckState::default()));
        {
            let mut ack = ack.lock().unwrap();
            ack.in_flight.insert(10);
            ack.completed.insert(11);
        }

        complete_setup_failure_row(&store, &ack, "imessage", 10);

        assert_eq!(store.lock().unwrap().last_row(), 11);
        let ack = ack.lock().unwrap();
        assert!(ack.in_flight.is_empty());
        assert!(ack.completed.is_empty());

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn session_lookup_failure_completes_in_flight_row() {
        let blocker = temp_path("state-blocker");
        let state_path = blocker.join("state.json");
        let store = Arc::new(Mutex::new(
            Store::open(state_path.to_str().unwrap()).unwrap(),
        ));
        std::fs::write(&blocker, "not a directory").unwrap();
        let ack = Arc::new(Mutex::new(AckState::default()));
        {
            let mut ack = ack.lock().unwrap();
            ack.in_flight.insert(10);
            ack.completed.insert(11);
        }
        let ctx = setup_failure_ctx(
            store,
            ack.clone(),
            temp_path("sessions").to_string_lossy().to_string(),
        );

        handle(&ctx, setup_failure_job(10)).await;

        assert_eq!(
            ctx.setup_failure_replies.lock().unwrap().as_slice(),
            [SESSION_SETUP_FAILURE]
        );
        let ack = ack.lock().unwrap();
        assert!(ack.in_flight.is_empty());
        assert!(ack.completed.is_empty());

        let _ = std::fs::remove_file(blocker);
    }

    #[tokio::test]
    async fn sandbox_setup_failure_completes_in_flight_row_and_advances_cursor() {
        let state_path = temp_state_path();
        let store = Arc::new(Mutex::new(Store::open(&state_path).unwrap()));
        let sessions_blocker = temp_path("sessions-blocker");
        std::fs::write(&sessions_blocker, "not a directory").unwrap();
        let ack = Arc::new(Mutex::new(AckState::default()));
        {
            let mut ack = ack.lock().unwrap();
            ack.in_flight.insert(10);
            ack.completed.insert(11);
        }
        let ctx = setup_failure_ctx(
            store.clone(),
            ack.clone(),
            sessions_blocker.to_string_lossy().to_string(),
        );

        handle(&ctx, setup_failure_job(10)).await;

        assert_eq!(
            ctx.setup_failure_replies.lock().unwrap().as_slice(),
            [SANDBOX_SETUP_FAILURE]
        );
        assert_eq!(store.lock().unwrap().last_row(), 11);
        let ack = ack.lock().unwrap();
        assert!(ack.in_flight.is_empty());
        assert!(ack.completed.is_empty());

        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(sessions_blocker);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_channel_e2e_replies_once_ignores_unallowlisted_and_reuses_session() {
        let state_path = temp_state_path();
        let audit_path = format!("{state_path}.audit.jsonl");
        let sessions_dir = temp_path("e2e-sessions");
        let assistant_dir = temp_path("e2e-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));

        let mut handles = Vec::new();
        gateway
            .tick_fake(
                vec![
                    message(1, "+19998887777", "+19998887777", false, "ignore me"),
                    message(2, "+15551234567", "+15551234567", false, "first"),
                    message(3, "+15551234567", "+15551234567", false, "second"),
                ],
                &mut handles,
            )
            .await;
        gateway.queues.clear();
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(gateway.store.lock().unwrap().last_row(), 3);
        assert_eq!(
            gateway.ctx.sent_replies.lock().unwrap().as_slice(),
            [
                (
                    "+15551234567".to_string(),
                    "fake reply: first\n\n-- sent by push".to_string()
                ),
                (
                    "+15551234567".to_string(),
                    "fake reply: second\n\n-- sent by push".to_string()
                )
            ]
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            [
                FakeRunCall {
                    session_id: String::new(),
                    is_new: true,
                    prompt: "first".to_string(),
                },
                FakeRunCall {
                    session_id: "fake-session".to_string(),
                    is_new: false,
                    prompt: "second".to_string(),
                }
            ]
        );
        let events = audit_events(&audit_path);
        assert!(events.iter().any(|e| {
            e.event == "message_ignored"
                && e.row_id == Some(1)
                && e.reason.as_deref() == Some("not_allowlisted")
        }));
        assert!(events.iter().any(|e| {
            e.event == "message_accepted"
                && e.row_id == Some(2)
                && e.thread.as_deref() == Some("imessage:dm:+15551234567")
                && e.backend.as_deref() == Some("codex")
        }));
        assert!(events.iter().any(|e| {
            e.event == "backend_run_completed"
                && e.row_id == Some(2)
                && e.reply.as_ref().is_some_and(|c| c.text.is_none())
        }));
        assert!(events
            .iter()
            .any(|e| e.event == "reply_sent" && e.row_id == Some(2)));
        assert!(events
            .iter()
            .any(|e| e.event == "message_completed" && e.row_id == Some(3)));

        let replay_calls = Arc::new(Mutex::new(Vec::new()));
        let mut replay_gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        replay_gateway.ctx.runners = Arc::new(fake_runners(replay_calls.clone()));
        let mut replay_handles = Vec::new();
        replay_gateway
            .tick_fake(
                vec![message(3, "+15551234567", "+15551234567", false, "second")],
                &mut replay_handles,
            )
            .await;
        replay_gateway.queues.clear();
        for handle in replay_handles {
            handle.await.unwrap();
        }

        assert!(replay_gateway.ctx.sent_replies.lock().unwrap().is_empty());
        assert!(replay_calls.lock().unwrap().is_empty());

        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(audit_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn telegram_filters_before_agent_and_replies_to_originating_chat() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("telegram-sessions");
        let assistant_dir = temp_path("telegram-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.channel = "telegram".to_string();
        cfg.self_handles.clear();
        cfg.allow_from.clear();
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];
        let mut gateway = Gateway::new(cfg).unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));

        let mut handles = Vec::new();
        gateway
            .tick_fake(
                vec![
                    telegram_message(10, 8, 8, false, "ignore me"),
                    telegram_message(11, 7, 7, false, "hello"),
                    telegram_message(12, 7, -100, true, "group"),
                ],
                &mut handles,
            )
            .await;
        gateway.queues.clear();
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(gateway.store.lock().unwrap().cursor("telegram"), 12);
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0].prompt, "hello");
        assert_eq!(
            gateway.ctx.sent_replies.lock().unwrap().as_slice(),
            [("7".to_string(), "fake reply: hello".to_string())]
        );

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn telegram_topic_gets_own_thread_and_reply_targets_the_topic() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("telegram-topic-sessions");
        let assistant_dir = temp_path("telegram-topic-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.channel = "telegram".to_string();
        cfg.self_handles.clear();
        cfg.allow_from.clear();
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];
        let mut gateway = Gateway::new(cfg).unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));

        let mut topic_message = telegram_message(20, 7, 7, false, "in topic");
        topic_message.thread_id = Some(99);
        let mut handles = Vec::new();
        gateway
            .tick_fake(
                vec![topic_message, telegram_message(21, 7, 7, false, "in main")],
                &mut handles,
            )
            .await;
        gateway.queues.clear();
        for handle in handles {
            handle.await.unwrap();
        }

        assert_eq!(calls.lock().unwrap().len(), 2);
        assert!(calls.lock().unwrap().iter().all(|call| call.is_new));
        let replies = gateway.ctx.sent_replies.lock().unwrap().clone();
        assert!(replies.contains(&("7:99".to_string(), "fake reply: in topic".to_string())));
        assert!(replies.contains(&("7".to_string(), "fake reply: in main".to_string())));
        let events = audit_events(&format!("{state_path}.audit.jsonl"));
        assert!(events.iter().any(|e| {
            e.event == "message_accepted" && e.thread.as_deref() == Some("telegram:dm:7:topic:99")
        }));
        assert!(events.iter().any(|e| {
            e.event == "message_accepted" && e.thread.as_deref() == Some("telegram:dm:7")
        }));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    fn test_config(state_path: &str, sessions_dir: &str, assistant_dir: &str) -> Config {
        Config {
            channel: "imessage".to_string(),
            db_path: "fake-chat.db".to_string(),
            poll_interval: "1s".to_string(),
            run_timeout: "1s".to_string(),
            self_handles: vec!["me@icloud.com".to_string()],
            allow_from: vec!["+15551234567".to_string()],
            telegram_bot_token: None,
            telegram_bot_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            telegram_allow_user_ids: Vec::new(),
            telegram_allow_chat_ids: Vec::new(),
            agent: "codex".to_string(),
            routes: Vec::new(),
            assistant: AssistantProfile::default(),
            claude_bin: "claude".to_string(),
            claude_permission_mode: "bypassPermissions".to_string(),
            claude_tools: None,
            claude_allowed_tools: Vec::new(),
            claude_disallowed_tools: Vec::new(),
            codex_bin: "codex".to_string(),
            codex_sandbox: "workspace-write".to_string(),
            codex_approval_policy: "never".to_string(),
            codex_model: None,
            sessions_dir: sessions_dir.to_string(),
            state_path: state_path.to_string(),
            audit_log_path: format!("{state_path}.audit.jsonl"),
            audit_log_content: false,
            assistant_dir: assistant_dir.to_string(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }

    fn audit_events(path: &str) -> Vec<crate::audit::AuditEvent> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn fake_runners(calls: Arc<Mutex<Vec<FakeRunCall>>>) -> HashMap<AgentBackend, Runner> {
        let mut runners = HashMap::new();
        runners.insert(
            AgentBackend::Codex,
            Runner::Fake(FakeRunner {
                backend: AgentBackend::Codex,
                session_id: "fake-session".to_string(),
                calls,
            }),
        );
        runners
    }

    fn message(row_id: i64, chat: &str, handle: &str, is_from_me: bool, text: &str) -> RawMessage {
        RawMessage {
            row_id,
            channel: "imessage",
            handle: handle.to_string(),
            chat_identifier: chat.to_string(),
            text: text.to_string(),
            is_from_me,
            is_group: false,
            is_supported: true,
            thread_id: None,
        }
    }

    fn telegram_message(
        row_id: i64,
        user_id: i64,
        chat_id: i64,
        is_group: bool,
        text: &str,
    ) -> RawMessage {
        RawMessage {
            row_id,
            channel: "telegram",
            handle: user_id.to_string(),
            chat_identifier: chat_id.to_string(),
            text: text.to_string(),
            is_from_me: false,
            is_group,
            is_supported: true,
            thread_id: None,
        }
    }
}
