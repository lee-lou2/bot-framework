//! 수신자가 등록부에서 빠졌을 때 «미발송(`delivery_unrouted`)»으로 끝나는 전체 흐름(실제 브로커 + 실제 PG). 소유자 결정 우선은 `delivery_unsent_precedence.rs`가 본다.
//! - 카드를 열기 전·카드를 연 뒤 클릭할 때·승인한 뒤 소비 전에 빠지면 각각 어떻게 끝나는가: 원래 대화 안내 한 건·초안 종결·옛 카드 무효·run 성공·같은 영수증, 목적지 발송 0.
//! - 등록부가 돌아와도 끝난 발생은 되살아나지 않고, 이미 보낸 답은 미발송으로 바뀌지 않는다.
//! - 같은 버튼을 동시에 여러 번 눌러도 종결과 안내는 한 번이다.

use crate::support::{
    delivery::{outbox_rows, replies},
    *,
};
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, MemRoutes, RecordingTracer, snapshot},
};
use bot_core::{
    command::CallResult,
    id::{ApprovalId, Attempt, PrincipalId, Revision, RunId},
    port::{AuditLog, Broker, Error},
    schedule::{DeliveryFence, DeliveryTarget, ScheduleDelivery, UNROUTED_NOTICE, UnsentReason},
    state::ConversationKind,
};
use sqlx::{Connection, Executor, query};
use std::{sync::Arc, time::Duration};

const UNROUTED: &str = r#"{"status":"delivery_unrouted"}"#;
const QUEUED: &str = r#"{"queued":1}"#;
const CHANNEL: &str = "T1/C9";

struct Rig {
    db: TestDb,
    service: Arc<BrokerService>,
    routes: Arc<MemRoutes>,
}

fn register(routes: &MemRoutes) {
    routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        CHANNEL,
        CHANNEL,
    );
}

