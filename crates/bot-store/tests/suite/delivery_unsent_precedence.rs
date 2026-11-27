//! Stored owner decisions (denial or pre-consumption revocation) take precedence over route registry state (PG).
//!
//! Covers all Denied/Revoked × missing/outage combinations, store-side `finish_unsent`
//! promotion to `delivery_rejected`, and run-lock serialization races between decision and unsent finish.

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

async fn open_card(rig: &Rig) -> (Attempt, ApprovalId, DeliveryTarget) {
    let db = &rig.db;
    // Use a push-capable direct origin so asserting zero origin notices is meaningful.
    let mut request = accept_as(
        "personal",
        "delivery",
        OWNER,
        ConversationKind::Direct,
        "c1",
        "scheduled reply",
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
        .draft(first.clone(), Some(done("approved reply")))
        .await
        .unwrap()
        .unwrap();
    let Ok(CallResult::Awaiting(nonce)) = rig.service.release(first).await else {
        panic!("owner approval required")
    };
    (first_of(db, &id.0).await, nonce, draft.target)
}

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
        other => panic!("expected receipt: {other:?}"),
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
            let what = format!("{verdict:?} x {}", if gone { "missing" } else { "outage" });
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
            assert_eq!(
                outbox_total(&rig.db).await,
                before,
                "{what}: no notice or delivery"
            );
            assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"], "{what}");
            // Redelivery returns the committed receipt without re-checking routes.
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
        assert_eq!(
            finish(UnsentReason::Unrouted).await.unwrap().0,
            REJECTED,
            "{verdict:?}"
        );
        assert_eq!(
            outbox_total(&rig.db).await,
            before,
            "{verdict:?}: no notice"
        );
        assert_eq!(
            delivery_row(&rig.db).await,
            ("dropped".into(), Some("rejected".into()))
        );
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
            "expected {count} waiters on the run lock"
        );
        assert!(!task_done(), "finished without waiting for the lock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Holds the run row lock, queues `first` then `second` on the PG lock FIFO, and releases.
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
    assert_eq!(finished.unwrap().0, UNROUTED);
    assert!(revoked.is_err(), "{revoked:?}");
    assert_eq!(outbox_total(&rig.db).await, before + 1, "one origin notice");
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
    revoked.unwrap();
    assert_eq!(finished.unwrap().0, REJECTED);
    assert_eq!(outbox_total(&rig.db).await, before, "no origin notice");
    assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"]);
    assert_eq!(
        delivery_row(&rig.db).await,
        ("dropped".into(), Some("rejected".into()))
    );
    assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revocation_between_the_last_route_check_and_the_consumption_ends_as_rejected() {
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
    // Commit owner revocation right before the second (pre-consumption) route check in `release`.
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
    assert_eq!(outbox_total(&rig.db).await, before, "no delivery or notice");
    assert_eq!(unsent_audit(&rig.db).await, ["delivery_rejected"]);
    assert_eq!(receipt(rig.service.release(resumed).await), REJECTED);
    assert_eq!(unsent_audit(&rig.db).await.len(), 1);
    assert_eq!(outbox_total(&rig.db).await, before);
    assert_eq!(rig.db.store.verify().await.unwrap().first_bad, None);
}
