//! The message loop: poll, filter, and route each message to a per-thread
//! worker task that runs an agent backend and sends the reply.
//!
//! Design: a single loop task owns the queue map and the store. Each thread
//! gets its own task fed by an mpsc channel, which serializes that thread's
//! messages. Workers share only the store, behind a mutex, for tiny bookkeeping
//! writes. Everything else is message passing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::agent::{Request, RunError, Runner};
use crate::claude;
use crate::codex;
use crate::config::AgentBackend;
use crate::config::Config;
use crate::imessage::{Message, Poller, Sender};
use crate::memory;
use crate::store::Store;

const QUEUE_DEPTH: usize = 32;
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

struct Job {
    thread: String,
    target: String,
    text: String,
}

/// Decides which messages to act on. Kept separate so it is trivially testable.
struct Filter {
    self_set: HashSet<String>,
    allow_set: HashSet<String>,
    reply_marker: String,
}

impl Filter {
    /// Returns `(thread_key, reply_target)` for an accepted message.
    fn accept(&self, m: &Message) -> Option<(String, String)> {
        if m.text.trim().is_empty() {
            return None;
        }
        // Never react to our own replies.
        if !self.reply_marker.is_empty() && m.text.contains(&self.reply_marker) {
            return None;
        }
        // Self-chat: you texting yourself.
        if self.self_set.contains(&m.chat_identifier) {
            return Some((
                format!("self:{}", m.chat_identifier),
                m.chat_identifier.clone(),
            ));
        }
        // Inbound DM from an allowed sender.
        if !m.is_from_me && self.allow_set.contains(&m.handle) {
            return Some((format!("dm:{}", m.handle), m.handle.clone()));
        }
        None
    }
}

/// Shared, cheaply cloneable context handed to each worker task.
#[derive(Clone)]
struct Ctx {
    store: Arc<Mutex<Store>>,
    runner: Arc<Runner>,
    sender: Sender,
    run_timeout: Duration,
    reply_marker: String,
    sessions_dir: String,
    assistant_dir: String,
}

pub struct Gateway {
    poller: Poller,
    store: Arc<Mutex<Store>>,
    filter: Filter,
    ctx: Ctx,
    poll_interval: Duration,
    queues: HashMap<String, mpsc::Sender<Job>>,
}

impl Gateway {
    pub fn new(cfg: Config) -> Result<Self> {
        let store = Arc::new(Mutex::new(Store::open(&cfg.state_path)?));
        let runner = Arc::new(match cfg.agent_backend()? {
            AgentBackend::Claude => Runner::Claude(claude::Runner {
                bin: cfg.claude_bin.clone(),
                permission_mode: cfg.claude_permission_mode.clone(),
            }),
            AgentBackend::Codex => Runner::Codex(codex::Runner {
                bin: cfg.codex_bin.clone(),
                sandbox: cfg.codex_sandbox.clone(),
                approval_policy: cfg.codex_approval_policy.clone(),
                model: cfg.codex_model.clone(),
            }),
        });
        let ctx = Ctx {
            store: store.clone(),
            runner,
            sender: Sender::new(),
            run_timeout: cfg.run_timeout_dur()?,
            reply_marker: cfg.reply_marker.clone(),
            sessions_dir: cfg.sessions_dir.clone(),
            assistant_dir: cfg.assistant_dir.clone(),
        };
        let filter = Filter {
            self_set: cfg.self_handles.iter().cloned().collect(),
            allow_set: cfg.allow_from.iter().cloned().collect(),
            reply_marker: cfg.reply_marker.clone(),
        };
        Ok(Self {
            poller: Poller::new(cfg.db_path.clone()),
            store,
            filter,
            ctx,
            poll_interval: cfg.poll_interval_dur()?,
            queues: HashMap::new(),
        })
    }

