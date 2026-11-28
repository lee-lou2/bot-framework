//! Telegram extended integration tests: groups, forum topics, quoted replies, attachments, plain-text/photo delivery, UX reactions, and DM-routed group approvals.
use crate::support::ux::UxStore;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::post,
};
use bot_core::{
    cards::{CardJob, CardKey, CardOutcome},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId},
    intake::PushTransportId,
    ledger::ApprovalState,
    profile::Adapter,
    state::{Conversation, ConversationKind, RunState},
};
use bot_gateway::{
    delivery::Deliverer,
    host::HostConfig,
    ingress::Ingress,
    telegram::{TelegramGateway, api::TelegramApi, cards::TelegramCardDeliverer},
    testing::{
        MemoryPrivate, MemorySecrets, TestCatalog,
        cards::MemoryCards,
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

const BOT_ID: u64 = 200_300_400;
const BOT_TOKEN: &str = "200300400:SYNTHETIC_GROUP_TOKEN_SECRET";
const USER_ID: u64 = 77_000_001;
const GROUP_ID: i64 = -100_555_666;

#[derive(Clone, Debug)]
struct RecordedCall {
    method: String,
    body_text: String,
    body_json: Option<Value>,
}

struct FakeState {
    calls: Mutex<Vec<RecordedCall>>,
    updates: Mutex<VecDeque<(StatusCode, Value)>>,
    next_msg_id: Mutex<u64>,
    fail_photo_400: Mutex<bool>,
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
            next_msg_id: Mutex::new(100),
            fail_photo_400: Mutex::new(false),
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

    fn push_updates(&self, body: Value) {
        self.state
            .updates
            .lock()
            .unwrap()
            .push_back((StatusCode::OK, body));
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

    async fn until(&self, pred: impl Fn(&FakeTelegram) -> bool) -> bool {
        for _ in 0..80 {
            if pred(self) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        pred(self)
    }
}

fn multipart_field<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    let needle = format!("name=\"{name}\"\r\n\r\n");
    let start = body.find(&needle)? + needle.len();
    let end = body[start..].find("\r\n")?;
    Some(&body[start..start + end])
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
        body_text: body_text.clone(),
        body_json: body_json.clone(),
    });
    match method.as_str() {
        "getMe" => (
            StatusCode::OK,
            Json(
                json!({"ok": true, "result": {"id": BOT_ID, "is_bot": true, "username": "ops_bot"}}),
            ),
        ),
        "getUpdates" => state.updates.lock().unwrap().pop_front().map_or_else(
            || (StatusCode::OK, Json(json!({"ok": true, "result": []}))),
            |(s, v)| (s, Json(v)),
        ),
        "sendPhoto" if *state.fail_photo_400.lock().unwrap() => {
            *state.fail_photo_400.lock().unwrap() = false;
            (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"ok": false, "error_code": 400, "description": "PHOTO_INVALID_DIMENSIONS"}),
                ),
            )
        }
        "sendMessage" | "sendDocument" | "sendPhoto" => {
            let mut next = state.next_msg_id.lock().unwrap();
            *next += 1;
            let chat_id = body_json
                .as_ref()
                .and_then(|v| v["chat_id"].as_i64())
                .or_else(|| {
                    multipart_field(&body_text, "chat_id").and_then(|s| s.parse::<i64>().ok())
                })
                .unwrap_or(USER_ID as i64);
            let chat_type = if chat_id < 0 { "supergroup" } else { "private" };
            (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "result": {"message_id": *next, "chat": {"id": chat_id, "type": chat_type}}}),
                ),
            )
        }
        "setMessageReaction" | "sendChatAction" | "answerCallbackQuery" | "editMessageText" => {
            (StatusCode::OK, Json(json!({"ok": true, "result": true})))
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error_code": 404, "description": "unknown"})),
        ),
    }
}

