//! 저장된 소유자 결정(거절·소비 전 철회)은 등록부 상태보다 먼저다(PG). 수신자가 빠졌거나 확인하지 못해도 이 발생은 «거절»로 끝나고 원래 대화 안내는 0이다.
//! - 실제 브로커+실제 PG로 4조합(Denied/Revoked × 빠짐/일시 오류).
//! - 저장소가 «미발송» 요청도 같은 트랜잭션에서 스스로 거절로 가른다(재전달은 같은 영수증).
//! - 소유자 결정과 종결이 run 잠금으로 직렬화되는 두 순서: 종결이 먼저면 «미발송»이고 결정은 받지 못하며, 결정이 먼저면 «거절»이다.

use crate::support::{delivery::replies, *};
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, MemRoutes, RecordingTracer, snapshot},
};
use bot_core::{
    command::CallResult,
    id::{ApprovalId, Attempt, PrincipalId, Revision},
    port::{AuditLog, Broker, Error},
    schedule::{DeliveryFence, DeliveryTarget, ScheduleDelivery, ScheduledReplies, UnsentReason},
    state::ConversationKind,
};
use sqlx::{Connection, Executor, query};
use std::{sync::Arc, time::Duration};

const REJECTED: &str = r#"{"status":"delivery_rejected"}"#;
const UNROUTED: &str = r#"{"status":"delivery_unrouted"}"#;
const CHANNEL: &str = "T1/C9";

#[derive(Clone, Copy, Debug)]
enum Verdict {
    Denied,
    Revoked,
}