    /// Runs until SIGINT/SIGTERM, then drains in-flight runs and returns.
    pub async fn run(mut self) -> Result<()> {
        self.skip_backlog().await;

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        let mut ticker = tokio::time::interval(self.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        info!(
            "push gateway running, polling every {:?}",
            self.poll_interval
        );

        loop {
            tokio::select! {
                _ = shutdown_signal() => {
                    info!("signal received, draining in-flight runs");
                    break;
                }
                _ = ticker.tick() => {
                    self.tick(&mut handles).await;
                }
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

    async fn skip_backlog(&self) {
        if self.store.lock().unwrap().last_row() != 0 {
            return;
        }
        let poller = self.poller.clone();
        if let Ok(Ok(max)) = tokio::task::spawn_blocking(move || poller.max_row_id()).await {
            let _ = self.store.lock().unwrap().set_last_row(max);
            info!("starting from ROWID {max} (skipping backlog)");
        }
    }

    async fn tick(&mut self, handles: &mut Vec<JoinHandle<()>>) {
        let since = self.store.lock().unwrap().last_row();
        let poller = self.poller.clone();
        let msgs = match tokio::task::spawn_blocking(move || poller.poll(since)).await {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                error!("poll error: {e}");
                return;
            }
            Err(e) => {
                error!("poll task error: {e}");
                return;
            }
        };

        let mut max_row = 0i64;
        for m in &msgs {
            max_row = max_row.max(m.row_id);
            if let Some((thread, target)) = self.filter.accept(m) {
                self.route(thread, target, m.text.trim().to_string(), handles)
                    .await;
            }
        }
        if max_row > 0 {
            if let Err(e) = self.store.lock().unwrap().set_last_row(max_row) {
                error!("save state error: {e}");
            }
        }
    }

    async fn route(
        &mut self,
        thread: String,
        target: String,
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
            thread: thread.clone(),
            target: target.clone(),
            text,
        };
        let full = matches!(
            self.queues.get(&thread).unwrap().try_send(job),
            Err(mpsc::error::TrySendError::Full(_))
        );
        if full {
            warn!("[{thread}] queue full, asking sender to resend");
            reply_to(
                &self.ctx,
                &target,
                "I'm a bit behind on this thread - resend that in a moment.",
            )
            .await;
        }
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
        reply_to(ctx, &job.target, &reply).await;
        return;
    }

    let initial_session_id = ctx.runner.initial_session_id();
    let (session_id, is_new) = match ctx.store.lock().unwrap().session_for(
        &job.thread,
        ctx.runner.backend(),
        initial_session_id,
    ) {
        Ok(v) => v,
        Err(e) => {
            error!("[{}] session error: {e}", job.thread);
            return;
        }
    };

    let work_dir = sandbox(&ctx.sessions_dir, &job.thread);
    if let Err(e) = std::fs::create_dir_all(&work_dir) {
        error!("[{}] sandbox error: {e}", job.thread);
        return;
    }

    let system = memory::load(&ctx.assistant_dir);

    // Some backends let push choose the session id. Mark those before the run
    // so a post-create failure does not retry the same create call.
    if is_new && ctx.runner.mark_started_before_run() {
        let _ = ctx.store.lock().unwrap().mark_started(&job.thread, None);
    }

    let req = |is_new| Request {
        session_id: &session_id,
        is_new,
        work_dir: &work_dir,
        system_append: &system,
        prompt: &job.text,
    };

    let mut result = ctx.runner.run(req(is_new), ctx.run_timeout).await;
    // If the session id already exists (e.g. left over from a previous run or a
    // different build), resume it instead of trying to create it again.
    if is_new {
        if let Err(RunError::Failed(msg)) = &result {
            if msg.to_lowercase().contains("already in use") {
                warn!("[{}] session id already existed, resuming", job.thread);
                result = ctx.runner.run(req(false), ctx.run_timeout).await;
            }
        }
    }

    match result {
        Ok(out) => {
            if is_new && !ctx.runner.mark_started_before_run() {
                if let Err(e) = ctx
                    .store
                    .lock()
                    .unwrap()
                    .mark_started(&job.thread, out.session_id.as_deref())
                {
                    error!("[{}] session save error: {e}", job.thread);
                    return;
                }
            }
            reply_to(ctx, &job.target, &out.reply).await;
        }
        Err(RunError::Timeout) => {
            warn!("[{}] {} run timed out", job.thread, ctx.runner.label());
            reply_to(
                ctx,
                &job.target,
                "That took too long and was stopped. Try again or simplify the request.",
            )
            .await;
        }
        Err(RunError::Failed(msg)) => {
            error!("[{}] {} error: {msg}", job.thread, ctx.runner.label());
            reply_to(ctx, &job.target, &format!("⚠️ {}", short(&msg))).await;
        }
    }
}

/// Handles gateway-level slash commands before anything reaches the agent.
fn command(ctx: &Ctx, job: &Job) -> Option<String> {
    match job.text.trim().to_lowercase().as_str() {
        "/clear" | "/new" | "/reset" => match ctx.store.lock().unwrap().rotate(
            &job.thread,
            ctx.runner.backend(),
            ctx.runner.initial_session_id(),
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

async fn reply_to(ctx: &Ctx, target: &str, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let out = format!("{text}{}", ctx.reply_marker);
    match tokio::time::timeout(SEND_TIMEOUT, ctx.sender.send(target, &out)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => error!("send error to {target}: {e}"),
        Err(_) => error!("send to {target} timed out"),
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
    format!("{sessions_dir}/{}", sanitize(thread))
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

    fn filter() -> Filter {
        Filter {
            self_set: ["me@icloud.com".to_string()].into_iter().collect(),
            allow_set: ["+15551234567".to_string()].into_iter().collect(),
            reply_marker: "\n\n-- sent by push".to_string(),
        }
    }

    fn msg(chat: &str, handle: &str, from_me: bool, text: &str) -> Message {
        Message {
            row_id: 1,
            handle: handle.to_string(),
            chat_identifier: chat.to_string(),
            text: text.to_string(),
            is_from_me: from_me,
        }
    }

    #[test]
    fn self_chat_accepted() {
        let got = filter().accept(&msg("me@icloud.com", "", true, "hi"));
        assert_eq!(
            got,
            Some((
                "self:me@icloud.com".to_string(),
                "me@icloud.com".to_string()
            ))
        );
    }

    #[test]
    fn allowlisted_dm_accepted() {
        let got = filter().accept(&msg("+15551234567", "+15551234567", false, "hi"));
        assert_eq!(
            got,
            Some(("dm:+15551234567".to_string(), "+15551234567".to_string()))
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
}
