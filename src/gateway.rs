//! The message loop: poll, filter, and route each message to a per-thread
//! worker task that runs an agent backend and sends the reply.
//!
//! Design: each enabled channel has an independent loop, acknowledgement state,
//! and queue map. Channel loops share the durable store and history behind
//! short-lived locks. Each channel-qualified thread gets its own worker task,
//! which serializes that thread's messages.

use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, info, warn};

use crate::agent::{Request, RunError, Runner};
use crate::approval::{
    AnswerOrigin, AnswerOutcome, DeliveryStatus as ApprovalDeliveryStatus, Question,
};
use crate::audit::{AuditEvent, AuditLog};
use crate::channel::{Channel, RawMessage};
use crate::claude;
use crate::codex;
use crate::config::{AgentBackend, ChannelKind, Config, PermissionProfile, PrimaryDeliveryConfig};
use crate::history::{DeliveryStatus, History, OutboundMessage, OutboundOrigin};
use crate::rehydration::{self, RehydrationPrompt};
use crate::soul;
use crate::store::Store;

const QUEUE_DEPTH: usize = 32;
#[cfg(not(test))]
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
const TYPING_REFRESH: Duration = Duration::from_secs(4);
const DELIVERY_ATTEMPTS: usize = 3;
const SESSION_SETUP_FAILURE: &str =
    "Push could not prepare this conversation. Check the local logs, then resend.";
const SANDBOX_SETUP_FAILURE: &str =
    "Push could not create its session workspace. Check the local logs, then resend.";

#[derive(Clone)]
struct Job {
    row_id: i64,
    inbound_id: i64,
    thread: String,
    target: String,
    backend: AgentBackend,
    permission: PermissionProfile,
    text: String,
}

/// Shared, cheaply cloneable context handed to each worker task.
#[derive(Clone)]
struct Ctx {
    store: Arc<Mutex<Store>>,
    history: Arc<Mutex<History>>,
    ack: Arc<Mutex<AckState>>,
    runners: Arc<HashMap<AgentBackend, Runner>>,
    channel: Channel,
    run_timeout: Duration,
    reply_marker: String,
    sessions_dir: String,
    assistant_dir: String,
    audit: Arc<AuditLog>,
    #[cfg(test)]
    setup_failure_replies: Arc<Mutex<Vec<String>>>,
    #[cfg(test)]
    sent_replies: Arc<Mutex<Vec<(String, String)>>>,
    #[cfg(test)]
    send_failures_remaining: Arc<Mutex<usize>>,
}

pub struct Gateway {
    channel: Channel,
    store: Arc<Mutex<Store>>,
    ack: Arc<Mutex<AckState>>,
    ctx: Ctx,
    cfg: Config,
    poll_interval: Duration,
    queues: HashMap<String, mpsc::Sender<Job>>,
    handles: Vec<JoinHandle<()>>,
}

pub struct GatewayGroup {
    gateways: Vec<Gateway>,
    primary_delivery: Option<PrimaryDeliveryConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryDestination {
    pub channel: String,
    pub target: String,
}

#[derive(Default)]
struct AckState {
    in_flight: BTreeSet<i64>,
    completed: BTreeSet<i64>,
}

impl GatewayGroup {
    pub fn new(cfg: Config) -> Result<Self> {
        let enabled = cfg.enabled_channel_kinds()?;
        let store = Arc::new(Mutex::new(Store::open(&cfg.state_path)?));
        let history = Arc::new(Mutex::new(History::open(&cfg.database_path).with_context(
            || format!("open canonical history database {}", cfg.database_path),
        )?));
        let runners = Arc::new(runners(&cfg));
        let audit_lock = Arc::new(Mutex::new(()));
        let mut gateways = Vec::with_capacity(enabled.len());
        for kind in enabled {
            gateways.push(Gateway::new_with_shared(
                cfg.clone(),
                kind,
                store.clone(),
                history.clone(),
                runners.clone(),
                audit_lock.clone(),
            )?);
        }
        Ok(Self {
            gateways,
            primary_delivery: cfg.primary_delivery,
        })
    }

    #[allow(dead_code)]
    pub fn primary_destination(&self) -> Result<PrimaryDestination> {
        let configured = self
            .primary_delivery
            .as_ref()
            .context("primary delivery is not configured")?;
        let kind =
            ChannelKind::parse(&configured.channel).context("invalid primary delivery channel")?;
        let gateway = self
            .gateways
            .iter()
            .find(|gateway| gateway.channel.id() == kind.as_str())
            .with_context(|| {
                format!(
                    "primary delivery channel {:?} is not enabled in channels",
                    configured.channel
                )
            })?;
        let target = gateway
            .channel
            .primary_target(&configured.target)
            .context("invalid primary delivery target")?;
        Ok(PrimaryDestination {
            channel: kind.as_str().to_string(),
            target,
        })
    }

    #[allow(dead_code)]
    pub async fn deliver_primary(&self, text: &str) -> Result<PrimaryDestination> {
        if text.trim().is_empty() {
            anyhow::bail!("primary delivery text cannot be empty");
        }
        let destination = self.primary_destination()?;
        let gateway = self
            .gateways
            .iter()
            .find(|gateway| gateway.channel.id() == destination.channel)
            .context("resolved primary delivery channel is unavailable")?;
        if !reply_to(&gateway.ctx, &destination.target, text).await {
            anyhow::bail!(
                "primary delivery to {} target {:?} failed",
                destination.channel,
                destination.target
            );
        }
        Ok(destination)
    }

    #[allow(dead_code)]
    pub async fn ask_user(&self, question: Question) -> Result<String> {
        let gateway = self
            .gateways
            .iter()
            .find(|gateway| gateway.channel.id() == question.channel)
            .with_context(|| {
                format!(
                    "approval question channel {:?} is not enabled",
                    question.channel
                )
            })?;
        gateway.ask_user(question).await
    }

    pub async fn run(self) -> Result<()> {
        self.run_until(shutdown_signal()).await
    }

