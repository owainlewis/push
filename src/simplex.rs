//! SimpleX Chat client using an externally-managed `simplex-chat` daemon's
//! WebSocket JSON-RPC API (started separately, e.g.
//! `simplex-chat -d <dir> -p 5225`). Push does not spawn or supervise this
//! process — see `.planning/2026-07-18-simplex-channel.md` for why.
//!
//! `dispatch()` (from `simploxide_client`) owns a blocking event loop, so it
//! runs in its own spawned task. Inbound messages are pushed into a bounded
//! mpsc channel; `poll()` (required by `ChannelContract`, which is `&self`)
//! drains it non-blockingly. A `Disconnected` receive is how a dead pump
//! task surfaces as an error to the caller — no separate error-state channel.
//!
//! The `ChannelContract` trait itself is private to `crate::channel`, so
//! `impl ChannelContract for Simplex` lives there (mirroring how the
//! existing `Telegram` and `IMessageChannel` impls are also defined in
//! `channel.rs` rather than `telegram.rs`/`imessage/`) — this module only
//! owns connection setup and the event-pump bridge.
//!
//! ## Daemon profile requirement (important, found via live testing)
//!
//! `simplex-chat` requires an existing user profile before its WS port will
//! accept any command at all — even `-m/--maintenance` mode exits
//! immediately with "no active user" on a truly empty `-d` directory. So the
//! daemon's *first* profile must be created before Push ever connects, e.g.:
//!
//! ```sh
//! simplex-chat -d <dir> -p 5225 -y --create-bot-display-name push
//! ```
//!
//! `Bot::init` (called from `connect()` below) looks up a profile by exact
//! display-name match and reuses it if found; if the name doesn't match
//! anything on the daemon, it creates a *second*, distinct user instead.
//! `simploxide`'s own event stream is then scoped via `set_owner(bot.user_id())`
//! — so a display-name mismatch means Push's connection silently receives
//! events for the wrong (empty) user while the daemon's original identity is
//! the one actually holding the real conversation. This was caught live: a
//! mismatched name (`push-test-daemon` pre-created vs. `push` requested)
//! reproduced intermittent-to-silent message delivery; matching them
//! (`--create-bot-display-name push`, matching the default below) reproduced
//! a full, reliable round trip through to a real agent reply every time.
//! **The pre-created daemon profile's display name MUST exactly match
//! `simplex.display_name` in Push's config (default `"push"`).**

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use simploxide_client::prelude::*;
use simploxide_client::ws;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;

use crate::channel::RawMessage;

/// Bounded so a stalled/slow poller cannot let the pump task grow memory
/// without limit. `try_send` in the handler drops (with a log warning)
/// rather than blocking the pump on `Full`.
const INBOUND_QUEUE_CAPACITY: usize = 256;

#[derive(Clone)]
struct SimplexCtx {
    tx: mpsc::Sender<RawMessage>,
    next_row_id: Arc<AtomicI64>,
}

pub struct Simplex {
    pub(crate) bot: ws::Bot,
    allow_contact_ids: HashSet<i64>,
    pub(crate) inbound: Mutex<mpsc::Receiver<RawMessage>>,
    pub(crate) next_row_id: Arc<AtomicI64>,
    pub(crate) pump: JoinHandle<()>,
}

