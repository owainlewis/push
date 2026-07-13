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

use crate::agent::Runner;
use crate::approval::{AnswerOrigin, AnswerOutcome};
use crate::audit::{AuditEvent, AuditLog};
use crate::channel::{Channel, RawMessage};
use crate::config::{AgentBackend, ChannelKind, Config, PermissionProfile, PrimaryDeliveryConfig};
use crate::history::{History, OutboundOrigin};
use crate::jobs;
use crate::store::Store;
use crate::util::now_ms;

const QUEUE_DEPTH: usize = 32;
#[cfg(not(test))]
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Job {
    row_id: i64,
    inbound_id: i64,
    thread: String,
    target: String,
    backend: AgentBackend,
    permission: PermissionProfile,
    approval_origin: AnswerOrigin,
    text: String,
}

/// Shared, cheaply cloneable context handed to each worker task.
#[derive(Clone)]
struct Ctx {
    cfg: Config,
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
    cfg: Config,
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
            primary_delivery: cfg.primary_delivery.clone(),
            cfg,
        })
    }

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

    #[cfg(test)]
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

    pub async fn run(self) -> Result<()> {
        self.run_until(shutdown_signal()).await
    }

    async fn run_until<S>(self, shutdown: S) -> Result<()>
    where
        S: Future<Output = ()>,
    {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let scheduler = match self.primary_destination() {
            Ok(destination) => {
                jobs::Scheduler::new(self.cfg.clone(), destination.channel, destination.target)
            }
            Err(error) => {
                warn!("scheduled jobs disabled: {error:#}");
                jobs::Scheduler::delivery_only(self.cfg.clone())
            }
        };
        let contexts = self
            .gateways
            .iter()
            .map(|gateway| (gateway.channel.id().to_string(), gateway.ctx.clone()))
            .collect::<HashMap<_, _>>();
        let receiver = shutdown_rx.clone();
        let scheduler =
            tokio::spawn(async move { run_scheduler(scheduler, contexts, receiver).await });
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
        let result = coordinate_channel_tasks(tasks, shutdown_tx, shutdown).await;
        match scheduler.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!("job scheduler stopped: {error:#}"),
            Err(error) => error!("job scheduler task failed: {error}"),
        }
        result
    }
}