    async fn run_until<S>(self, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()>,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();
        for gateway in self.gateways {
            let channel = gateway.channel.id();
            let receiver = shutdown_rx.clone();
            tasks.spawn(async move {
                gateway
                    .run_with_shutdown(receiver)
                    .await
                    .with_context(|| format!("{channel} channel stopped"))?;
                Ok(channel)
            });
        }
        drop(shutdown_rx);
        coordinate_channel_tasks(tasks, shutdown_tx, shutdown).await
    }
}

async fn coordinate_channel_tasks<S>(
    mut tasks: JoinSet<Result<&'static str>>,
    shutdown_tx: watch::Sender<bool>,
    shutdown: S,
) -> Result<()>
where
    S: Future<Output = ()>,
{
    let mut active = tasks.len();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            result = tasks.join_next(), if active > 0 => {
                active -= 1;
                match result {
                    Some(Ok(Ok(channel))) => warn!("{channel} channel exited before shutdown"),
                    Some(Ok(Err(error))) => error!("{error:#}"),
                    Some(Err(error)) => error!("channel task failed: {error}"),
                    None => {}
                }
                if active == 0 {
                    anyhow::bail!("all enabled reply channels stopped");
                }
            }
        }
    }

    let _ = shutdown_tx.send(true);
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(channel)) => info!("{channel} channel shut down cleanly"),
            Ok(Err(error)) => error!("{error:#}"),
            Err(error) => error!("channel task failed during shutdown: {error}"),
        }
    }
    Ok(())
}

impl Gateway {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open(&cfg.state_path)?));
        let history = Arc::new(Mutex::new(History::open(&cfg.database_path).with_context(
            || format!("open canonical history database {}", cfg.database_path),
        )?));
        let runners = Arc::new(runners(&cfg));
        let kind = cfg
            .enabled_channel_kinds()?
            .into_iter()
            .next()
            .context("at least one reply channel must be enabled")?;
        Self::new_with_shared(cfg, kind, store, history, runners, Arc::new(Mutex::new(())))
    }

    fn new_with_shared(
        cfg: Config,
        kind: ChannelKind,
        store: Arc<Mutex<Store>>,
        history: Arc<Mutex<History>>,
        runners: Arc<HashMap<AgentBackend, Runner>>,
        audit_lock: Arc<Mutex<()>>,
    ) -> Result<Self> {
        let ack = Arc::new(Mutex::new(AckState::default()));
        let channel = Channel::new_for(&cfg, kind)?;
        let audit = Arc::new(AuditLog::with_lock(
            cfg.audit_log_path.clone(),
            cfg.audit_log_content,
            channel.id(),
            audit_lock,
        ));
        let ctx = Ctx {
            store: store.clone(),
            history,
            ack: ack.clone(),
            runners,
            channel: channel.clone(),
            run_timeout: cfg.run_timeout_dur()?,
            reply_marker: cfg.reply_marker.clone(),
            sessions_dir: cfg.sessions_dir.clone(),
            assistant_dir: cfg.assistant_dir.clone(),
            audit,
            #[cfg(test)]
            setup_failure_replies: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            sent_replies: Arc::new(Mutex::new(Vec::new())),
            #[cfg(test)]
            send_failures_remaining: Arc::new(Mutex::new(0)),
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
            handles: Vec::new(),
        })
    }

    #[allow(dead_code)]
    pub async fn ask_user(&self, question: Question) -> Result<String> {
        if question.channel != self.channel.id() {
            anyhow::bail!(
                "question channel {:?} does not match running channel {:?}",
                question.channel,
                self.channel.id()
            );
        }
        let id = question.id.clone();
        self.ctx
            .history
            .lock()
            .unwrap()
            .create_question(&question, now_ms())?;
        let delivered = reply_to(&self.ctx, &question.target, &question.render_text()).await;
        self.ctx.history.lock().unwrap().mark_question_delivery(
            &id,
            if delivered {
                ApprovalDeliveryStatus::Delivered
            } else {
                ApprovalDeliveryStatus::Failed
            },
        )?;
        if !delivered {
            anyhow::bail!("approval question {id} could not be delivered");
        }
        Ok(id)
    }

    /// Runs until SIGINT/SIGTERM, then drains in-flight runs and returns.
    #[allow(dead_code)]
    pub async fn run(self) -> Result<()> {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(async move {
            shutdown_signal().await;
            let _ = shutdown_tx.send(true);
        });
        self.run_with_shutdown(shutdown_rx).await
    }

    async fn run_with_shutdown(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        if let Some(result) = wait_for_channel_shutdown_or(&mut shutdown, self.skip_backlog()).await
        {
            result?;
            let mut ticker = tokio::time::interval(self.poll_interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            info!(
                "{} gateway running, polling every {:?}",
                self.channel.id(),
                self.poll_interval
            );

            loop {
                let poll = async {
                    ticker.tick().await;
                    self.tick().await;
                };
                if wait_for_channel_shutdown_or(&mut shutdown, poll)
                    .await
                    .is_none()
                {
                    break;
                }
            }
        }
        info!(
            "shutdown requested for {}, draining in-flight runs",
            self.channel.id()
        );

        // Dropping the senders lets each worker finish its current job, drain
        // its queue, and exit. Then wait, bounded by a grace period.
        self.queues.clear();
        self.drain_workers().await;
        info!("shut down cleanly");
        Ok(())
    }

    async fn drain_workers(&mut self) {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let mut workers = std::mem::take(&mut self.handles).into_iter();
        while let Some(mut worker) = workers.next() {
            if tokio::time::timeout_at(deadline, &mut worker)
                .await
                .is_err()
            {
                worker.abort();
                let _ = worker.await;
                for worker in workers {
                    worker.abort();
                    let _ = worker.await;
                }
                warn!("shutdown grace expired; remaining channel workers were aborted");
                return;
            }
        }
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

    async fn tick(&mut self) {
        let since = self.store.lock().unwrap().cursor(self.channel.id());
        let msgs = match self.channel.poll(since).await {
            Ok(messages) => messages,
            Err(e) => {
                error!("{} poll error: {e}", self.channel.id());
                return;
            }
        };

        self.process_messages(msgs).await;
    }

    #[cfg(test)]
    async fn tick_fake(&mut self, msgs: Vec<RawMessage>) {
        self.process_messages(msgs).await;
    }

    async fn process_messages(&mut self, msgs: Vec<RawMessage>) {
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
                let approval = self.ctx.history.lock().unwrap().answer_question(
                    &approval_origin(m, &thread),
                    m.text.trim(),
                    now_ms(),
                );
                match approval {
                    Ok(AnswerOutcome::NotAnAnswer) => {}
                    Ok(outcome) => {
                        self.audit_approval(m.row_id, &thread, &outcome);
                        self.complete_row(m.row_id, "approval_answer");
                        continue;
                    }
                    Err(error) => {
                        error!("[{thread}] approval lookup failed: {error}");
                        self.audit(self.ctx.audit.failed(
                            "approval_answer_failed",
                            m.row_id,
                            &thread,
                            None,
                            error.to_string(),
                        ));
                        return;
                    }
                }
                let inbound_id = match self.ctx.history.lock().unwrap().record_inbound(
                    m.channel,
                    &thread,
                    &m.event_id(),
                    m.text.trim(),
                ) {
                    Ok(id) => id,
                    Err(error) => {
                        error!(
                            "[{}] canonical history write failed for event {}: {error}; message will be retried",
                            thread,
                            m.event_id()
                        );
                        self.audit(self.ctx.audit.failed(
                            "message_history_failed",
                            m.row_id,
                            &thread,
                            None,
                            error.to_string(),
                        ));
                        return;
                    }
                };
                let route = match self.cfg.route_for_message(m.channel, &thread) {
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
                let backend = route.backend;
                self.audit(self.ctx.audit.accepted(m, &thread, backend));
                info!(
                    "[{thread}] new message accepted; routing to {}",
                    backend.as_str()
                );
                let job = Job {
                    row_id: m.row_id,
                    inbound_id,
                    thread,
                    target,
                    backend,
                    permission: route.permission,
                    text: m.text.trim().to_string(),
                };
                if !self.route(job).await {
                    return;
                }
            } else {
                self.audit(self.ctx.audit.ignored(m, self.channel.reject_reason(m)));
                self.complete_row(m.row_id, "ignored");
            }
        }
        if max_row > 0 && msgs.is_empty() {
            self.complete_row(max_row, "empty_poll");
        }
    }

    async fn route(&mut self, job: Job) -> bool {
        let row_id = job.row_id;
        let thread = job.thread.clone();
        let target = job.target.clone();
        let backend = job.backend;
        if !self.queues.contains_key(&thread) {
            let (tx, rx) = mpsc::channel::<Job>(QUEUE_DEPTH);
            let ctx = self.ctx.clone();
            self.handles.push(tokio::spawn(worker(ctx, rx)));
            self.queues.insert(thread.clone(), tx);
        }

        let full = matches!(
            self.queues.get(&thread).unwrap().try_send(job.clone()),
            Err(mpsc::error::TrySendError::Full(_))
        );
        if full {
            self.ack.lock().unwrap().in_flight.insert(row_id);
            warn!("[{thread}] queue full, asking sender to resend");
            self.audit(self.ctx.audit.failed(
                "message_queue_failed",
                row_id,
                &thread,
                Some(backend),
                "queue full",
            ));
            let reply = "I'm a bit behind on this thread - resend that in a moment.";
            match record_and_deliver(&self.ctx, &job, OutboundOrigin::Gateway, reply).await {
                Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
                    self.audit(self.ctx.audit.reply_sent(
                        row_id,
                        &thread,
                        &target,
                        Some(backend),
                        reply,
                    ));
                    self.complete_row(row_id, "queue_full");
                }
                Err(error) => {
                    error!("[{thread}] canonical history write failed: {error}");
                    self.audit(self.ctx.audit.failed(
                        "message_history_failed",
                        row_id,
                        &thread,
                        Some(backend),
                        error.to_string(),
                    ));
                    self.ack.lock().unwrap().in_flight.remove(&row_id);
                    return false;
                }
            }
        } else {
            self.ack.lock().unwrap().in_flight.insert(row_id);
        }
        true
    }

    fn complete_row(&self, row_id: i64, reason: &str) {
        self.audit(self.ctx.audit.completed(row_id, reason));
        complete_row(&self.store, &self.ack, self.channel.id(), row_id);
    }

    fn audit(&self, event: AuditEvent) {
        audit(&self.ctx, event);
    }

    fn audit_approval(&self, row_id: i64, thread: &str, outcome: &AnswerOutcome) {
        let (event, reason) = match outcome {
            AnswerOutcome::Selected(answer) => (
                "approval_answer_selected",
                format!(
                    "correlation_id={}, selected_number={}",
                    answer.correlation_id, answer.selected_number
                ),
            ),
            AnswerOutcome::Expired(id) => ("approval_answer_rejected", format!("expired:{id}")),
            AnswerOutcome::Duplicate(id) => ("approval_answer_rejected", format!("duplicate:{id}")),
            AnswerOutcome::Cancelled(id) => ("approval_answer_rejected", format!("cancelled:{id}")),
            AnswerOutcome::Mismatched(id) => {
                ("approval_answer_rejected", format!("mismatched:{id}"))
            }
            AnswerOutcome::InvalidChoice(id) => {
                ("approval_answer_rejected", format!("invalid_choice:{id}"))
            }
            AnswerOutcome::Ambiguous => (
                "approval_answer_rejected",
                "ambiguous_plain_number".to_string(),
            ),
            AnswerOutcome::NotAnAnswer => return,
        };
        self.audit(self.ctx.audit.approval(event, row_id, thread, reason));
    }
}