impl Simplex {
    /// Connects to an already-running `simplex-chat` daemon on
    /// `127.0.0.1:<port>` and starts the background event pump. Does not
    /// spawn/manage the daemon (see the module doc comment).
    pub async fn connect(
        port: u16,
        display_name: &str,
        allow_contact_ids: Vec<i64>,
    ) -> Result<Self> {
        // `.auto_accept()` accepts incoming SimpleX connection requests at
        // the protocol/transport level for anyone holding the bot's
        // address — without it, `ReceivedContactRequest` events are never
        // acted on and NOBODY can ever establish a connection at all
        // (caught live: a real connect attempt hung waiting for
        // `contactConnected`, which never fires without this). This does
        // NOT mean anyone can talk to the agent — `accept()`/`allow_contact_ids`
        // below is the separate, strict application-level gate on whose
        // *messages* actually get processed once connected. Same two-layer
        // shape as claude-simplex-channel's connect-then-gate design
        // reviewed earlier this session.
        let (bot, events) = ws::BotBuilder::new(display_name, port)
            .auto_accept()
            .connect()
            .await
            .with_context(|| format!("connect to SimpleX daemon at 127.0.0.1:{port}"))?;

        // Without this, the bot has a live daemon connection but no invite
        // link — nothing could ever discover/message it. Logged at INFO
        // (not just on first run) so restarting the process doesn't require
        // digging through daemon state to find the address again.
        let address = bot
            .get_or_create_address()
            .await
            .context("create or fetch SimpleX bot address")?;
        tracing::info!(%address, "SimpleX bot address ready");

        let (tx, rx) = mpsc::channel(INBOUND_QUEUE_CAPACITY);
        let next_row_id = Arc::new(AtomicI64::new(0));
        let ctx = SimplexCtx {
            tx,
            next_row_id: next_row_id.clone(),
        };

        let pump = tokio::spawn(async move {
            if let Err(err) = events.into_dispatcher(ctx).on(handle_new_chat_items).dispatch().await
            {
                tracing::error!(error = %err, "SimpleX event dispatcher exited");
            }
            // The `tx` inside `ctx` (moved into the dispatcher/handler chain)
            // is dropped here when the async block ends, whether via error
            // or a StreamEvents::Break we never actually return. That drop
            // is what turns `rx.try_recv()` into `Disconnected` for poll().
        });

        Ok(Self {
            bot,
            allow_contact_ids: allow_contact_ids.into_iter().collect(),
            inbound: Mutex::new(rx),
            next_row_id,
            pump,
        })
    }

    pub(crate) fn is_allowed(&self, contact_id: i64) -> bool {
        self.allow_contact_ids.contains(&contact_id)
    }
}

async fn handle_new_chat_items(
    ev: Arc<NewChatItems>,
    ctx: SimplexCtx,
) -> ws::ClientResult<StreamEvents> {
    for (chat_id, item, content) in ev.chat_items.filter_messages() {
        let Some(text) = content.text() else {
            continue;
        };
        let is_group = matches!(chat_id, ChatId::Group { .. });
        let contact_id = match chat_id {
            ChatId::Direct(id) => id.raw(),
            // Groups are out of scope for v1 (see contract §F); still
            // forward a RawMessage so `common_reject_reason`'s existing
            // `is_group` check rejects it uniformly instead of silently
            // dropping it here (silently dropping would hide a
            // misconfiguration from the operator's reject-reason logs).
            ChatId::Group { id, .. } => id.raw(),
            ChatId::Local(id) => id.raw(),
        };

        // Cursor assigned at enqueue time, not drain time, so `poll(since)`
        // filtering can't race a message that arrives between two poll calls.
        let row_id = ctx.next_row_id.fetch_add(1, Ordering::SeqCst);

        let message = RawMessage {
            row_id,
            channel: "simplex",
            handle: contact_id.to_string(),
            chat_identifier: contact_id.to_string(),
            is_group,
            text: text.clone(),
            voice: None,
            is_from_me: false, // filter_messages() only yields received content
            is_supported: true,
            thread_id: None,
        };

        if ctx.tx.try_send(message).is_err() {
            tracing::warn!(
                contact_id,
                "SimpleX inbound queue full or closed; dropping message"
            );
        }

        let _ = item; // reply-to target not used for v1 (no per-message threading)
    }

    Ok(StreamEvents::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn poll_returns_err_once_the_pump_channel_is_disconnected() {
        // Mirrors the real Simplex struct's inbound-drain logic without a
        // live daemon: a dropped Sender must surface as an error from the
        // next drain, exactly as it would from a dead pump task.
        let (tx, mut rx) = mpsc::channel::<RawMessage>(4);
        drop(tx);
        let result: std::result::Result<Vec<RawMessage>, ()> = 'drain: {
            let mut out = Vec::new();
            loop {
                match rx.try_recv() {
                    Ok(m) => out.push(m),
                    Err(mpsc::error::TryRecvError::Empty) => break 'drain Ok(out),
                    Err(mpsc::error::TryRecvError::Disconnected) => break 'drain Err(()),
                }
            }
        };
        assert!(result.is_err());
    }
}
