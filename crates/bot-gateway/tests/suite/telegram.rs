//! Telegram Bot API gateway tests: startup checks, DM intake, 409 conflict abort, batch ordering, approvals, and delivery.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use bot_core::{
    id::ProfileId,
    intake::{DeliveryScope, PushTransportId},
    ledger::ApprovalState,
    port::Error,
    profile::Adapter,
    state::{Conversation, ConversationKind},
};
use bot_gateway::{
    delivery::{Deliverer, Sender},
    host::HostConfig,
    ingress::Ingress,
    telegram::{TelegramGateway, api::TelegramApi},
    testing::{
        MemoryPrivate, MemorySecrets, MemoryStore, TestCatalog,
        ports::{MemoryBroker, MemoryLedger, MemoryOutbox, outgoing},
        profile,
    },
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const BOT_ID: u64 = 100_200_300;
const BOT_TOKEN: &str = "100200300:SYNTHETIC_TEST_TOKEN_SECRET_XYZ";
const USER_ID: u64 = 42_000_001;

#[derive(Clone, Debug)]
struct RecordedCall {
    method: String,
    body_text: String,
    body_json: Option<Value>,
}

struct FakeState {
    calls: Mutex<Vec<RecordedCall>>,
    updates: Mutex<VecDeque<(StatusCode, Value)>>,
    get_me_id: Mutex<u64>,
    next_msg_id: Mutex<u64>,
    fail_next_edit: Mutex<bool>,
}

struct FakeTelegram {
    base: String,
    state: Arc<FakeState>,
}

impl FakeTelegram {
    async fn start() -> Self {
        let state = Arc::new(FakeState {
            calls: Mutex::new(Vec::new()),
            updates: Mutex::new(VecDeque::new()),
            get_me_id: Mutex::new(BOT_ID),
            next_msg_id: Mutex::new(10),
            fail_next_edit: Mutex::new(false),
        });
        let app = Router::new()
            .route("/{bot}/{method}", post(handle_tg))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}/"),
            state,
        }
    }

    fn push_updates(&self, status: StatusCode, body: Value) {
        self.state.updates.lock().unwrap().push_back((status, body));
    }

    fn calls(&self, method: &str) -> Vec<RecordedCall> {
        self.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.method == method)
            .cloned()
            .collect()
    }
}

async fn handle_tg(
    State(state): State<Arc<FakeState>>,
    Path((_bot, method)): Path<(String, String)>,
    bytes: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let body_text = String::from_utf8_lossy(&bytes).into_owned();
    let body_json = serde_json::from_slice::<Value>(&bytes).ok();
    state.calls.lock().unwrap().push(RecordedCall {
        method: method.clone(),
        body_text,
        body_json: body_json.clone(),
    });
    match method.as_str() {
        "getMe" => {
            let id = *state.get_me_id.lock().unwrap();
            (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "result": {"id": id, "is_bot": true, "username": "test_bot"}}),
                ),
            )
        }
        "getUpdates" => state.updates.lock().unwrap().pop_front().map_or_else(
            || (StatusCode::OK, Json(json!({"ok": true, "result": []}))),
            |(s, v)| (s, Json(v)),
        ),
        "sendMessage" | "sendDocument" => {
            let mut next = state.next_msg_id.lock().unwrap();
            *next += 1;
            let chat_id = body_json
                .as_ref()
                .and_then(|v| v["chat_id"].as_u64())
                .unwrap_or(USER_ID);
            (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "result": {"message_id": *next, "chat": {"id": chat_id, "type": "private"}}}),
                ),
            )
        }
        "editMessageText" => {
            let mut fail = state.fail_next_edit.lock().unwrap();
            if *fail {
                *fail = false;
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"ok": false, "error_code": 500, "description": "temporary error"})),
                );
            }
            (StatusCode::OK, Json(json!({"ok": true, "result": true})))
        }
        "answerCallbackQuery" | "sendChatAction" => {
            (StatusCode::OK, Json(json!({"ok": true, "result": true})))
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error_code": 404, "description": "unknown method"})),
        ),
    }
}

fn tg_host() -> Arc<HostConfig> {
    Arc::new(
        HostConfig::from_value(json!({
            "telegram": {"bots": {"ops": {"bot_id": BOT_ID, "bot_token": "telegram-bot-token", "dm": true}}},
            "admin_url": "http://127.0.0.1:8787/ui"
        }))
        .unwrap(),
    )
}