/// Processes one thread's jobs strictly in order, exiting when the queue closes.
async fn worker(ctx: Ctx, mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        handle(&ctx, job).await;
    }
}

async fn handle(ctx: &Ctx, job: Job) {
    let existing_outbound = { ctx.history.lock().unwrap().outbound_for(job.inbound_id) };
    match existing_outbound {
        Ok(Some(outbound)) => {
            match deliver_stored(ctx, &job, &outbound).await {
                Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
                    audit(
                        ctx,
                        ctx.audit.reply_sent(
                            job.row_id,
                            &job.thread,
                            &job.target,
                            Some(job.backend),
                            &outbound.content,
                        ),
                    );
                    complete_job(ctx, &job, "recovered_outbound");
                }
                Err(error) => history_error(ctx, &job, "recover outbound", error),
            }
            return;
        }
        Ok(None) => {}
        Err(error) => {
            history_error(ctx, &job, "read outbound", error);
            return;
        }
    }

    if let Some(reply) = command(ctx, &job) {
        match record_and_deliver(ctx, &job, OutboundOrigin::Gateway, &reply).await {
            Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
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
            }
            Err(error) => history_error(ctx, &job, "record command reply", error),
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

    let instructions = match soul::load(&ctx.assistant_dir) {
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
                backend_request(
                    &session_id,
                    is_new,
                    &work_dir,
                    &instructions,
                    job.permission.capability,
                    prompt,
                ),
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
                                job.permission.capability,
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
                    backend_request(
                        &session_id,
                        true,
                        &work_dir,
                        &instructions,
                        job.permission.capability,
                        prompt,
                    ),
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
            match deliver_stored(ctx, &job, &outbound).await {
                Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
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
                }
                Err(error) => history_error(ctx, &job, "deliver backend reply", error),
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
            let reply = "That took too long and was stopped. Try again or simplify the request.";
            match record_and_deliver(ctx, &job, OutboundOrigin::Gateway, reply).await {
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
                    complete_job(ctx, &job, "timeout");
                }
                Err(error) => history_error(ctx, &job, "record timeout reply", error),
            }
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
            match record_and_deliver(ctx, &job, OutboundOrigin::Gateway, &reply).await {
                Ok(DeliveryOutcome::Delivered | DeliveryOutcome::AlreadyDelivered) => {
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
                    complete_job(ctx, &job, "backend_failed");
                }
                Err(error) => history_error(ctx, &job, "record failure reply", error),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryOutcome {
    Delivered,
    AlreadyDelivered,
}

async fn record_and_deliver(
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

async fn reply_to(ctx: &Ctx, target: &str, text: &str) -> bool {
    let chunks = ctx.channel.outbound_chunks(text, &ctx.reply_marker);
    #[cfg(test)]
    {
        let mut failures = ctx.send_failures_remaining.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return false;
        }
        drop(failures);
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
        }),
    );
    runners.insert(
        AgentBackend::Codex,
        Runner::Codex(codex::Runner {
            bin: cfg.codex_bin.clone(),
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
    permission: crate::config::PermissionCapability,
    prompt: &'a str,
) -> Request<'a> {
    Request {
        session_id,
        is_new,
        work_dir,
        instructions,
        permission,
        prompt,
    }
}

fn approval_origin(message: &RawMessage, thread: &str) -> AnswerOrigin {
    if message.channel == "imessage" {
        let chat_key = crate::channel::normalize_handle(&message.chat_identifier);
        let sender_key = if thread.starts_with("imessage:self:") {
            chat_key.clone()
        } else {
            crate::channel::normalize_handle(&message.handle)
        };
        AnswerOrigin {
            channel: message.channel.to_string(),
            thread_key: thread.to_string(),
            sender_key,
            chat_key,
        }
    } else {
        AnswerOrigin {
            channel: message.channel.to_string(),
            thread_key: thread.to_string(),
            sender_key: message.handle.clone(),
            chat_key: message.chat_identifier.clone(),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
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

async fn wait_for_channel_shutdown_or<O>(
    shutdown: &mut watch::Receiver<bool>,
    operation: O,
) -> Option<O::Output>
where
    O: Future,
{
    if *shutdown.borrow() {
        return None;
    }
    tokio::pin!(operation);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return None;
                }
            }
            output = &mut operation => return Some(output),
        }
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
pub(crate) mod tests {
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
        let (send_shutdown, mut receive_shutdown) = watch::channel(false);

        let operation_dropped = Arc::new(AtomicBool::new(false));
        let drop_flag = operation_dropped.clone();
        let operation = async move {
            let _drop_flag = DropFlag(drop_flag);
            std::future::pending::<()>().await;
        };
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = send_shutdown.send(true);
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_channel_shutdown_or(&mut receive_shutdown, operation),
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
            }),
        );
        let history_path = temp_path("setup-failure-history");
        let mut history = History::open(history_path.to_str().unwrap()).unwrap();
        let inbound_id = history
            .record_inbound("imessage", "imessage:self:me", "imessage:10", "hello")
            .unwrap();
        assert_eq!(inbound_id, 1);
        Ctx {
            store,
            history: Arc::new(Mutex::new(history)),
            ack,
            runners: Arc::new(runners),
            channel: filter(),
            run_timeout: Duration::from_secs(1),
            reply_marker: String::new(),
            sessions_dir,
            assistant_dir: String::new(),
            audit: Arc::new(AuditLog::new(
                temp_path("setup-failure-audit")
                    .to_string_lossy()
                    .to_string(),
                false,
                "imessage",
            )),
            setup_failure_replies: Arc::new(Mutex::new(Vec::new())),
            sent_replies: Arc::new(Mutex::new(Vec::new())),
            send_failures_remaining: Arc::new(Mutex::new(0)),
        }
    }

    fn setup_failure_job(row_id: i64) -> Job {
        Job {
            row_id,
            inbound_id: 1,
            thread: "imessage:self:me".to_string(),
            target: "me@icloud.com".to_string(),
            backend: AgentBackend::Claude,
            permission: PermissionProfile {
                name: "restricted".to_string(),
                capability: crate::config::PermissionCapability::ReadOnly,
            },
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

    #[tokio::test]
    async fn soul_read_failure_stops_backend_dispatch_and_completes_row() {
        let state_path = temp_state_path();
        let store = Arc::new(Mutex::new(Store::open(&state_path).unwrap()));
        let sessions_dir = temp_path("soul-failure-sessions");
        let assistant_dir = temp_path("soul-failure-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        std::fs::write(assistant_dir.join("SOUL.md"), [0xff, 0xfe]).unwrap();
        let ack = Arc::new(Mutex::new(AckState::default()));
        ack.lock().unwrap().in_flight.insert(10);
        let mut ctx = setup_failure_ctx(
            store.clone(),
            ack.clone(),
            sessions_dir.to_string_lossy().to_string(),
        );
        ctx.assistant_dir = assistant_dir.to_string_lossy().to_string();

        handle(&ctx, setup_failure_job(10)).await;

        assert_eq!(
            ctx.setup_failure_replies.lock().unwrap().as_slice(),
            [SESSION_SETUP_FAILURE]
        );
        assert_eq!(store.lock().unwrap().last_row(), 10);
        assert!(ack.lock().unwrap().in_flight.is_empty());

        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fake_channel_e2e_replies_once_ignores_unallowlisted_and_reuses_session() {
        let state_path = temp_state_path();
        let audit_path = format!("{state_path}.audit.jsonl");
        let sessions_dir = temp_path("e2e-sessions");
        let assistant_dir = temp_path("e2e-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.routes = vec![crate::config::RouteRule {
            thread: Some("imessage:dm:+15551234567".to_string()),
            channel: None,
            agent: "codex".to_string(),
            permission_profile: Some("workspace".to_string()),
        }];
        let mut gateway = Gateway::new(cfg).unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));

        gateway
            .tick_fake(vec![
                message(1, "+19998887777", "+19998887777", false, "ignore me"),
                message(2, "+15551234567", "+15551234567", false, "first"),
                message(3, "+15551234567", "+15551234567", false, "second"),
            ])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

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
                    permission: crate::config::PermissionCapability::Workspace,
                },
                FakeRunCall {
                    session_id: "fake-session".to_string(),
                    is_new: false,
                    prompt: "second".to_string(),
                    permission: crate::config::PermissionCapability::Workspace,
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
        replay_gateway
            .tick_fake(vec![message(
                3,
                "+15551234567",
                "+15551234567",
                false,
                "second",
            )])
            .await;
        replay_gateway.queues.clear();
        replay_gateway.drain_workers().await;

        assert!(replay_gateway.ctx.sent_replies.lock().unwrap().is_empty());
        assert!(replay_calls.lock().unwrap().is_empty());

        let _ = std::fs::remove_file(state_path);
        let _ = std::fs::remove_file(audit_path);
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn imessage_question_delivers_and_plain_number_resolves_once() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("imessage-approval-sessions");
        let assistant_dir = temp_path("imessage-approval-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        let question = approval_question(
            "imessage",
            "imessage:self:me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
        );
        let id = gateway.ask_user(question).await.unwrap();

        run_messages(
            &mut gateway,
            vec![
                message(1, "me@icloud.com", "", true, "2"),
                message(2, "me@icloud.com", "", true, "2"),
            ],
        )
        .await;

        assert!(calls.lock().unwrap().is_empty());
        assert!(gateway.ctx.sent_replies.lock().unwrap()[0]
            .1
            .contains("1. Approve"));
        assert_eq!(
            gateway
                .ctx
                .history
                .lock()
                .unwrap()
                .take_answer(&id, now_ms())
                .unwrap()
                .unwrap()
                .value,
            "reject"
        );
        run_messages(
            &mut gateway,
            vec![message(3, "me@icloud.com", "", true, "hello")],
        )
        .await;
        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(calls.lock().unwrap()[0].prompt, "hello");
        let events = audit_events(&format!("{state_path}.audit.jsonl"));
        assert!(events
            .iter()
            .any(|event| event.event == "approval_answer_selected"));
        assert!(events.iter().any(|event| {
            event.event == "approval_answer_rejected"
                && event.reason.as_deref() == Some(&format!("duplicate:{id}"))
        }));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn telegram_question_rejects_wrong_topic_sender_and_duplicate() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("telegram-approval-sessions");
        let assistant_dir = temp_path("telegram-approval-assistant");
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
        cfg.telegram_allow_user_ids = vec![7, 8];
        let mut gateway = Gateway::new(cfg).unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        let question = approval_question("telegram", "telegram:dm:7:topic:9", "7", "7", "7:9");
        let id = gateway.ask_user(question).await.unwrap();
        let correlated = format!("{id} 1");
        let mut wrong_topic = telegram_message(10, 7, 7, false, &correlated);
        wrong_topic.thread_id = None;
        let mut unallowlisted = telegram_message(11, 9, 7, false, &correlated);
        unallowlisted.thread_id = Some(9);
        let mut wrong_sender = telegram_message(12, 8, 7, false, &correlated);
        wrong_sender.thread_id = Some(9);
        let mut malformed = telegram_message(13, 7, 7, false, &format!("{id} junk"));
        malformed.thread_id = Some(9);
        let mut valid = telegram_message(14, 7, 7, false, "1");
        valid.thread_id = Some(9);
        let mut duplicate = telegram_message(15, 7, 7, false, &correlated);
        duplicate.thread_id = Some(9);

        run_messages(
            &mut gateway,
            vec![
                wrong_topic,
                unallowlisted,
                wrong_sender,
                malformed,
                valid,
                duplicate,
            ],
        )
        .await;

        assert!(calls.lock().unwrap().is_empty());
        let events = audit_events(&format!("{state_path}.audit.jsonl"));
        assert!(events.iter().any(|event| {
            event.event == "approval_answer_rejected"
                && event.reason.as_deref() == Some(&format!("mismatched:{id}"))
        }));
        assert!(events
            .iter()
            .any(|event| { event.event == "message_ignored" && event.row_id == Some(11) }));
        assert!(events.iter().any(|event| {
            event.event == "approval_answer_rejected"
                && event.reason.as_deref() == Some(&format!("duplicate:{id}"))
        }));
        assert!(events.iter().any(|event| {
            event.event == "approval_answer_rejected"
                && event.reason.as_deref() == Some(&format!("invalid_choice:{id}"))
        }));
        assert_eq!(
            gateway
                .ctx
                .history
                .lock()
                .unwrap()
                .take_answer(&id, now_ms())
                .unwrap()
                .unwrap()
                .value,
            "approve"
        );

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_question_delivery_keeps_the_durable_pending_question() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("approval-delivery-sessions");
        let assistant_dir = temp_path("approval-delivery-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        *gateway.ctx.send_failures_remaining.lock().unwrap() = 1;
        let question = approval_question(
            "imessage",
            "imessage:self:me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
        );
        let id = question.id.clone();

        assert!(gateway.ask_user(question).await.is_err());
        assert!(matches!(
            gateway.ctx.history.lock().unwrap().answer_question(
                &AnswerOrigin {
                    channel: "imessage".to_string(),
                    thread_key: "imessage:self:me@icloud.com".to_string(),
                    sender_key: "me@icloud.com".to_string(),
                    chat_key: "me@icloud.com".to_string(),
                },
                &format!("{id} 1"),
                now_ms(),
            ),
            Ok(AnswerOutcome::Selected(_))
        ));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn expired_answer_is_audited_without_reaching_the_backend() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("approval-expiry-sessions");
        let assistant_dir = temp_path("approval-expiry-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        let mut question = approval_question(
            "imessage",
            "imessage:self:me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
            "me@icloud.com",
        );
        let created_at = now_ms();
        question.expires_at_ms = created_at + 10;
        let id = question.id.clone();
        gateway
            .ctx
            .history
            .lock()
            .unwrap()
            .create_question(&question, created_at)
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        run_messages(
            &mut gateway,
            vec![message(1, "me@icloud.com", "", true, &format!("{id} 1"))],
        )
        .await;

        assert!(calls.lock().unwrap().is_empty());
        assert!(audit_events(&format!("{state_path}.audit.jsonl"))
            .iter()
            .any(|event| {
                event.event == "approval_answer_rejected"
                    && event.reason.as_deref() == Some(&format!("expired:{id}"))
            }));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_backend_session_rotates_and_rehydrates_once() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("missing-session-rehydration");
        let assistant_dir = temp_path("missing-session-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let missing = Arc::new(AtomicBool::new(true));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        let mut runners = HashMap::new();
        runners.insert(
            AgentBackend::Codex,
            Runner::Fake(FakeRunner {
                backend: AgentBackend::Codex,
                session_id: "fake-session".to_string(),
                calls: calls.clone(),
                before_return: None,
                resume_missing_once: Some(missing),
            }),
        );
        gateway.ctx.runners = Arc::new(runners);

        run_messages(
            &mut gateway,
            vec![message(1, "+15551234567", "+15551234567", false, "first")],
        )
        .await;
        run_messages(
            &mut gateway,
            vec![message(2, "+15551234567", "+15551234567", false, "second")],
        )
        .await;

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert!(!calls[1].is_new);
        assert_eq!(calls[1].prompt, "second");
        assert!(calls[2].is_new);
        assert!(calls[2]
            .prompt
            .contains(r#"{"role":"user","content":"first"}"#));
        assert!(calls[2]
            .prompt
            .contains(r#"{"role":"assistant","content":"fake reply: first"}"#));
        assert!(calls[2]
            .prompt
            .ends_with(r#"{"role":"user","content":"second"}"#));
        drop(calls);

        let events = audit_events(&format!("{state_path}.audit.jsonl"));
        assert!(events
            .iter()
            .any(|event| event.event == "backend_session_missing"));
        assert!(events.iter().any(|event| {
            event.event == "backend_run_started"
                && event.row_id == Some(2)
                && event.is_new_session == Some(true)
                && event.rehydrated_messages == Some(2)
        }));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn backend_switch_and_clear_start_fresh_sessions_with_history() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("switch-rehydration");
        let assistant_dir = temp_path("switch-rehydration-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let codex_calls = Arc::new(Mutex::new(Vec::new()));
        let claude_calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        let mut runners = HashMap::new();
        runners.insert(
            AgentBackend::Codex,
            Runner::Fake(FakeRunner {
                backend: AgentBackend::Codex,
                session_id: "codex-session".to_string(),
                calls: codex_calls.clone(),
                before_return: None,
                resume_missing_once: None,
            }),
        );
        runners.insert(
            AgentBackend::Claude,
            Runner::Fake(FakeRunner {
                backend: AgentBackend::Claude,
                session_id: "claude-session".to_string(),
                calls: claude_calls.clone(),
                before_return: None,
                resume_missing_once: None,
            }),
        );
        gateway.ctx.runners = Arc::new(runners);

        run_messages(
            &mut gateway,
            vec![message(1, "+15551234567", "+15551234567", false, "first")],
        )
        .await;
        gateway.cfg.routes = vec![crate::config::RouteRule {
            thread: Some("imessage:dm:+15551234567".to_string()),
            channel: None,
            agent: "claude".to_string(),
            permission_profile: None,
        }];
        run_messages(
            &mut gateway,
            vec![message(2, "+15551234567", "+15551234567", false, "switch")],
        )
        .await;
        run_messages(
            &mut gateway,
            vec![message(3, "+15551234567", "+15551234567", false, "/clear")],
        )
        .await;
        run_messages(
            &mut gateway,
            vec![message(
                4,
                "+15551234567",
                "+15551234567",
                false,
                "after clear",
            )],
        )
        .await;

        assert_eq!(codex_calls.lock().unwrap().len(), 1);
        let claude_calls = claude_calls.lock().unwrap();
        assert_eq!(claude_calls.len(), 2);
        assert!(claude_calls[0].is_new);
        assert!(claude_calls[0].prompt.contains("first"));
        assert!(claude_calls[0]
            .prompt
            .ends_with(r#"{"role":"user","content":"switch"}"#));
        assert!(claude_calls[1].is_new);
        assert!(claude_calls[1].prompt.contains("switch"));
        assert!(claude_calls[1]
            .prompt
            .ends_with(r#"{"role":"user","content":"after clear"}"#));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canonical_history_failure_prevents_backend_dispatch_and_cursor_advance() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("history-failure-sessions");
        let assistant_dir = temp_path("history-failure-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        gateway
            .ctx
            .history
            .lock()
            .unwrap()
            .execute_batch_for_test("DROP TABLE messages");
        gateway
            .tick_fake(vec![message(1, "me@icloud.com", "", true, "hello")])
            .await;

        assert!(gateway.handles.is_empty());
        assert!(calls.lock().unwrap().is_empty());
        assert!(gateway.ctx.sent_replies.lock().unwrap().is_empty());
        assert_eq!(gateway.store.lock().unwrap().cursor("imessage"), 0);

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pending_outbound_is_delivered_after_restart_without_backend_rerun() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("history-recovery-sessions");
        let assistant_dir = temp_path("history-recovery-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let config = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        let mut history = History::open(&config.database_path).unwrap();
        let inbound_id = history
            .record_inbound(
                "imessage",
                "imessage:self:me@icloud.com",
                "imessage:1",
                "hello",
            )
            .unwrap();
        history
            .record_outbound(
                inbound_id,
                OutboundOrigin::Backend,
                Some("codex"),
                "stored reply",
            )
            .unwrap();
        drop(history);
        let mut gateway = Gateway::new(config).unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        gateway
            .tick_fake(vec![message(1, "me@icloud.com", "", true, "hello")])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(
            gateway.ctx.sent_replies.lock().unwrap().as_slice(),
            [(
                "me@icloud.com".to_string(),
                "stored reply\n\n-- sent by push".to_string()
            )]
        );
        assert_eq!(
            gateway
                .ctx
                .history
                .lock()
                .unwrap()
                .outbound_for(inbound_id)
                .unwrap()
                .unwrap()
                .status,
            DeliveryStatus::Delivered
        );
        assert_eq!(gateway.store.lock().unwrap().cursor("imessage"), 1);

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn session_state_save_failure_keeps_reply_for_restart_without_backend_rerun() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("session-save-failure-sessions");
        let assistant_dir = temp_path("session-save-failure-assistant");
        let state_blocker = temp_path("session-save-failure-blocker");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        std::fs::write(&state_blocker, "not a directory").unwrap();
        let first_calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        let store = gateway.store.clone();
        let broken_state_path = state_blocker.join("state.json");
        gateway.ctx.runners = Arc::new(fake_runners_with_hook(
            first_calls.clone(),
            Some(Arc::new(move || {
                store
                    .lock()
                    .unwrap()
                    .set_path_for_test(broken_state_path.clone());
            })),
        ));
        gateway
            .tick_fake(vec![message(1, "me@icloud.com", "", true, "hello")])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

        assert_eq!(first_calls.lock().unwrap().len(), 1);
        assert!(gateway.ctx.sent_replies.lock().unwrap().is_empty());
        let inbound_id = gateway
            .ctx
            .history
            .lock()
            .unwrap()
            .record_inbound(
                "imessage",
                "imessage:self:me@icloud.com",
                "imessage:1",
                "hello",
            )
            .unwrap();
        assert_eq!(
            gateway
                .ctx
                .history
                .lock()
                .unwrap()
                .outbound_for(inbound_id)
                .unwrap()
                .unwrap()
                .status,
            DeliveryStatus::Pending
        );
        drop(gateway);

        let second_calls = Arc::new(Mutex::new(Vec::new()));
        let mut restarted = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        restarted.ctx.runners = Arc::new(fake_runners(second_calls.clone()));
        restarted
            .tick_fake(vec![message(1, "me@icloud.com", "", true, "hello")])
            .await;
        restarted.queues.clear();
        restarted.drain_workers().await;

        assert!(second_calls.lock().unwrap().is_empty());
        assert_eq!(
            restarted.ctx.sent_replies.lock().unwrap().as_slice(),
            [(
                "me@icloud.com".to_string(),
                "fake reply: hello\n\n-- sent by push".to_string()
            )]
        );
        assert_eq!(restarted.store.lock().unwrap().cursor("imessage"), 1);

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_file(state_blocker);
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_delivery_batch_retries_without_blocking_cursor() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("delivery-retry-sessions");
        let assistant_dir = temp_path("delivery-retry-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut gateway = Gateway::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        *gateway.ctx.send_failures_remaining.lock().unwrap() = DELIVERY_ATTEMPTS;
        gateway
            .tick_fake(vec![message(1, "me@icloud.com", "", true, "hello")])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

        assert_eq!(calls.lock().unwrap().len(), 1);
        assert_eq!(gateway.ctx.sent_replies.lock().unwrap().len(), 1);
        assert_eq!(gateway.store.lock().unwrap().cursor("imessage"), 1);
        let inbound_id = gateway
            .ctx
            .history
            .lock()
            .unwrap()
            .record_inbound(
                "imessage",
                "imessage:self:me@icloud.com",
                "imessage:1",
                "hello",
            )
            .unwrap();
        assert_eq!(
            gateway
                .ctx
                .history
                .lock()
                .unwrap()
                .outbound_for(inbound_id)
                .unwrap()
                .unwrap()
                .status,
            DeliveryStatus::Delivered
        );

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.db"));
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
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

        gateway
            .tick_fake(vec![
                telegram_message(10, 8, 8, false, "ignore me"),
                telegram_message(11, 7, 7, false, "hello"),
                telegram_message(12, 7, -100, true, "group"),
            ])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

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
        gateway
            .tick_fake(vec![
                topic_message,
                telegram_message(21, 7, 7, false, "in main"),
            ])
            .await;
        gateway.queues.clear();
        gateway.drain_workers().await;

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

    #[tokio::test(flavor = "current_thread")]
    async fn enabled_channels_process_concurrently_with_isolated_state_and_origin_replies() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("multi-channel-sessions");
        let assistant_dir = temp_path("multi-channel-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut cfg = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.channels = vec!["imessage".to_string(), "telegram".to_string()];
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];
        let mut group = GatewayGroup::new(cfg).unwrap();
        for gateway in &mut group.gateways {
            gateway.ctx.runners = Arc::new(fake_runners(calls.clone()));
        }

        let (imessage, telegram) = group.gateways.split_at_mut(1);
        tokio::join!(
            imessage[0].tick_fake(vec![message(
                5,
                "+15551234567",
                "+15551234567",
                false,
                "from imessage"
            )],),
            telegram[0].tick_fake(vec![telegram_message(5, 7, 7, false, "from telegram")])
        );
        imessage[0].queues.clear();
        telegram[0].queues.clear();
        tokio::join!(imessage[0].drain_workers(), telegram[0].drain_workers());

        let store = imessage[0].store.lock().unwrap();
        assert_eq!(store.cursor("imessage"), 5);
        assert_eq!(store.cursor("telegram"), 5);
        drop(store);
        assert_eq!(
            imessage[0].ctx.sent_replies.lock().unwrap().as_slice(),
            [(
                "+15551234567".to_string(),
                "fake reply: from imessage\n\n-- sent by push".to_string()
            )]
        );
        assert_eq!(
            telegram[0].ctx.sent_replies.lock().unwrap().as_slice(),
            [("7".to_string(), "fake reply: from telegram".to_string())]
        );
        let prompts = calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.prompt.clone())
            .collect::<Vec<_>>();
        assert!(prompts.contains(&"from imessage".to_string()));
        assert!(prompts.contains(&"from telegram".to_string()));
        let persisted = std::fs::read_to_string(&state_path).unwrap();
        assert!(persisted.contains("imessage:dm:+15551234567"));
        assert!(persisted.contains("telegram:dm:7"));

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn primary_delivery_is_scoped_validated_and_non_fatal_when_missing_or_invalid() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("primary-sessions");
        let assistant_dir = temp_path("primary-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let mut cfg = test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        );
        cfg.channels = vec!["imessage".to_string(), "telegram".to_string()];
        cfg.telegram_bot_token = Some("secret".to_string());
        cfg.telegram_allow_user_ids = vec![7];

        let missing = GatewayGroup::new(cfg.clone()).unwrap();
        assert!(missing
            .primary_destination()
            .unwrap_err()
            .to_string()
            .contains("not configured"));

        cfg.primary_delivery = Some(PrimaryDeliveryConfig {
            channel: "telegram".to_string(),
            target: "99".to_string(),
        });
        let invalid = GatewayGroup::new(cfg.clone()).unwrap();
        assert!(invalid
            .primary_destination()
            .unwrap_err()
            .to_string()
            .contains("invalid primary delivery target"));

        cfg.primary_delivery = Some(PrimaryDeliveryConfig {
            channel: "telegram".to_string(),
            target: "7:42".to_string(),
        });
        let valid = GatewayGroup::new(cfg).unwrap();
        let destination = valid.deliver_primary("scheduled result").await.unwrap();
        assert_eq!(
            destination,
            PrimaryDestination {
                channel: "telegram".to_string(),
                target: "7:42".to_string(),
            }
        );
        let telegram = valid
            .gateways
            .iter()
            .find(|gateway| gateway.channel.id() == "telegram")
            .unwrap();
        assert_eq!(
            telegram.ctx.sent_replies.lock().unwrap().as_slice(),
            [("7:42".to_string(), "scheduled result".to_string())]
        );

        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    #[tokio::test]
    async fn one_channel_failure_does_not_stop_another_and_shutdown_reaches_survivor() {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let healthy_started = Arc::new(AtomicBool::new(false));
        let healthy_stopped = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        tasks.spawn(async { anyhow::bail!("imessage rate limited") });
        let started = healthy_started.clone();
        let stopped = healthy_stopped.clone();
        tasks.spawn(async move {
            let mut shutdown = shutdown_rx;
            started.store(true, Ordering::SeqCst);
            shutdown.changed().await.unwrap();
            stopped.store(true, Ordering::SeqCst);
            Ok("telegram")
        });
        let shutdown = async {
            while !healthy_started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            tokio::task::yield_now().await;
        };

        coordinate_channel_tasks(tasks, shutdown_tx, shutdown)
            .await
            .unwrap();

        assert!(healthy_stopped.load(Ordering::SeqCst));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn group_shutdown_waits_for_an_in_flight_channel_worker() {
        let state_path = temp_state_path();
        let sessions_dir = temp_path("group-drain-sessions");
        let assistant_dir = temp_path("group-drain-assistant");
        std::fs::create_dir_all(&assistant_dir).unwrap();
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let started_hook = started.clone();
        let finished_hook = finished.clone();
        let hook = Arc::new(move || {
            started_hook.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(50));
            finished_hook.store(true, Ordering::SeqCst);
        });
        let mut group = GatewayGroup::new(test_config(
            &state_path,
            sessions_dir.to_str().unwrap(),
            assistant_dir.to_str().unwrap(),
        ))
        .unwrap();
        group.gateways[0].ctx.runners = Arc::new(fake_runners_with_hook(
            Arc::new(Mutex::new(Vec::new())),
            Some(hook),
        ));
        group.gateways[0]
            .tick_fake(vec![message(
                1,
                "+15551234567",
                "+15551234567",
                false,
                "finish during shutdown",
            )])
            .await;
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        group.gateways[0]
            .store
            .lock()
            .unwrap()
            .set_cursor("imessage", 1)
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), group.run_until(async {}))
            .await
            .expect("group shutdown should remain bounded")
            .unwrap();

        assert!(finished.load(Ordering::SeqCst));
        let _ = std::fs::remove_file(&state_path);
        let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
        let _ = std::fs::remove_dir_all(sessions_dir);
        let _ = std::fs::remove_dir_all(assistant_dir);
    }

    fn test_config(state_path: &str, sessions_dir: &str, assistant_dir: &str) -> Config {
        Config {
            channel: "imessage".to_string(),
            channels: Vec::new(),
            primary_delivery: None,
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
            permission_profile: "restricted".to_string(),
            job_permission_profiles: vec!["restricted".to_string()],
            permission_profiles: HashMap::new(),
            jobs_dir: format!("{state_path}.jobs"),
            jobs_agent: None,
            jobs_max_timeout: "30m".to_string(),
            jobs_run_dir: format!("{state_path}.run"),
            claude_bin: "claude".to_string(),
            codex_bin: "codex".to_string(),
            codex_model: None,
            sessions_dir: sessions_dir.to_string(),
            state_path: state_path.to_string(),
            audit_log_path: format!("{state_path}.audit.jsonl"),
            database_path: format!("{state_path}.db"),
            audit_log_content: false,
            assistant_dir: assistant_dir.to_string(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }

    pub(crate) fn test_config_for_jobs(
        state_path: &str,
        sessions_dir: &str,
        assistant_dir: &str,
    ) -> Config {
        test_config(state_path, sessions_dir, assistant_dir)
    }

    fn audit_events(path: &str) -> Vec<crate::audit::AuditEvent> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn fake_runners(calls: Arc<Mutex<Vec<FakeRunCall>>>) -> HashMap<AgentBackend, Runner> {
        fake_runners_with_hook(calls, None)
    }

    fn fake_runners_with_hook(
        calls: Arc<Mutex<Vec<FakeRunCall>>>,
        before_return: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> HashMap<AgentBackend, Runner> {
        let mut runners = HashMap::new();
        runners.insert(
            AgentBackend::Codex,
            Runner::Fake(FakeRunner {
                backend: AgentBackend::Codex,
                session_id: "fake-session".to_string(),
                calls,
                before_return,
                resume_missing_once: None,
            }),
        );
        runners
    }

    async fn run_messages(gateway: &mut Gateway, messages: Vec<RawMessage>) {
        gateway.tick_fake(messages).await;
        gateway.queues.clear();
        gateway.drain_workers().await;
    }

    fn approval_question(
        channel: &str,
        thread: &str,
        sender: &str,
        chat: &str,
        target: &str,
    ) -> Question {
        Question::new(
            AnswerOrigin {
                channel: channel.to_string(),
                thread_key: thread.to_string(),
                sender_key: sender.to_string(),
                chat_key: chat.to_string(),
            },
            target,
            "Apply the draft?",
            vec![
                crate::approval::Choice {
                    label: "Approve".to_string(),
                    value: "approve".to_string(),
                },
                crate::approval::Choice {
                    label: "Reject".to_string(),
                    value: "reject".to_string(),
                },
            ],
            now_ms() + 60_000,
        )
        .unwrap()
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
