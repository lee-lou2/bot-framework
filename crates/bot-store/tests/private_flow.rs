//! 민감 DM 종단: 실제 PrivateGate를 PG 전용 원장(PrivateLedger) 위에서 돌린다.
//! 정상 전달 1회, 같은 키 재전달은 추가 호출 0회, 결과 불명·프로세스 사망은 재전달 없이도 내구 보류(운영자 조회),
//! 전송 전 실패는 같은 입력의 재전달이 이어 받음, 원문·판정 입력 원문 저장 0.

mod support;

use bot_broker::{
    ports::Ran,
    private::{FINGERPRINT_SECRET, PrivateGate, PrivateParts},
    rate::RateConfig,
    sys::SystemClock,
    testing::{
        FakeCatalog, FakeRunner, FakeSealer, FakeSecrets, RecordingTracer, snapshot,
        tool as tool_decl,
    },
};
use bot_core::{
    admin::{AdminAct, AdminRead, HoldRef, HoldVerdict, Viewer},
    command::*,
    id::*,
    intake::{PrivateCall, PrivateIntake, RequestKey},
    ledger::{PrivateLedger, PrivatePermit},
    port::{AuditLog, Error},
    state::{Conversation, ConversationKind, Risk},
};
use std::sync::Arc;
use support::*;

const SENTINEL: &str = "TOP-SECRET-DM-TEXT-7731";
const SENDER: &str = "slack:demo/U1";

struct Rig {
    gate: PrivateGate,
    runner: Arc<FakeRunner>,
}

fn rig(db: &TestDb) -> Rig {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "inbox.write"])
            .member(SENDER, &["owner"])
            .slack_dm_binding("direct", "main")
            .policy_scopes(&["run", "inbox.write"])
            .tool(
                "main",
                "inbox.receive",
                tool_decl(Risk::Write, &["inbox.write"]),
            )
            .private_trigger("main", "secure", "direct", "inbox.receive")
            .build(),
    );
    let runner = Arc::new(FakeRunner::new());
    let gate = PrivateGate::new(
        PrivateParts {
            catalog,
            ledger: Arc::new(db.store.clone()),
            secrets: Arc::new(FakeSecrets::new(&[(
                "personal",
                FINGERPRINT_SECRET,
                "profile-intake-key-bytes",
            )])),
            sealer: Arc::new(FakeSealer),
            runner: runner.clone(),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
        },
        RateConfig::default(),
    );
    Rig { gate, runner }
}

fn delivery(request: &str, text: &str) -> PrivateCall {
    PrivateCall {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId("direct".into()),
            principal: PrincipalId(SENDER.into()),
            request: RequestId(request.into()),
        },
        revision: Revision("r1".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        agent: AgentId("main".into()),
        tool: ToolId("inbox.receive".into()),
        args: JsonPayload(serde_json::json!({ "text": text }).to_string()),
    }
}