fn tg_profile(id: &str) -> Arc<bot_core::profile::Snapshot> {
    profile(id)
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .agent_scopes("main", &["run"])
        .member(&format!("telegram:{USER_ID}"), &["owner"])
        .binding("tg", Adapter::TelegramDm { bot: "ops".into() }, "main")
        .build()
}

struct Rig {
    tg: FakeTelegram,
    store: Arc<MemoryStore>,
    broker: Arc<MemoryBroker>,
    ledger: Arc<MemoryLedger>,
    outbox: Arc<MemoryOutbox>,
    ingress: Arc<Ingress>,
    gateway: Arc<TelegramGateway>,
}

async fn setup_rig() -> Rig {
    let tg = FakeTelegram::start().await;
    let store = MemoryStore::new();
    let broker = MemoryBroker::new();
    let ledger = MemoryLedger::new();
    let outbox = MemoryOutbox::new();
    let secrets = MemorySecrets::new(&[("personal", "telegram-bot-token", BOT_TOKEN.as_bytes())]);
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new(vec![tg_profile("personal")]),
        tg_host(),
        store.clone(),
        MemoryPrivate::new(),
    ));
    let gateway = Arc::new(
        TelegramGateway::new(
            ingress.clone(),
            broker.clone(),
            ledger.clone(),
            secrets,
            TelegramApi::new(Some(&tg.base)),
        )
        .with_runs(store.clone())
        .with_poll_timing(0, Duration::from_millis(5)),
    );
    Rig {
        tg,
        store,
        broker,
        ledger,
        outbox,
        ingress,
        gateway,
    }
}

fn dm_update(update_id: i64, user_id: u64, message_id: u64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {
            "message_id": message_id,
            "from": {"id": user_id, "is_bot": false},
            "chat": {"id": user_id, "type": "private"},
            "text": text
        }
    })
}

fn callback_update(id: i64, cb_id: &str, user_id: u64, msg_id: u64, data: &str) -> Value {
    json!({
        "update_id": id,
        "callback_query": {
            "id": cb_id,
            "from": {"id": user_id, "is_bot": false},
            "message": {"message_id": msg_id, "chat": {"id": user_id, "type": "private"}},
            "data": data
        }
    })
}

#[tokio::test]
async fn plan_verifies_get_me_and_rejects_wrong_bot_or_missing_secret() {
    let rig = setup_rig().await;
    let installs = rig.gateway.plan().await.unwrap();
    assert_eq!(installs.len(), 1);
    assert_eq!(installs[0].profile, ProfileId("personal".into()));
    assert_eq!(installs[0].alias, "ops");
    assert_eq!(installs[0].bot_id, BOT_ID);

    let tg = FakeTelegram::start().await;
    *tg.state.get_me_id.lock().unwrap() = BOT_ID + 1;
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new(vec![tg_profile("personal")]),
        tg_host(),
        MemoryStore::new(),
        MemoryPrivate::new(),
    ));
    let gw_wrong = TelegramGateway::new(
        ingress.clone(),
        MemoryBroker::new(),
        MemoryLedger::new(),
        MemorySecrets::new(&[("personal", "telegram-bot-token", BOT_TOKEN.as_bytes())]),
        TelegramApi::new(Some(&tg.base)),
    );
    let err = gw_wrong.plan().await.unwrap_err().to_string();
    assert!(err.contains("different bot_id"), "{err}");
    assert!(!err.contains(BOT_TOKEN), "error leaked bot token: {err}");

    let gw_missing = TelegramGateway::new(
        ingress,
        MemoryBroker::new(),
        MemoryLedger::new(),
        MemorySecrets::new(&[]),
        TelegramApi::new(Some(&tg.base)),
    );
    assert!(
        gw_missing
            .plan()
            .await
            .unwrap_err()
            .to_string()
            .contains("bot_token")
    );
}