struct Rig {
    db: TestDb,
    service: Arc<BrokerService>,
    routes: Arc<MemRoutes>,
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
    routes.register(
        "personal",
        "slack",
        ConversationKind::Channel,
        CHANNEL,
        CHANNEL,
    );
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

/// 발화한 run이 동결한 초안의 전송 카드를 열어 승인 대기로 둔다.
async fn open_card(rig: &Rig) -> (Attempt, ApprovalId, DeliveryTarget) {
    let db = &rig.db;
    // 원래 대화는 1:1 대화다(안내가 갈 푸시 대화여야 «안내 0»이 의미가 있다)
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
    let first = runs(db).claim().await.unwrap().unwrap().attempt;
    query("insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id) values ('personal', $1, 'slack', 'channel', $2)")
        .bind(&id.0)
        .bind(CHANNEL)
        .execute(db.store.pool())
        .await
        .unwrap();
    let draft = replies(db)
        .draft(first.clone(), Some(done("승인한 답")))
        .await
        .unwrap()
        .unwrap();
    let Ok(CallResult::Awaiting(nonce)) = rig.service.release(first).await else {
        panic!("소유자의 승인이 필요하다")
    };
    (first_of(db, &id.0).await, nonce, draft.target)
}

/// 이 run의 최초 시도(재개 전). 카드를 연 시도는 대기로 넘어갔다.
async fn first_of(db: &TestDb, run: &str) -> Attempt {
    let _ = (db, run);
    Attempt {
        profile: bot_core::id::ProfileId("personal".into()),
        run: bot_core::id::RunId(run.into()),
        claim: None,
        generation: bot_core::id::Generation(1),
    }
}

async fn decide(rig: &Rig, nonce: &ApprovalId, allow: bool) -> Result<(), Error> {
    rig.service
        .decide(nonce.clone(), PrincipalId(OWNER.into()), allow)
        .await
}

/// 소유자가 결정한 뒤 이어 받은 시도. `Revoked`는 승인으로 깨어난 시도가 입구를 부르기 전에 철회한 경우다(그 시도는 그대로 잡고 있다).
async fn decide_and_resume(rig: &Rig, nonce: &ApprovalId, verdict: Verdict) -> Attempt {
    match verdict {
        Verdict::Denied => {
            decide(rig, nonce, false).await.unwrap();
            runs(&rig.db).claim().await.unwrap().unwrap().attempt
        }
        Verdict::Revoked => {
            decide(rig, nonce, true).await.unwrap();
            let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
            decide(rig, nonce, false).await.unwrap();
            resumed
        }
    }
}

fn receipt(result: Result<CallResult, Error>) -> String {
    match result {
        Ok(CallResult::Done(payload)) => payload.0,
        other => panic!("영수증이 필요하다: {other:?}"),
    }
}

async fn outbox_total(db: &TestDb) -> i64 {
    db.scalar("select count(*) from bot_outbox").await
}

async fn unsent_audit(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar(
        "select verdict from bot_audit where action = 'delivery.unsent' order by seq",
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

#[tokio::test]
async fn a_stored_owner_decision_ends_the_occurrence_as_rejected_whatever_the_registry_says() {
    for verdict in [Verdict::Denied, Verdict::Revoked] {
        for gone in [true, false] {
            let what = format!("{verdict:?} × {}", if gone { "빠짐" } else { "일시 오류" });
            let Some(rig) = rig().await else {
                return;
            };
            let (_, nonce, _) = open_card(&rig).await;
            let resumed = decide_and_resume(&rig, &nonce, verdict).await;
            let before = outbox_total(&rig.db).await;
            if gone {
                rig.routes.unregister(CHANNEL);
            } else {
                rig.routes.outage(true);
            }
            let issued = receipt(rig.service.release(resumed.clone()).await);
            assert_eq!(issued, REJECTED, "{what}");
            assert_eq!(
                state_of_run(&rig.db, &resumed.run).await,
                "succeeded",
                "{what}"
            );
            assert_eq!(
                delivery_row(&rig.db).await,
                ("dropped".into(), Some("rejected".into())),
                "{what}"
            );
            assert_eq!(outbox_total(&rig.db).await, before, "{what}: 안내·발송 0");
            assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"], "{what}");
            // 재전달(응답을 잃은 재시도)은 등록부를 다시 보지 않고 같은 영수증이다
            assert_eq!(
                receipt(rig.service.release(resumed).await),
                REJECTED,
                "{what}"
            );
            assert_eq!(unsent_audit(&rig.db).await.len(), 1, "{what}");
            assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
        }
    }
}

#[tokio::test]
async fn the_store_settles_an_unrouted_request_as_rejected_when_the_owners_decision_is_stored() {
    for verdict in [Verdict::Denied, Verdict::Revoked] {
        let Some(rig) = rig().await else {
            return;
        };
        let (_, nonce, target) = open_card(&rig).await;
        let resumed = decide_and_resume(&rig, &nonce, verdict).await;
        let before = outbox_total(&rig.db).await;
        let finish = |reason| {
            replies(&rig.db).finish_unsent(
                DeliveryFence::Claim(resumed.clone()),
                target.clone(),
                reason,
            )
        };
        // 브로커를 거치지 않고 저장소에 «미발송»을 요청해도 저장된 결정이 먼저다
        assert_eq!(
            finish(UnsentReason::Unrouted).await.unwrap().0,
            REJECTED,
            "{verdict:?}"
        );
        assert_eq!(outbox_total(&rig.db).await, before, "{verdict:?}: 안내 0");
        assert_eq!(
            delivery_row(&rig.db).await,
            ("dropped".into(), Some("rejected".into()))
        );
        // 어느 이유로 다시 요청해도 같은 영수증이고 기록·안내는 늘지 않는다
        for reason in [UnsentReason::Unrouted, UnsentReason::Rejected] {
            assert_eq!(
                finish(reason).await.unwrap().0,
                REJECTED,
                "{verdict:?} {reason:?}"
            );
        }
        assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"]);
        assert_eq!(outbox_total(&rig.db).await, before);
        assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
    }
}

async fn waiting_on_the_run_lock(db: &TestDb) -> i64 {
    db.scalar(&format!(
        "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_runs%'",
        db.schema
    ))
    .await
}

async fn wait_for_waiters(db: &TestDb, count: i64, task_done: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while waiting_on_the_run_lock(db).await != count {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "run 잠금 대기자가 {count}이 되지 않는다"
        );
        assert!(!task_done(), "잠금을 기다리지 않고 끝났다");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// run 잠금을 쥔 연결 하나를 두고, 먼저 `first`를 시작해 대기를 확인한 다음 `second`를 시작해 대기를 확인하고 잠금을 놓는다(잠금 대기열은 도착 순서다).
async fn race<A, B>(rig: &Rig, run: &str, first: A, second: B) -> (A::Output, B::Output)
where
    A: std::future::Future + Send + 'static,
    A::Output: Send + 'static,
    B: std::future::Future + Send + 'static,
    B::Output: Send + 'static,
{
    let mut holder = rig.db.raw().await;
    let mut tx = holder.begin().await.unwrap();
    tx.execute(
        sqlx::query("select 1 from bot_runs where profile = 'personal' and id = $1 for update")
            .bind(run),
    )
    .await
    .unwrap();
    let first = tokio::spawn(first);
    wait_for_waiters(&rig.db, 1, || first.is_finished()).await;
    let second = tokio::spawn(second);
    wait_for_waiters(&rig.db, 2, || second.is_finished()).await;
    tx.commit().await.unwrap();
    (first.await.unwrap(), second.await.unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn when_the_unrouted_finish_wins_the_run_lock_the_late_revocation_is_refused() {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, target) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    let before = outbox_total(&rig.db).await;
    let pg = Arc::new(rig.db.store.for_activation(Revision("set-1".into())));
    let (finish_pg, finish_at, finish_target) = (pg, resumed.clone(), target.clone());
    let (service, owner_nonce) = (rig.service.clone(), nonce.clone());
    let (finished, revoked) = race(
        &rig,
        &resumed.run.0,
        async move {
            finish_pg
                .finish_unsent(
                    DeliveryFence::Claim(finish_at),
                    finish_target,
                    UnsentReason::Unrouted,
                )
                .await
        },
        async move {
            service
                .decide(owner_nonce, PrincipalId(OWNER.into()), false)
                .await
        },
    )
    .await;
    // 종결이 먼저: «미발송»이고 안내 한 건. 늦은 철회는 받지 못한다(이미 닫힌 카드)
    assert_eq!(finished.unwrap().0, UNROUTED);
    assert!(revoked.is_err(), "{revoked:?}");
    assert_eq!(outbox_total(&rig.db).await, before + 1, "안내 한 건");
    assert_eq!(unsent_audit(&rig.db).await, ["delivery_unrouted"]);
    assert_eq!(
        delivery_row(&rig.db).await,
        ("dropped".into(), Some("unrouted".into()))
    );
    assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn when_the_revocation_wins_the_run_lock_the_unrouted_finish_ends_as_rejected() {
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, target) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    let before = outbox_total(&rig.db).await;
    let pg = Arc::new(rig.db.store.for_activation(Revision("set-1".into())));
    let (finish_pg, finish_at, finish_target) = (pg, resumed.clone(), target.clone());
    let (service, owner_nonce) = (rig.service.clone(), nonce.clone());
    let (revoked, finished) = race(
        &rig,
        &resumed.run.0,
        async move {
            service
                .decide(owner_nonce, PrincipalId(OWNER.into()), false)
                .await
        },
        async move {
            finish_pg
                .finish_unsent(
                    DeliveryFence::Claim(finish_at),
                    finish_target,
                    UnsentReason::Unrouted,
                )
                .await
        },
    )
    .await;
    // 결정이 먼저: 저장소가 같은 트랜잭션에서 그 결정을 보고 «거절»로 끝낸다(안내 0)
    revoked.unwrap();
    assert_eq!(finished.unwrap().0, REJECTED);
    assert_eq!(outbox_total(&rig.db).await, before, "안내 0");
    assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"]);
    assert_eq!(
        delivery_row(&rig.db).await,
        ("dropped".into(), Some("rejected".into()))
    );
    assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revocation_between_the_last_route_check_and_the_consumption_ends_as_rejected() {
    // 수신 경로는 정상이다. 마지막 경로 확인 뒤 승인을 소비하기 직전에 소유자 철회가 커밋된다: 발송 0, 이 발생은 «거절»로 끝난다(재호출도 같은 영수증)
    let Some(rig) = rig().await else {
        return;
    };
    let (_, nonce, _) = open_card(&rig).await;
    decide(&rig, &nonce, true).await.unwrap();
    let resumed = runs(&rig.db).claim().await.unwrap().unwrap().attempt;
    let before = outbox_total(&rig.db).await;
    let (service, owner, handle) = (
        rig.service.clone(),
        nonce.clone(),
        tokio::runtime::Handle::current(),
    );
    // 이번 `release`의 등록부 확인: ① 초안 직후 ② 소비 직전. ②에 닿기 직전에 철회가 커밋된다(등록부는 그대로 같은 경로다)
    rig.routes.on_nth_ask(rig.routes.asked() + 2, move || {
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                service
                    .decide(owner.clone(), PrincipalId(OWNER.into()), false)
                    .await
                    .unwrap();
            });
        });
    });
    assert_eq!(
        receipt(rig.service.release(resumed.clone()).await),
        REJECTED
    );
    assert_eq!(
        rig.db
            .scalar::<String>(&format!(
                "select state from bot_approvals where nonce = '{}'",
                nonce.0
            ))
            .await,
        "revoked"
    );
    assert_eq!(state_of_run(&rig.db, &resumed.run).await, "succeeded");
    assert_eq!(
        delivery_row(&rig.db).await,
        ("dropped".into(), Some("rejected".into()))
    );
    assert_eq!(outbox_total(&rig.db).await, before, "발송·안내 0");
    assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"]);
    // 재호출은 같은 영수증이고 아무것도 더 쓰지 않는다
    assert_eq!(receipt(rig.service.release(resumed).await), REJECTED);
    assert_eq!(unsent_audit(&rig.db).await.len(), 1);
    assert_eq!(outbox_total(&rig.db).await, before);
    assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
}