async fn run_scheduler(
    mut scheduler: jobs::Scheduler,
    contexts: HashMap<String, Ctx>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let contexts = Arc::new(contexts);
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            _ = ticker.tick() => {
                let contexts = contexts.clone();
                if let Err(error) = scheduler.tick(now_ms(), move |channel, target, text| {
                    let contexts = contexts.clone();
                    async move {
                        let ctx = contexts.get(&channel).with_context(|| {
                            format!("persisted delivery channel {channel:?} is not enabled")
                        })?;
                        let target = ctx.channel.primary_target(&target).context(
                            "persisted delivery target is no longer allowlisted",
                        )?;
                        if reply_to(ctx, &target, &text).await {
                            Ok(())
                        } else {
                            anyhow::bail!("delivery to {channel} target {target:?} failed")
                        }
                    }
                }).await {
                    error!("job scheduler tick failed: {error:#}");
                }
            }
        }
    }
    scheduler.shutdown().await;
    Ok(())
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
        crate::drafts::prepare(&cfg).context("prepare agent job draft boundary")?;
        let ack = Arc::new(Mutex::new(AckState::default()));
        let channel = Channel::new_for(&cfg, kind)?;
        let audit = Arc::new(AuditLog::with_lock(
            cfg.audit_log_path.clone(),
            cfg.audit_log_content,
            channel.id(),
            audit_lock,
        ));
        let ctx = Ctx {
            cfg: cfg.clone(),
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

    /// Delivers a durable approval question. Test-only seeding for the active
    /// inbound answer-resolution flow; nothing in production asks yet.
    #[cfg(test)]
    pub async fn ask_user(&self, question: crate::approval::Question) -> Result<String> {
        use crate::approval::DeliveryStatus as ApprovalDeliveryStatus;

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
        for m in &msgs {
            if m.row_id <= since {
                continue;
            }
            if self.ack.lock().unwrap().is_known(m.row_id) {
                continue;
            }
            if let Some((thread, target)) = self.channel.accept(m) {
                let approval_origin = approval_origin(m, &thread);
                let approval = self.ctx.history.lock().unwrap().answer_question(
                    &approval_origin,
                    m.text.trim(),
                    now_ms(),
                );
                match approval {
                    Ok(AnswerOutcome::NotAnAnswer) => {}
                    Ok(outcome @ (AnswerOutcome::Selected(_) | AnswerOutcome::Duplicate(_))) => {
                        self.audit_approval(m.row_id, &thread, &outcome);
                        let question_id = match &outcome {
                            AnswerOutcome::Selected(answer) => &answer.correlation_id,
                            AnswerOutcome::Duplicate(id) => id,
                            _ => unreachable!(),
                        };
                        if let Err(error) = self
                            .handle_draft_answer(question_id, &approval_origin, &target)
                            .await
                        {
                            error!("[{thread}] draft approval failed: {error:#}");
                            self.audit(self.ctx.audit.failed(
                                "draft_approval_failed",
                                m.row_id,
                                &thread,
                                None,
                                error.to_string(),
                            ));
                            return;
                        }
                        self.complete_row(m.row_id, "approval_answer");
                        continue;
                    }
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
                    approval_origin,
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
    }

    async fn route(&mut self, job: Job) -> bool {
        let row_id = job.row_id;
        let thread = job.thread.clone();
        let target = job.target.clone();
        let backend = job.backend;
        if !self.queues.contains_key(&thread) {
            let (tx, rx) = mpsc::channel::<Job>(QUEUE_DEPTH);
            let ctx = self.ctx.clone();
            self.handles.push(tokio::spawn(worker::run(ctx, rx)));
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
            match worker::record_and_deliver(&self.ctx, &job, OutboundOrigin::Gateway, reply).await
            {
                Ok(
                    worker::DeliveryOutcome::Delivered | worker::DeliveryOutcome::AlreadyDelivered,
                ) => {
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

    async fn handle_draft_answer(
        &self,
        question_id: &str,
        origin: &AnswerOrigin,
        target: &str,
    ) -> Result<()> {
        let approved_by = format!(
            "channel={} thread={} sender={} chat={}",
            origin.channel, origin.thread_key, origin.sender_key, origin.chat_key
        );
        let outcome = crate::drafts::decide(
            &self.cfg,
            &mut self.ctx.history.lock().unwrap(),
            question_id,
            &approved_by,
            now_ms(),
        )?;
        let message = match outcome {
            crate::drafts::DecisionOutcome::Installed(name) => {
                Some(format!("Installed approved job `{name}`."))
            }
            crate::drafts::DecisionOutcome::Rejected(name) => Some(format!(
                "Rejected job draft `{name}`. It remains inactive in drafts."
            )),
            crate::drafts::DecisionOutcome::Invalidated(message) => {
                Some(format!("Draft approval was invalidated: {message}"))
            }
            crate::drafts::DecisionOutcome::Failed(message) => {
                Some(format!("Draft approval failed safely: {message}"))
            }
            crate::drafts::DecisionOutcome::AlreadyHandled => None,
        };
        if let Some(message) = message {
            if !reply_to(&self.ctx, target, &message).await {
                warn!("draft decision reply delivery failed for question {question_id}");
            }
        }
        Ok(())
    }
}

fn audit(ctx: &Ctx, event: AuditEvent) {
    if let Err(e) = ctx.audit.record(event) {
        error!("audit log error: {e}");
    }
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
    [AgentBackend::Claude, AgentBackend::Codex, AgentBackend::Pi]
        .into_iter()
        .map(|backend| (backend, Runner::for_backend(backend, cfg)))
        .collect()
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

mod worker;

#[cfg(test)]
pub(crate) mod tests;