async fn rig() -> Option<Rig> {
    let db = TestDb::new().await?;
    let catalog = Arc::new(FakeCatalog::new());
    let grants = ["run", "schedule.create"];
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &grants)
            .member(OWNER, &["owner"])
            .policy_scopes(&grants)
            .agent("main")
            .build(),
    );
    let pg = Arc::new(db.store.for_activation(Revision("set-1".into())));
    let routes = Arc::new(MemRoutes::new());
    register(&routes);
    let service = BrokerService::new(
        Parts {
            catalog,
            ledger: pg.clone(),
            effects: pg.clone(),
            memory: pg.clone(),
            audit: pg.clone(),
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_replies(pg)
    .with_routes(routes.clone());
    Some(Rig {
        db,
        service: Arc::new(service),
        routes,
    })
}

/// 발화한 run이 최종 답을 동결했다(카드는 아직 없다). 돌려주는 것은 그 run을 쥔 시도와 동결한 수신자다.
async fn start(rig: &Rig) -> (Attempt, DeliveryTarget) {
    let db = &rig.db;
    // 원래 대화는 1:1 대화다(안내가 갈 푸시 대화)
    let mut request = accept_as(
        "personal",
        "delivery",
        OWNER,
        ConversationKind::Direct,
        "c1",
        "예약 회신",
    );
    request.scopes = scopes(&["run", "schedule.create"]);
    request.approver = Some(PrincipalId(OWNER.into()));
    let id = runs(db).accept(request).await.unwrap();
    let claimed = runs(db).claim().await.unwrap().unwrap().attempt;
    query("insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id) values ('personal', $1, 'slack', 'channel', $2)")
        .bind(&id.0)
        .bind(CHANNEL)
        .execute(db.store.pool())
        .await
        .unwrap();
    let draft = replies(db)
        .draft(claimed.clone(), Some(done("승인한 답")))
        .await
        .unwrap()
        .unwrap();
    (claimed, draft.target)
}

/// 카드를 열어 승인 대기로 둔다.
async fn open_card(rig: &Rig) -> (Attempt, ApprovalId, DeliveryTarget) {
    let (claimed, target) = start(rig).await;
    let Ok(CallResult::Awaiting(nonce)) = rig.service.release(claimed.clone()).await else {
        panic!("소유자의 승인이 필요하다")
    };
    (claimed, nonce, target)
}

async fn decide(rig: &Rig, nonce: &ApprovalId, allow: bool) -> Result<(), Error> {
    rig.service
        .decide(nonce.clone(), PrincipalId(OWNER.into()), allow)
        .await
}

fn receipt(result: Result<CallResult, Error>) -> String {
    match result {
        Ok(CallResult::Done(payload)) => payload.0,
        other => panic!("영수증이 필요하다: {other:?}"),
    }
}

/// 원래 대화(c1)에 간 «미발송» 고정 안내들. (승인 카드도 원래 대화로 가므로 카드 행은 세지 않는다.)
async fn original(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar(
        "select body from bot_outbox where conv_id = 'c1' and body = $1 order by seq",
    )
    .bind(notice())
    .fetch_all(db.store.pool())
    .await
    .unwrap()
}

fn notice() -> String {
    format!(r#"{{"text":"{UNROUTED_NOTICE}"}}"#)
}

async fn unsent_audit(db: &TestDb) -> Vec<(String, String)> {
    sqlx::query_as(
        "select actor, verdict from bot_audit where action = 'delivery.unsent' order by seq",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap()
}

async fn delivery_row(db: &TestDb) -> (String, Option<String>) {
    sqlx::query_as("select state, unsent from bot_deliveries")
        .fetch_one(db.store.pool())
        .await
        .unwrap()
}

async fn approval_state(db: &TestDb, nonce: &ApprovalId) -> String {
    db.scalar(&format!(
        "select state from bot_approvals where nonce = '{}'",
        nonce.0
    ))
    .await
}

#[tokio::test]
async fn a_recipient_removed_before_the_card_ends_the_occurrence_with_no_card_one_notice_and_the_same_receipt_on_redelivery()
 {
    let Some(rig) = rig().await else {
        return;
    };
    let (claimed, _) = start(&rig).await;
    rig.routes.unregister(CHANNEL);
    assert_eq!(
        receipt(rig.service.release(claimed.clone()).await),
        UNROUTED
    );
    let db = &rig.db;
    assert_eq!(state_of_run(db, &claimed.run).await, "succeeded");
    assert_eq!(
        delivery_row(db).await,
        ("dropped".into(), Some("unrouted".into()))
    );
    assert_eq!(
        original(db).await,
        [notice()],
        "원래 대화에 고정 안내 한 건"
    );
    assert_eq!(outbox_rows(db, CHANNEL).await, 0, "목적지 발송 0");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0,
        "카드를 열지 않았다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls where call = 'schedule-deliver'")
            .await,
        0,
        "호출 기록도 만들지 않았다"
    );
    assert_eq!(
        unsent_audit(db).await,
        [("system".to_owned(), "delivery_unrouted".to_owned())]
    );
    // 응답을 잃은 재전달: 등록부가 돌아와도 같은 영수증이고 아무것도 더 쓰지 않는다
    register(&rig.routes);
    assert_eq!(
        receipt(rig.service.release(claimed.clone()).await),
        UNROUTED
    );
    assert_eq!(original(db).await.len(), 1);
    assert_eq!(unsent_audit(db).await.len(), 1);
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_recipient_removed_after_the_card_ends_it_on_the_owners_click_and_the_old_button_stays_dead()
 {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, _) = open_card(&rig).await;
    let db = &rig.db;
    assert_eq!(approval_state(db, &nonce).await, "pending");
    rig.routes.unregister(CHANNEL);
    // 클릭은 낡은 버튼이다(`Stale`). 같은 커밋에서 발생이 «미발송»으로 끝난다
    assert!(matches!(
        decide(&rig, &nonce, true).await,
        Err(Error::Stale)
    ));
    assert_eq!(approval_state(db, &nonce).await, "superseded");
    let run = db.scalar::<String>("select id from bot_runs").await;
    assert_eq!(state_of_run(db, &RunId(run)).await, "succeeded");
    assert_eq!(
        delivery_row(db).await,
        ("dropped".into(), Some("unrouted".into()))
    );
    assert_eq!(original(db).await, [notice()]);
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(
        db.scalar::<String>("select result from bot_calls where call = 'schedule-deliver'")
            .await,
        UNROUTED,
        "열려 있던 전송 호출은 미발송 영수증으로 끝난다"
    );
    assert_eq!(
        unsent_audit(db).await,
        [(OWNER.to_owned(), "delivery_unrouted".to_owned())]
    );
    // 같은 버튼·거절 버튼을 다시 눌러도, 등록부가 돌아와도 죽은 버튼이다: 안내·감사는 그대로, 발송은 0
    register(&rig.routes);
    for allow in [true, false, true] {
        assert!(
            matches!(decide(&rig, &nonce, allow).await, Err(Error::Stale)),
            "{allow}"
        );
    }
    assert_eq!(original(db).await.len(), 1);
    assert_eq!(unsent_audit(db).await.len(), 1);
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    // 오너가 아닌 사람이 누르면 아무것도 끝나지 않는다는 것은 다른 발생으로 확인한다(아래 시험)
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_click_by_anyone_but_the_owner_never_ends_the_occurrence() {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, _) = open_card(&rig).await;
    rig.routes.unregister(CHANNEL);
    let denied = rig
        .service
        .decide(nonce.clone(), PrincipalId("local:alice".into()), true)
        .await;
    assert!(matches!(denied, Err(Error::Denied)), "{denied:?}");
    let db = &rig.db;
    assert_eq!(approval_state(db, &nonce).await, "pending");
    assert_eq!(delivery_row(db).await.0, "drafted");
    assert!(original(db).await.is_empty());
    assert!(unsent_audit(db).await.is_empty());
}

#[tokio::test]
async fn a_recipient_removed_after_the_approval_ends_it_at_the_resumed_attempt_and_the_approval_is_never_used()
 {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, _) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    rig.routes.unregister(CHANNEL);
    assert_eq!(
        receipt(rig.service.release(resumed.clone()).await),
        UNROUTED
    );
    let db = &rig.db;
    // 승인은 소비되지 않고 무효가 된다
    assert_eq!(approval_state(db, &nonce).await, "superseded");
    assert_eq!(state_of_run(db, &resumed.run).await, "succeeded");
    assert_eq!(original(db).await, [notice()]);
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(
        unsent_audit(db).await,
        [("system".to_owned(), "delivery_unrouted".to_owned())]
    );
    // 등록부가 돌아와도 그 승인으로 다시 보내지 않는다: 같은 영수증이다
    register(&rig.routes);
    assert_eq!(
        receipt(rig.service.release(resumed.clone()).await),
        UNROUTED
    );
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(original(db).await.len(), 1);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_recipient_removed_between_the_last_registry_check_and_the_consumption_ends_it_unrouted_and_sends_nothing()
 {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, _) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    // 이번 `release`의 등록부 확인: ① 초안 직후 ② 승인을 소비하기 직전. ②를 막 하려는 순간 수신자가 빠진다
    let routes = rig.routes.clone();
    rig.routes
        .on_nth_ask(rig.routes.asked() + 2, move || routes.unregister(CHANNEL));
    assert_eq!(
        receipt(rig.service.release(resumed.clone()).await),
        UNROUTED
    );
    let db = &rig.db;
    assert_eq!(
        approval_state(db, &nonce).await,
        "superseded",
        "승인을 소비하지 않았다"
    );
    assert_eq!(outbox_rows(db, CHANNEL).await, 0, "발송 0");
    assert_eq!(original(db).await, [notice()]);
    assert_eq!(
        delivery_row(db).await,
        ("dropped".into(), Some("unrouted".into()))
    );
    assert_eq!(state_of_run(db, &resumed.run).await, "succeeded");
    // 재전달은 같은 영수증이다
    register(&rig.routes);
    assert_eq!(receipt(rig.service.release(resumed).await), UNROUTED);
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_answer_that_was_already_registered_is_never_turned_into_unrouted_afterwards() {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, target) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    assert_eq!(receipt(rig.service.release(resumed.clone()).await), QUEUED);
    let db = &rig.db;
    assert_eq!(outbox_rows(db, CHANNEL).await, 1);
    // 보낸 뒤에 수신자가 빠져도 같은 영수증이고, 미발송 안내·종결은 없다
    rig.routes.unregister(CHANNEL);
    assert_eq!(receipt(rig.service.release(resumed.clone()).await), QUEUED);
    assert!(original(db).await.is_empty(), "안내 0");
    // 저장소에 직접 «미발송»을 요청해도 등록된 답은 취소할 수 없다
    let late = replies(db)
        .finish_unsent(
            DeliveryFence::Claim(resumed.clone()),
            target,
            UnsentReason::Unrouted,
        )
        .await;
    assert!(matches!(late, Err(Error::Conflict)), "{late:?}");
    assert_eq!(outbox_rows(db, CHANNEL).await, 1);
    assert_eq!(delivery_row(db).await.1, None);
    assert!(unsent_audit(db).await.is_empty());
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

async fn waiting_on_the_run_lock(db: &TestDb) -> i64 {
    db.scalar(&format!(
        "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_runs%'",
        db.schema
    ))
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn several_clicks_on_the_same_dead_card_end_the_occurrence_exactly_once() {
    let Some(rig) = rig().await else {
        return;
    };
    let (claimed, nonce, _) = open_card(&rig).await;
    rig.routes.unregister(CHANNEL);
    // 클릭 여러 개가 같은 run 잠금 앞에서 동시에 기다리게 한 다음 한꺼번에 놓는다(시간 추정 없음)
    let mut holder = rig.db.raw().await;
    let mut tx = holder.begin().await.unwrap();
    tx.execute(
        sqlx::query("select 1 from bot_runs where profile = 'personal' and id = $1 for update")
            .bind(&claimed.run.0),
    )
    .await
    .unwrap();
    const CLICKS: i64 = 5;
    let mut clicks = Vec::new();
    for _ in 0..CLICKS {
        let (service, nonce) = (rig.service.clone(), nonce.clone());
        clicks.push(tokio::spawn(async move {
            service.decide(nonce, PrincipalId(OWNER.into()), true).await
        }));
    }
    let started = std::time::Instant::now();
    while waiting_on_the_run_lock(&rig.db).await < CLICKS {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "클릭이 잠금 앞에 모이지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tx.commit().await.unwrap();
    for click in clicks {
        assert!(matches!(click.await.unwrap(), Err(Error::Stale)));
    }
    let db = &rig.db;
    assert_eq!(original(db).await, [notice()], "안내는 한 건");
    assert_eq!(unsent_audit(db).await.len(), 1, "종결 감사는 한 건");
    assert_eq!(
        delivery_row(db).await,
        ("dropped".into(), Some("unrouted".into()))
    );
    assert_eq!(outbox_rows(db, CHANNEL).await, 0);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where state = 'succeeded'")
            .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}