#[tokio::test]
async fn private_dm_admits_member_ignores_strangers_bots_and_groups_and_deduplicates() {
    let rig = setup_rig().await;
    let installs = rig.gateway.plan().await.unwrap();
    let stop = CancellationToken::new();
    let mut offset = None;

    rig.tg.push_updates(
        StatusCode::OK,
        json!({
            "ok": true,
            "result": [
                dm_update(100, USER_ID, 5, "hello bot"),
                dm_update(101, USER_ID, 5, "hello bot"),
                dm_update(102, 999_999, 6, "unregistered stranger"),
                {"update_id": 103, "message": {"message_id": 7, "from": {"id": USER_ID, "is_bot": true}, "chat": {"id": USER_ID, "type": "private"}, "text": "from bot"}},
                {"update_id": 104, "message": {"message_id": 8, "from": {"id": USER_ID, "is_bot": false}, "chat": {"id": 555_555, "type": "group"}, "text": "group message"}}
            ]
        }),
    );

    let pause = rig
        .gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(pause, Duration::ZERO);
    assert_eq!(offset, Some(105));

    let accepted = rig.store.accepted();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].key.principal.0, format!("telegram:{USER_ID}"));
    assert_eq!(accepted[0].key.request.0, format!("tg.{USER_ID}.5"));
    assert_eq!(
        accepted[0].conversation,
        format!("direct:{BOT_ID}/{USER_ID}:")
    );
    assert_eq!(accepted[0].reply_transport, Some(PushTransportId::TELEGRAM));
    assert_eq!(
        accepted[0].approver.as_deref(),
        Some(format!("telegram:{USER_ID}").as_str())
    );
    assert_eq!(accepted[0].target, "agent main hello bot");
}

#[tokio::test]
async fn transient_outage_stops_batch_and_409_aborts_without_delete_webhook() {
    let rig = setup_rig().await;
    let installs = rig.gateway.plan().await.unwrap();
    let stop = CancellationToken::new();
    let mut offset = None;

    rig.store.fail_next(Error::Unavailable);
    let batch = json!({
        "ok": true,
        "result": [
            dm_update(200, USER_ID, 20, "first"),
            dm_update(201, USER_ID, 21, "second")
        ]
    });
    rig.tg.push_updates(StatusCode::OK, batch.clone());
    let pause = rig
        .gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert!(pause > Duration::ZERO);
    assert_eq!(
        offset, None,
        "must not advance offset past unsettled update 200"
    );
    assert!(rig.store.accepted().is_empty());

    rig.tg.push_updates(StatusCode::OK, batch);
    rig.gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(offset, Some(202));
    assert_eq!(rig.store.accepted().len(), 2);

    rig.tg.push_updates(
        StatusCode::CONFLICT,
        json!({"ok": false, "error_code": 409, "description": format!("Conflict: other poller {BOT_TOKEN}")}),
    );
    let err = rig
        .gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("409 Conflict"), "{err}");
    assert!(
        !err.contains(BOT_TOKEN),
        "409 error leaked bot token: {err}"
    );
    assert!(
        rig.tg.calls("deleteWebhook").is_empty(),
        "must never call deleteWebhook"
    );
}

#[tokio::test]
async fn approval_callbacks_decide_answer_and_edit_card_while_rejecting_unauthorized_clicks() {
    let rig = setup_rig().await;
    let installs = rig.gateway.plan().await.unwrap();
    let stop = CancellationToken::new();
    let mut offset = None;

    let card = json!({"nonce": "n1", "title": "Query DB", "lines": ["Target: main"]}).to_string();
    rig.ledger
        .put("n1", "personal", ApprovalState::Approved, &card);
    rig.ledger.put("n2", "other", ApprovalState::Pending, &card);

    rig.broker.fail_next(Error::Unavailable);
    let approve_batch =
        json!({"ok": true, "result": [callback_update(300, "cb-1", USER_ID, 50, "bot:a:n1")]});
    rig.tg.push_updates(StatusCode::OK, approve_batch.clone());
    assert!(
        rig.gateway
            .poll_once(&installs[0], &mut offset, &stop)
            .await
            .unwrap()
            > Duration::ZERO
    );
    assert_eq!(
        offset, None,
        "transient callback outage must not advance offset"
    );

    *rig.tg.state.fail_next_edit.lock().unwrap() = true;
    rig.tg.push_updates(StatusCode::OK, approve_batch);
    rig.gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(offset, Some(301));
    assert_eq!(
        rig.broker.decisions(),
        [("n1".to_owned(), format!("telegram:{USER_ID}"), true)]
    );
    assert_eq!(
        rig.tg
            .calls("answerCallbackQuery")
            .last()
            .unwrap()
            .body_json
            .as_ref()
            .unwrap()["text"],
        "Approved"
    );
    assert_eq!(
        rig.tg.calls("editMessageText").len(),
        2,
        "transient 500 on editMessageText must retry once"
    );
    let edit_body = rig.tg.calls("editMessageText")[1]
        .body_json
        .clone()
        .unwrap();
    assert_eq!(
        (
            edit_body["chat_id"].as_u64(),
            edit_body["message_id"].as_u64()
        ),
        (Some(USER_ID), Some(50))
    );
    assert!(edit_body["text"].as_str().unwrap().contains("✅ Approved"));
    assert_eq!(edit_body["reply_markup"], json!({"inline_keyboard": []}));

    rig.ledger.set_state("n1", ApprovalState::Denied);
    rig.tg.push_updates(
        StatusCode::OK,
        json!({"ok": true, "result": [callback_update(301, "cb-2", USER_ID, 50, "bot:d:n1")]}),
    );
    rig.gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert!(!rig.broker.decisions()[1].2);
    assert!(
        rig.tg.calls("editMessageText")[2]
            .body_json
            .as_ref()
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("⛔ Denied")
    );

    rig.broker.fail_next(Error::Denied);
    rig.tg.push_updates(
        StatusCode::OK,
        json!({"ok": true, "result": [callback_update(302, "cb-3", 999_999, 51, "bot:a:n1"), callback_update(303, "cb-4", USER_ID, 52, "bot:a:n2"), callback_update(304, "cb-5", USER_ID, 53, "bot:a:n1")]}),
    );
    rig.gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(offset, Some(305));
    assert_eq!(rig.broker.decisions().len(), 2);
    assert_eq!(rig.tg.calls("editMessageText").len(), 3);
}