fn host_with_groups() -> Arc<HostConfig> {
    Arc::new(
        HostConfig::from_value(json!({
            "telegram": {"bots": {"ops": {"bot_id": BOT_ID, "bot_token": "telegram-bot-token", "dm": true, "groups": true}}},
            "admin_url": "http://127.0.0.1:8787/ui"
        }))
        .unwrap(),
    )
}

fn group_and_dm_profile(include_dm: bool) -> Arc<bot_core::profile::Snapshot> {
    let mut b = profile("personal")
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .agent_scopes("main", &["run"])
        .member(&format!("telegram:{USER_ID}"), &["owner"])
        .binding(
            "tg-groups",
            Adapter::TelegramGroups { bot: "ops".into() },
            "main",
        );
    if include_dm {
        b = b.binding("tg-dm", Adapter::TelegramDm { bot: "ops".into() }, "main");
    }
    b.build()
}

#[tokio::test]
async fn groups_accept_mentions_commands_replies_and_forum_topics_with_quotes_and_attachments() {
    let tg = FakeTelegram::start().await;
    let ux_store = UxStore::new();
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new(vec![group_and_dm_profile(true)]),
        host_with_groups(),
        ux_store.clone(),
        MemoryPrivate::new(),
    ));
    let gateway = Arc::new(
        TelegramGateway::new(
            ingress,
            MemoryBroker::new(),
            MemoryLedger::new(),
            MemorySecrets::new(&[("personal", "telegram-bot-token", BOT_TOKEN.as_bytes())]),
            TelegramApi::new(Some(&tg.base)),
        )
        .with_ux_timing(
            ux_store.clone(),
            Duration::from_millis(20),
            Duration::from_millis(20),
            Duration::from_secs(5),
        )
        .with_poll_timing(0, Duration::from_millis(5)),
    );
    let installs = gateway.plan().await.unwrap();
    assert_eq!(installs.len(), 1, "DM and groups share one poller per bot");
    assert_eq!(installs[0].username.as_deref(), Some("ops_bot"));

    let stop = CancellationToken::new();
    let mut offset = None;
    tg.push_updates(json!({
        "ok": true,
        "result": [
            // 1) Unmentioned group chatter -> ignored
            {"update_id": 1, "message": {"message_id": 10, "from": {"id": USER_ID, "is_bot": false}, "chat": {"id": GROUP_ID, "type": "supergroup"}, "text": "random chat"}},
            // 2) Forum topic creation service stub -> ignored even if from.id == BOT_ID
            {"update_id": 2, "message": {"message_id": 11, "is_topic_message": true, "message_thread_id": 77, "from": {"id": USER_ID, "is_bot": false}, "chat": {"id": GROUP_ID, "type": "supergroup"}, "reply_to_message": {"from": {"id": BOT_ID}, "forum_topic_created": {"name": "Ops"}}, "text": "topic chatter"}},
            // 3) Explicit @mention in forum topic replying to another person's message
            {"update_id": 3, "message": {
                "message_id": 12,
                "is_topic_message": true,
                "message_thread_id": 77,
                "from": {"id": USER_ID, "is_bot": false},
                "chat": {"id": GROUP_ID, "type": "supergroup"},
                "text": "@ops_bot 이거 확인해줘",
                "entities": [{"type": "mention", "offset": 0, "length": 8}],
                "reply_to_message": {"from": {"id": 888}, "text": "원래 질문 내용"}
            }},
            // 4) Direct reply to bot with captionless document upload
            {"update_id": 4, "message": {
                "message_id": 13,
                "from": {"id": USER_ID, "is_bot": false},
                "chat": {"id": GROUP_ID, "type": "group"},
                "reply_to_message": {"from": {"id": BOT_ID}, "text": "이전 봇 답변"},
                "document": {"file_id": "DOC_FILE_1", "file_name": "memo.pdf"}
            }},
            // 5) Non-member mention -> rejected cleanly
            {"update_id": 5, "message": {
                "message_id": 14,
                "from": {"id": 999_999, "is_bot": false},
                "chat": {"id": GROUP_ID, "type": "supergroup"},
                "text": "@ops_bot hello",
                "entities": [{"type": "mention", "offset": 0, "length": 8}]
            }}
        ]
    }));

    gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(offset, Some(6));

    let accepted = ux_store.inner.accepted();
    assert_eq!(accepted.len(), 2);
    assert_eq!(accepted[0].key.binding.0, "tg-groups");
    assert_eq!(
        accepted[0].conversation,
        format!("channel:{BOT_ID}/{GROUP_ID}:77")
    );
    assert_eq!(
        accepted[0].target,
        "agent main [Replying to telegram:888: \"원래 질문 내용\"]\n이거 확인해줘"
    );
    assert_eq!(
        accepted[1].conversation,
        format!("channel:{BOT_ID}/{GROUP_ID}:")
    );
    assert_eq!(
        accepted[1].target,
        "agent main [Replying to bot: \"이전 봇 답변\"]\n[Attachment: document memo.pdf]"
    );

    // Wait for 👀 reaction and forum-scoped typing indicator.
    assert!(
        tg.until(
            |f| f.calls("setMessageReaction").len() >= 2 && !f.calls("sendChatAction").is_empty()
        )
        .await
    );
    let reactions = tg.calls("setMessageReaction");
    assert!(reactions.iter().any(|c| {
        let j = c.body_json.as_ref().unwrap();
        j["chat_id"] == GROUP_ID && j["message_id"] == 12 && j["reaction"][0]["emoji"] == "👀"
    }));
    let typing = tg.calls("sendChatAction");
    assert!(typing.iter().any(|c| {
        let j = c.body_json.as_ref().unwrap();
        j["chat_id"] == GROUP_ID && j["message_thread_id"] == 77 && j["action"] == "typing"
    }));

    // Pause during waiting_approval, then complete -> 👍 for run-1 and cleared [] for run-2.
    let r1 = accepted[0].run.clone();
    let r2 = accepted[1].run.clone();
    ux_store.set_waiting_approval(&r1, true);
    tokio::time::sleep(Duration::from_millis(40)).await;
    ux_store.set_waiting_approval(&r1, false);
    ux_store.set_progress(&r1, RunState::Succeeded);
    ux_store.set_progress(&r2, RunState::Dead);

    assert!(
        tg.until(|f| {
            let calls = f.calls("setMessageReaction");
            let has_done = calls.iter().any(|c| {
                let j = c.body_json.as_ref().unwrap();
                j["message_id"] == 12 && j["reaction"][0]["emoji"] == "👍"
            });
            let has_clear = calls.iter().any(|c| {
                let j = c.body_json.as_ref().unwrap();
                j["message_id"] == 13 && j["reaction"] == json!([])
            });
            has_done && has_clear
        })
        .await
    );

    // Outbound group reply cleans Markdown, uses reply_parameters + message_thread_id, and supports photo -> doc fallback.
    let outbox = MemoryOutbox::new();
    let deliverer = Deliverer::new(
        outbox.clone(),
        Arc::new(Ingress::new(
            TestCatalog::new(vec![group_and_dm_profile(true)]),
            host_with_groups(),
            ux_store.clone(),
            MemoryPrivate::new(),
        )),
        Arc::new(gateway.sender()),
    );
    let group_dest = Conversation {
        kind: ConversationKind::Channel,
        id: format!("{BOT_ID}/{GROUP_ID}"),
        thread: Some("77".into()),
    };
    let mut text_item = outgoing(
        "personal",
        1,
        "tg-groups",
        &format!("telegram:{USER_ID}"),
        group_dest.clone(),
        &json!({"text": "### 요약\n**완료** (`ok`)"}).to_string(),
    );
    text_item.key.run = r1.clone();
    text_item.transport = PushTransportId::TELEGRAM;
    outbox.push(text_item);

    // Approval card directly targeting a group channel must be rejected.
    let mut bad_card = outgoing(
        "personal",
        2,
        "tg-groups",
        &format!("telegram:{USER_ID}"),
        group_dest.clone(),
        &json!({"approval": {"nonce": "n-leak", "title": "Secret", "lines": ["Args: secret"]}})
            .to_string(),
    );
    bad_card.key.run = r1.clone();
    bad_card.transport = PushTransportId::TELEGRAM;
    outbox.push(bad_card);

    // PNG attachment tries sendPhoto; on HTTP 400 falls back to sendDocument.
    *tg.state.fail_photo_400.lock().unwrap() = true;
    let mut photo_item = outgoing(
        "personal",
        3,
        "tg-groups",
        &format!("telegram:{USER_ID}"),
        group_dest,
        &json!({"text": "**차트**", "attachments": [{"name": "chart.png", "type": "image/png", "data": "iVBORw0K"}]}).to_string(),
    );
    photo_item.key.run = r1;
    photo_item.transport = PushTransportId::TELEGRAM;
    outbox.push(photo_item);

    assert_eq!(deliverer.tick().await.unwrap(), 3);
    let settled = outbox.settled();
    assert!(settled[0].1.starts_with("confirmed:"));
    assert_eq!(
        settled[1].1, "rejected",
        "must never post approval cards in group chats"
    );
    assert!(settled[2].1.starts_with("confirmed:"));

    let msg = tg.calls("sendMessage").pop().unwrap();
    let msg_json = msg.body_json.unwrap();
    assert_eq!(msg_json["chat_id"], GROUP_ID);
    assert_eq!(msg_json["message_thread_id"], 77);
    assert_eq!(msg_json["reply_parameters"]["message_id"], 12);
    assert_eq!(msg_json["text"], "요약\n완료 (ok)");

    assert_eq!(tg.calls("sendPhoto").len(), 1);
    let doc = tg.calls("sendDocument").pop().unwrap();
    assert!(
        doc.body_text
            .contains("name=\"chat_id\"\r\n\r\n-100555666\r\n")
    );
    assert!(
        doc.body_text
            .contains("name=\"message_thread_id\"\r\n\r\n77\r\n")
    );
    assert!(doc.body_text.contains("name=\"caption\"\r\n\r\n차트\r\n"));
    stop.cancel();
}