/// 도구가 정상으로 끝났다고 알리는 결과(`isError: false`가 명시돼야 성공으로 적는다).
fn ok() -> Ran {
    Ran::Done(JsonPayload(r#"{"isError":false,"content":[]}"#.into()))
}

fn operator() -> Viewer {
    Viewer {
        principal: PrincipalId("local:owner".into()),
        operator: true,
    }
}

/// 스키마의 모든 표를 글로 펼쳐 원문 표지가 있는 행 수를 센다.
async fn rows_containing(db: &TestDb, needle: &str) -> i64 {
    let tables: Vec<String> = sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = current_schema() and table_type = 'BASE TABLE'")
        .fetch_all(db.store.pool())
        .await
        .unwrap();
    let mut found = 0;
    for table in tables {
        let sql = format!("select count(*) from \"{table}\" t where t::text like '%{needle}%'");
        found += db.scalar::<i64>(&sql).await;
    }
    found
}

async fn the_hold(db: &TestDb) -> HoldRef {
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1);
    assert!(holds[0].call.0.starts_with("private:"));
    HoldRef {
        hold_id: holds[0]
            .hold_id
            .clone()
            .expect("민감 DM 보류는 발생 ID를 갖는다"),
        profile: holds[0].profile.clone(),
        run: holds[0].run.clone(),
        call: holds[0].call.clone(),
        effect: None,
    }
}

#[tokio::test]
async fn a_private_dm_reaches_only_its_tool_once_and_leaves_no_plaintext_in_pg() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(ok());
    r.gate.receive(delivery("req-1", SENTINEL)).await.unwrap();
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1, "지정 도구가 한 번 실행된다");
    assert_eq!(jobs[0].principal.0, SENDER);
    assert!(jobs[0].args.contains(SENTINEL), "도구만 원문을 받는다");
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "succeeded"
    );
    assert!(
        db.scalar::<bool>("select private_started_ms is not null from bot_runs")
            .await,
        "전송 직전에 시작이 한 번 기록된다"
    );
    for table in ["bot_calls", "bot_effects", "bot_approvals", "bot_steps"] {
        let sql = format!("select count(*) from {table}");
        assert_eq!(db.scalar::<i64>(&sql).await, 0, "{table}은 쓰지 않는다");
    }
    assert_eq!(
        rows_containing(&db, SENTINEL).await,
        0,
        "원문은 어느 표에도 남지 않는다"
    );

    // 같은 접수 키의 재전달은 호출을 늘리지 않는다. 같은 키·다른 본문은 충돌이다
    r.gate.receive(delivery("req-1", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
    assert!(matches!(
        r.gate.receive(delivery("req-1", "다른 본문")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_unknown_outcome_is_held_durably_never_rerun_and_the_verdict_input_is_not_stored() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(Ran::Unknown("timeout")); // 보냈는지 모른다
    r.gate.receive(delivery("req-2", SENTINEL)).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select state || ':' || wait_kind from bot_runs")
            .await,
        "waiting:hold"
    );
    // 원문으로 다시 보내도 새 효과는 없다
    r.gate.receive(delivery("req-2", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);

    // 운영자 보류 목록에 보이고, 판정(실행됨)으로 닫으면 완료가 된다. 영수증 원문은 어디에도 남지 않는다
    let hold = the_hold(&db).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:owner".into()),
        hold,
        HoldVerdict::Confirmed {
            receipt: "evt-RECEIPT-1".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        db.scalar::<String>("select state from bot_runs where private")
            .await,
        "succeeded"
    );
    assert_eq!(rows_containing(&db, "evt-RECEIPT-1").await, 0);
    assert_eq!(
        db.scalar::<String>("select verdict from bot_audit where action = 'hold.resolve'")
            .await,
        "confirmed"
    );

    // 미실행 판정의 증거 글도 감사·행에 남기지 않는다(고정 어휘만)
    r.runner.script(Ran::Unknown("timeout"));
    r.gate.receive(delivery("req-3", SENTINEL)).await.unwrap();
    let hold = the_hold(&db).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:owner".into()),
        hold,
        HoldVerdict::NotExecuted {
            evidence: "evidence-TEXT-9".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(rows_containing(&db, "evidence-TEXT-9").await, 0);
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'hold.resolve' and verdict = 'not_executed'"
        )
        .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_failure_before_sending_is_resumed_by_the_same_redelivery_with_a_new_claim() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    r.runner.script(Ran::NotSent); // 전송 전 실패(미실행 입증)
    r.runner.script(ok());
    let first = r.gate.receive(delivery("req-4", SENTINEL)).await;
    assert!(matches!(first, Err(Error::Unavailable)), "ACK하지 않는다");
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "ready",
        "리스를 반납했다(워커는 집지 않는다)"
    );
    // 같은 입력의 재전달이 새 claim 토큰(세대 유지)으로 이어 간다
    r.gate.receive(delivery("req-4", SENTINEL)).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(
        db.scalar::<String>(
            "select state || ':' || generation || ':' || claim_epoch from bot_runs"
        )
        .await,
        "succeeded:1:2"
    );
    assert_eq!(rows_containing(&db, SENTINEL).await, 0);
}

#[tokio::test]
async fn a_dead_process_becomes_a_durable_hold_without_any_redelivery() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 게이트가 접수만 하고 죽었다(Start를 받았으나 결과를 기록하지 못했다)
    let request = private_request("req-5", 5);
    let PrivatePermit::Start(_) = PrivateLedger::begin(&db.store, request).await.unwrap() else {
        panic!("처음 접수는 Start")
    };
    // 리스가 끝나기 전의 재전달은 진행 중으로 본다(새 실행 없음)
    assert!(matches!(
        PrivateLedger::begin(&db.store, private_request("req-5", 5))
            .await
            .unwrap(),
        PrivatePermit::InProgress
    ));
    assert!(
        AdminRead::holds(&db.store, &operator(), None)
            .await
            .unwrap()
            .is_empty(),
        "아직 만료 전"
    );

    db.sql("update bot_runs set lease_until_ms = bot_now_ms() - 1 where private")
        .await;
    // 재전달 없이도 운영자 조회가 만료된 접수를 내구 보류로 옮겨 보여 준다
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "만료된 접수가 보류로 보인다");
    assert_eq!(holds[0].reason, "unknown_outcome");
    assert_eq!(
        db.scalar::<String>("select state || ':' || wait_kind from bot_runs")
            .await,
        "waiting:hold"
    );
    assert!(
        matches!(
            PrivateLedger::begin(&db.store, private_request("req-5", 5))
                .await
                .unwrap(),
            PrivatePermit::Held
        ),
        "재전달은 다시 시작하지 못한다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.held'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_redelivery_cannot_change_the_reply_thread_or_use_a_non_direct_conversation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r = rig(&db);
    let mut first = delivery("req-6", SENTINEL);
    first.conversation.thread = Some("t1".into());
    r.gate.receive(first).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);

    // 같은 키·같은 본문·같은 대화 ID여도 다른 스레드면 같은 접수가 아니다(회신 목적지를 바꾸지 못한다)
    let mut other_thread = delivery("req-6", SENTINEL);
    other_thread.conversation.thread = Some("t2".into());
    assert!(matches!(
        r.gate.receive(other_thread).await,
        Err(Error::Conflict)
    ));
    let no_thread = delivery("req-6", SENTINEL);
    assert!(matches!(
        r.gate.receive(no_thread).await,
        Err(Error::Conflict)
    ));
    let mut other_conversation = delivery("req-6", SENTINEL);
    other_conversation.conversation.thread = Some("t1".into());
    other_conversation.conversation.id = "dm-2".into();
    assert!(matches!(
        r.gate.receive(other_conversation).await,
        Err(Error::Conflict)
    ));
    // 최초와 같은 스레드의 재전달은 호출을 늘리지 않는다
    let mut same = delivery("req-6", SENTINEL);
    same.conversation.thread = Some("t1".into());
    r.gate.receive(same).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);

    // 개인 DM이 아닌 대화는 첫 접수부터 받지 않는다(게이트도 원장도 거절하고 접수 행이 남지 않는다)
    let mut channel = delivery("req-7", SENTINEL);
    channel.conversation.kind = ConversationKind::Channel;
    assert!(matches!(
        r.gate.receive(channel).await,
        Err(Error::Denied | Error::Invalid)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests where request = 'req-7'")
            .await,
        0
    );
}

fn private_request(request: &str, fingerprint: u8) -> bot_core::ledger::PrivateRequest {
    bot_core::ledger::PrivateRequest {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId("direct".into()),
            principal: PrincipalId(SENDER.into()),
            request: RequestId(request.into()),
        },
        fingerprint: [fingerprint; 32],
        revision: Revision("r1".into()),
        agent: AgentId("main".into()),
        tool: ToolId("inbox.receive".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        scopes: scopes(&["inbox.write"]),
    }
}
