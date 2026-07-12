use super::worker::{
    handle, present_drafts, run_with_periodic_activity, sandbox, DELIVERY_ATTEMPTS,
    SANDBOX_SETUP_FAILURE, SESSION_SETUP_FAILURE,
};
use super::*;
use crate::agent::{FakeRunCall, FakeRunner};
use crate::approval::Question;
use crate::channel::{normalize_handle, thread_handle};
use crate::history::DeliveryStatus;
use crate::imessage::{Poller, Sender};
use std::path::{Path, PathBuf};
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
        Runner::Claude(crate::claude::Runner {
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
        cfg: test_config(
            &temp_path("setup-failure-state").to_string_lossy(),
            &sessions_dir,
            "",
        ),
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
        approval_origin: AnswerOrigin {
            channel: "imessage".to_string(),
            thread_key: "imessage:self:me".to_string(),
            sender_key: "me@icloud.com".to_string(),
            chat_key: "me@icloud.com".to_string(),
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

    complete_row(&store, &ack, "imessage", 10);

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
            failure: None,
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
            failure: None,
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
            failure: None,
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
async fn missing_primary_disables_new_schedules_without_stopping_gateway() {
    let state_path = temp_state_path();
    let sessions_dir = temp_path("missing-primary-sessions");
    let assistant_dir = temp_path("missing-primary-assistant");
    let jobs_dir = temp_path("missing-primary-jobs");
    let workdir = temp_path("missing-primary-work");
    std::fs::create_dir_all(&assistant_dir).unwrap();
    std::fs::create_dir_all(&jobs_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let mut cfg = test_config(
        &state_path,
        sessions_dir.to_str().unwrap(),
        assistant_dir.to_str().unwrap(),
    );
    cfg.jobs_dir = jobs_dir.to_string_lossy().to_string();
    std::fs::write(
        jobs_dir.join("disabled.md"),
        format!(
            "+++\nversion = 1\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n\n[[triggers]]\nid = \"minute\"\nkind = \"cron\"\nschedule = \"* * * * *\"\ntimezone = \"UTC\"\nenabled = true\n+++\n\nRun.\n",
            workdir.to_string_lossy()
        ),
    )
    .unwrap();
    let group = GatewayGroup::new(cfg.clone()).unwrap();
    group.gateways[0]
        .store
        .lock()
        .unwrap()
        .set_cursor("imessage", 1)
        .unwrap();

    group
        .run_until(tokio::time::sleep(Duration::from_millis(50)))
        .await
        .unwrap();

    let rows = crate::jobs::Ledger::open(&cfg.database_path)
        .unwrap()
        .runs(Some("disabled"))
        .unwrap();
    assert!(rows.is_empty());
    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_file(format!("{state_path}.db"));
    let _ = std::fs::remove_file(format!("{state_path}.audit.jsonl"));
    let _ = std::fs::remove_dir_all(sessions_dir);
    let _ = std::fs::remove_dir_all(assistant_dir);
    let _ = std::fs::remove_dir_all(jobs_dir);
    let _ = std::fs::remove_dir_all(workdir);
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

#[tokio::test]
async fn route_agent_draft_requires_bound_approval_before_atomic_install() {
    let state_path = temp_state_path();
    let sessions_dir = temp_path("draft-e2e-sessions");
    let assistant_dir = temp_path("draft-e2e-assistant");
    let workdir = temp_path("draft-e2e-workdir");
    std::fs::create_dir_all(&assistant_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let mut cfg = test_config(
        &state_path,
        sessions_dir.to_str().unwrap(),
        assistant_dir.to_str().unwrap(),
    );
    cfg.permission_profile = "workspace".to_string();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut gateway = Gateway::new(cfg.clone()).unwrap();
    let inbound = message(
        1,
        "+15551234567",
        "+15551234567",
        false,
        "Draft a morning job",
    );
    let (thread, _) = gateway.channel.accept(&inbound).unwrap();
    let draft_directory =
        crate::drafts::origin_directory(&cfg, &approval_origin(&inbound, &thread)).unwrap();
    let body = format!(
        "+++\nversion = 1\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n+++\n\nPrepare a note.\n",
        workdir.to_string_lossy()
    );
    let hook = Arc::new(move || {
        std::fs::write(draft_directory.join("agent-note.md"), &body).unwrap();
    });
    gateway.ctx.runners = Arc::new(fake_runners_with_hook(calls.clone(), Some(hook)));

    run_messages(&mut gateway, vec![inbound]).await;
    assert!(!Path::new(&cfg.jobs_dir).join("agent-note.md").exists());
    let replies = gateway.ctx.sent_replies.lock().unwrap().clone();
    assert!(replies
        .iter()
        .any(|(_, text)| text.contains("Prepare a note.")));
    let question_text = replies
        .iter()
        .find_map(|(_, text)| text.contains("Approve and install").then_some(text))
        .unwrap();
    let question_id = question_text
        .split_whitespace()
        .map(|part| part.trim_matches(|ch| ch == '`' || ch == ',' || ch == '.'))
        .find(|part| uuid::Uuid::parse_str(part).is_ok())
        .unwrap()
        .to_string();

    run_messages(
        &mut gateway,
        vec![message(
            2,
            "+15551234567",
            "+15551234567",
            false,
            &format!("{question_id} 1"),
        )],
    )
    .await;

    assert!(Path::new(&cfg.jobs_dir).join("agent-note.md").is_file());
    assert_eq!(calls.lock().unwrap().len(), 1);
    let proposal = gateway
        .ctx
        .history
        .lock()
        .unwrap()
        .draft_proposal(&question_id)
        .unwrap()
        .unwrap();
    assert_eq!(proposal.status, "installed");
    assert!(proposal.proposed_by.contains("sender=15551234567"));
    assert!(proposal
        .approved_by
        .as_deref()
        .unwrap()
        .contains("sender=15551234567"));
}

#[tokio::test]
async fn concurrent_origins_present_only_their_isolated_drafts() {
    let state_path = temp_state_path();
    let sessions_dir = temp_path("draft-concurrent-sessions");
    let assistant_dir = temp_path("draft-concurrent-assistant");
    let workdir = temp_path("draft-concurrent-workdir");
    std::fs::create_dir_all(&assistant_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let mut cfg = test_config(
        &state_path,
        sessions_dir.to_str().unwrap(),
        assistant_dir.to_str().unwrap(),
    );
    cfg.permission_profile = "workspace".to_string();
    let gateway = Gateway::new(cfg.clone()).unwrap();
    let first_origin = AnswerOrigin {
        channel: "imessage".to_string(),
        thread_key: "imessage:dm:111".to_string(),
        sender_key: "111".to_string(),
        chat_key: "111".to_string(),
    };
    let second_origin = AnswerOrigin {
        channel: "imessage".to_string(),
        thread_key: "imessage:dm:222".to_string(),
        sender_key: "222".to_string(),
        chat_key: "222".to_string(),
    };
    let first_dir = crate::drafts::origin_directory(&cfg, &first_origin).unwrap();
    let second_dir = crate::drafts::origin_directory(&cfg, &second_origin).unwrap();
    assert_ne!(first_dir, second_dir);
    std::fs::write(
        first_dir.join("first.md"),
        draft_runbook(&workdir, "FIRST ONLY"),
    )
    .unwrap();
    std::fs::write(
        second_dir.join("second.md"),
        draft_runbook(&workdir, "SECOND ONLY"),
    )
    .unwrap();
    let first = draft_test_job(1, "111", first_origin);
    let second = draft_test_job(2, "222", second_origin);

    let (first_result, second_result) = tokio::join!(
        present_drafts(&gateway.ctx, &first, &first_dir),
        present_drafts(&gateway.ctx, &second, &second_dir),
    );
    first_result.unwrap();
    second_result.unwrap();

    let replies = gateway.ctx.sent_replies.lock().unwrap();
    let first_replies = replies
        .iter()
        .filter(|(target, _)| target == "111")
        .map(|(_, text)| text)
        .collect::<Vec<_>>();
    let second_replies = replies
        .iter()
        .filter(|(target, _)| target == "222")
        .map(|(_, text)| text)
        .collect::<Vec<_>>();
    assert!(first_replies.iter().any(|text| text.contains("FIRST ONLY")));
    assert!(!first_replies
        .iter()
        .any(|text| text.contains("SECOND ONLY")));
    assert!(second_replies
        .iter()
        .any(|text| text.contains("SECOND ONLY")));
    assert!(!second_replies
        .iter()
        .any(|text| text.contains("FIRST ONLY")));
}

#[tokio::test]
async fn failed_run_and_recovered_outbound_still_reconcile_origin_drafts() {
    let state_path = temp_state_path();
    let sessions_dir = temp_path("draft-failure-sessions");
    let assistant_dir = temp_path("draft-failure-assistant");
    let workdir = temp_path("draft-failure-workdir");
    std::fs::create_dir_all(&assistant_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let mut cfg = test_config(
        &state_path,
        sessions_dir.to_str().unwrap(),
        assistant_dir.to_str().unwrap(),
    );
    cfg.permission_profile = "workspace".to_string();
    let mut gateway = Gateway::new(cfg.clone()).unwrap();
    let failed_message = message(1, "+15551234567", "+15551234567", false, "draft then fail");
    let (thread, _) = gateway.channel.accept(&failed_message).unwrap();
    let directory =
        crate::drafts::origin_directory(&cfg, &approval_origin(&failed_message, &thread)).unwrap();
    let failed_body = draft_runbook(&workdir, "CREATED BEFORE FAILURE");
    let hook = Arc::new(move || {
        std::fs::write(directory.join("failed-run.md"), &failed_body).unwrap();
    });
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runners = HashMap::new();
    runners.insert(
        AgentBackend::Codex,
        Runner::Fake(FakeRunner {
            backend: AgentBackend::Codex,
            session_id: "failed-session".to_string(),
            calls: calls.clone(),
            before_return: Some(hook),
            failure: Some("boom".to_string()),
            resume_missing_once: None,
        }),
    );
    gateway.ctx.runners = Arc::new(runners);

    run_messages(&mut gateway, vec![failed_message]).await;
    let replies = gateway.ctx.sent_replies.lock().unwrap().clone();
    assert!(replies
        .iter()
        .any(|(_, text)| text.contains("CREATED BEFORE FAILURE")));
    assert!(replies.iter().any(|(_, text)| text.contains("boom")));
    assert_eq!(calls.lock().unwrap().len(), 1);

    let recovered_message = message(2, "+15551234567", "+15551234567", false, "crash window");
    let (recovered_thread, _) = gateway.channel.accept(&recovered_message).unwrap();
    let recovered_dir = crate::drafts::origin_directory(
        &cfg,
        &approval_origin(&recovered_message, &recovered_thread),
    )
    .unwrap();
    std::fs::write(
        recovered_dir.join("recovered.md"),
        draft_runbook(&workdir, "RECOVERED AFTER CRASH"),
    )
    .unwrap();
    let inbound_id = gateway
        .ctx
        .history
        .lock()
        .unwrap()
        .record_inbound(
            recovered_message.channel,
            &recovered_thread,
            &recovered_message.event_id(),
            recovered_message.text.trim(),
        )
        .unwrap();
    gateway
        .ctx
        .history
        .lock()
        .unwrap()
        .record_outbound(
            inbound_id,
            OutboundOrigin::Backend,
            Some("codex"),
            "stored before crash",
        )
        .unwrap();
    run_messages(&mut gateway, vec![recovered_message]).await;
    let replies = gateway.ctx.sent_replies.lock().unwrap();
    assert!(replies
        .iter()
        .any(|(_, text)| text.contains("RECOVERED AFTER CRASH")));
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_draft_delivery_retries_after_restart_before_expiry() {
    let state_path = temp_state_path();
    let sessions_dir = temp_path("draft-delivery-sessions");
    let assistant_dir = temp_path("draft-delivery-assistant");
    let workdir = temp_path("draft-delivery-workdir");
    std::fs::create_dir_all(&assistant_dir).unwrap();
    std::fs::create_dir_all(&workdir).unwrap();
    let mut cfg = test_config(
        &state_path,
        sessions_dir.to_str().unwrap(),
        assistant_dir.to_str().unwrap(),
    );
    cfg.permission_profile = "workspace".to_string();
    let origin = AnswerOrigin {
        channel: "imessage".to_string(),
        thread_key: "imessage:dm:15551234567".to_string(),
        sender_key: "15551234567".to_string(),
        chat_key: "15551234567".to_string(),
    };
    let directory = crate::drafts::origin_directory(&cfg, &origin).unwrap();
    std::fs::write(
        directory.join("retry.md"),
        draft_runbook(&workdir, "RETRY THIS OFFER"),
    )
    .unwrap();
    let job = draft_test_job(1, "+15551234567", origin);
    let gateway = Gateway::new(cfg.clone()).unwrap();
    *gateway.ctx.send_failures_remaining.lock().unwrap() = DELIVERY_ATTEMPTS;

    assert!(present_drafts(&gateway.ctx, &job, &directory)
        .await
        .unwrap_err()
        .to_string()
        .contains("retry before expiry"));
    drop(gateway);

    let restarted = Gateway::new(cfg).unwrap();
    present_drafts(&restarted.ctx, &job, &directory)
        .await
        .unwrap();
    let replies = restarted.ctx.sent_replies.lock().unwrap();
    assert!(replies
        .iter()
        .any(|(_, text)| text.contains("RETRY THIS OFFER")));
    assert!(replies
        .iter()
        .any(|(_, text)| text.contains("Approve and install")));
}

fn draft_runbook(workdir: &Path, instruction: &str) -> String {
    format!(
        "+++\nversion = 1\ntimeout = \"5s\"\nworkdir = {:?}\nbackend = \"codex\"\n+++\n\n{instruction}\n",
        workdir.to_string_lossy()
    )
}

fn draft_test_job(row_id: i64, target: &str, origin: AnswerOrigin) -> Job {
    Job {
        row_id,
        inbound_id: row_id,
        thread: origin.thread_key.clone(),
        target: target.to_string(),
        backend: AgentBackend::Codex,
        permission: PermissionProfile {
            name: "workspace".to_string(),
            capability: crate::config::PermissionCapability::Workspace,
        },
        approval_origin: origin,
        text: "draft".to_string(),
    }
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
        permission_profiles: HashMap::new(),
        jobs_dir: format!("{state_path}.jobs"),
        drafts_dir: format!("{state_path}.drafts"),
        jobs_agent: None,
        jobs_max_timeout: "30m".to_string(),
        jobs_run_dir: format!("{state_path}.run"),
        jobs_max_workers: 2,
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
            failure: None,
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