#[tokio::test]
async fn group_origin_approval_cards_deliver_via_owner_dm_and_verify_receipt_without_dm_binding() {
    let tg = FakeTelegram::start().await;
    let ux_store = UxStore::new();
    let broker = MemoryBroker::new();
    let ledger = MemoryLedger::new();
    let cards_outbox = MemoryCards::linked(ledger.clone());
    // Profile binds ONLY TelegramGroups (no TelegramDm binding!).
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new(vec![group_and_dm_profile(false)]),
        host_with_groups(),
        ux_store,
        MemoryPrivate::new(),
    ));
    let gateway = Arc::new(
        TelegramGateway::new(
            ingress.clone(),
            broker.clone(),
            ledger.clone(),
            MemorySecrets::new(&[("personal", "telegram-bot-token", BOT_TOKEN.as_bytes())]),
            TelegramApi::new(Some(&tg.base)),
        )
        .with_poll_timing(0, Duration::from_millis(5)),
    );
    let installs = gateway.plan().await.unwrap();
    let card_json =
        json!({"nonce": "grp-n1", "title": "Restart service", "lines": ["Target: teams"]})
            .to_string();
    ledger.put("grp-n1", "personal", ApprovalState::Approved, &card_json);
    cards_outbox.push(CardJob {
        key: CardKey {
            nonce: ApprovalId("grp-n1".into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId("tg-groups".into()),
        owner: PrincipalId(format!("telegram:{USER_ID}")),
        card: JsonPayload(card_json),
        generation: Generation(1),
        source: None,
    });

    // Slack claim must not claim Telegram card jobs.
    assert!(
        bot_core::cards::CardOutbox::claim(&*cards_outbox, 8)
            .await
            .unwrap()
            .is_empty()
    );

    let card_deliverer =
        TelegramCardDeliverer::new(cards_outbox.clone(), ingress, gateway.sender());
    assert_eq!(card_deliverer.tick().await.unwrap(), 1);
    let settled = cards_outbox.settled();
    assert_eq!(settled.len(), 1);
    let msg_id: u64 = match &settled[0].1 {
        CardOutcome::Posted { team, dm, ts } => {
            assert_eq!(team, &BOT_ID.to_string());
            assert_eq!(dm, &USER_ID.to_string());
            ts.parse().unwrap()
        }
        other => panic!("expected Posted, got {other:?}"),
    };

    // Wrong message_id click is rejected as wrong_card; exact receipt message_id succeeds!
    let stop = CancellationToken::new();
    let mut offset = None;
    tg.push_updates(json!({
        "ok": true,
        "result": [
            {"update_id": 10, "callback_query": {
                "id": "cb-wrong",
                "from": {"id": USER_ID, "is_bot": false},
                "message": {"message_id": msg_id + 1, "chat": {"id": USER_ID, "type": "private"}},
                "data": "bot:a:grp-n1"
            }},
            {"update_id": 11, "callback_query": {
                "id": "cb-ok",
                "from": {"id": USER_ID, "is_bot": false},
                "message": {"message_id": msg_id, "chat": {"id": USER_ID, "type": "private"}},
                "data": "bot:a:grp-n1"
            }}
        ]
    }));
    gateway
        .poll_once(&installs[0], &mut offset, &stop)
        .await
        .unwrap();
    assert_eq!(offset, Some(12));
    assert_eq!(
        broker.decisions(),
        [("grp-n1".to_owned(), format!("telegram:{USER_ID}"), true)]
    );
    let answers = tg.calls("answerCallbackQuery");
    assert_eq!(
        answers[0].body_json.as_ref().unwrap()["text"],
        "Cannot approve using this card."
    );
    assert_eq!(answers[1].body_json.as_ref().unwrap()["text"], "Approved");
}

#[tokio::test]
async fn plan_rejects_same_telegram_bot_split_across_two_profiles() {
    let tg = FakeTelegram::start().await;
    let p1 = profile("p1")
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .agent_scopes("main", &["run"])
        .member(&format!("telegram:{USER_ID}"), &["owner"])
        .binding("dm", Adapter::TelegramDm { bot: "ops".into() }, "main")
        .build();
    let p2 = profile("p2")
        .role("owner", &["run"])
        .policy_scopes(&["run"])
        .agent_scopes("main", &["run"])
        .member(&format!("telegram:{USER_ID}"), &["owner"])
        .binding(
            "groups",
            Adapter::TelegramGroups { bot: "ops".into() },
            "main",
        )
        .build();
    let ingress = Arc::new(Ingress::new(
        TestCatalog::new(vec![p1, p2]),
        host_with_groups(),
        UxStore::new(),
        MemoryPrivate::new(),
    ));
    let gw = TelegramGateway::new(
        ingress,
        MemoryBroker::new(),
        MemoryLedger::new(),
        MemorySecrets::new(&[
            ("p1", "telegram-bot-token", BOT_TOKEN.as_bytes()),
            ("p2", "telegram-bot-token", BOT_TOKEN.as_bytes()),
        ]),
        TelegramApi::new(Some(&tg.base)),
    );
    let err = gw.plan().await.unwrap_err().to_string();
    assert!(
        err.contains("one bot may belong to only one profile"),
        "{err}"
    );
    assert!(!err.contains(BOT_TOKEN));
}