#[tokio::test]
async fn sender_delivers_chunked_plain_text_documents_and_approval_cards_without_parse_mode() {
    let rig = setup_rig().await;
    let sender = Arc::new(rig.gateway.sender());
    let deliverer = Deliverer::new(rig.outbox.clone(), rig.ingress.clone(), sender);
    let dest = Conversation {
        kind: ConversationKind::Direct,
        id: format!("{BOT_ID}/{USER_ID}"),
        thread: None,
    };
    let push_tg = |seq: u32, body: String| {
        let mut item = outgoing(
            "personal",
            seq,
            "tg",
            &format!("telegram:{USER_ID}"),
            dest.clone(),
            &body,
        );
        item.transport = PushTransportId::TELEGRAM;
        rig.outbox.push(item);
    };

    push_tg(1, json!({"text": "😀".repeat(2500)}).to_string());
    push_tg(
        2,
        json!({"approval": {"nonce": "nonce-42", "title": "Restart daemon", "lines": ["Target: svc"]}}).to_string(),
    );
    push_tg(
        3,
        json!({"text": "Report attached.", "attachments": [{"name": "summary.txt", "type": "text/plain", "data": "aGVsbG8="}]}).to_string(),
    );
    push_tg(
        4,
        json!({"approval": {"nonce": "nonce-big", "title": "Huge op", "lines": vec!["x".repeat(400); 12]}}).to_string(),
    );

    assert_eq!(
        rig.gateway.sender().scope(),
        DeliveryScope::all(PushTransportId::TELEGRAM)
    );
    assert_eq!(deliverer.tick().await.unwrap(), 4);
    let settled = rig.outbox.settled();
    assert_eq!(settled.len(), 4);
    assert!(
        settled.iter().all(|(_, s)| s.starts_with("confirmed:")),
        "{settled:?}"
    );

    let sends = rig.tg.calls("sendMessage");
    assert!(sends.len() >= 4);
    for call in &sends {
        let body = call.body_json.as_ref().unwrap();
        assert!(body.get("parse_mode").is_none(), "must omit parse_mode");
        assert_eq!(body["link_preview_options"]["is_disabled"], true);
    }
    let card = sends
        .iter()
        .find(|c| c.body_json.as_ref().unwrap().get("reply_markup").is_some())
        .unwrap();
    let buttons = &card.body_json.as_ref().unwrap()["reply_markup"]["inline_keyboard"][0];
    assert_eq!(buttons[0]["callback_data"], "bot:a:nonce-42");
    assert_eq!(buttons[1]["callback_data"], "bot:d:nonce-42");

    let fallback = sends
        .iter()
        .find(|c| {
            c.body_json.as_ref().unwrap()["text"]
                .as_str()
                .is_some_and(|t| t.contains("http://127.0.0.1:8787/ui"))
        })
        .expect("oversized card falls back to admin_url notice");
    assert!(
        fallback
            .body_json
            .as_ref()
            .unwrap()
            .get("reply_markup")
            .is_none()
    );

    let docs = rig.tg.calls("sendDocument");
    assert_eq!(docs.len(), 1);
    assert!(docs[0].body_text.contains("filename=\"summary.txt\""));
    assert!(docs[0].body_text.contains("Report attached."));
    assert!(docs[0].body_text.contains("hello"));
}
