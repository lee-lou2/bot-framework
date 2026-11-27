//! API-origin approval opening under the host active-set lock (PG).
//!
//! Verifies active-set serialization, requester/team pinning from admission evidence, and
//! Slack receipt team checks while leaving legacy runs unchanged.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiState, ApprovalBlock},
    cards::{CardKey, CardOutbox, CardOutcome},
    id::*,
    ledger::{ApprovalState, CallLedger, CallPhase, Decision, OpenApproval},
    port::Error,
    state::*,
};
use sqlx::{Connection, Executor};
use std::time::Duration;

const APPROVER: &str = "slack:home/U1";

async fn api_run(db: &TestDb, key: &str, team: Option<&str>) -> RunContext {
    let mut req = request(key, "build", "assistant", 1, 8);
    req.team = team.map(str::to_owned);
    accepted(admit(db, req).await);
    let ctx = claimed(db).await;
    begin_call(db, &ctx.attempt, "c1").await;
    ctx
}

fn open_as(
    ctx: &RunContext,
    nonce: &str,
    requester: &PrincipalId,
    delivery: Option<&str>,
) -> OpenApproval {
    let mut open = open_approval(Approval {
        requester: requester.clone(),
        approver: PrincipalId(APPROVER.into()),
        ..binding(&ctx.attempt, "c1", nonce)
    });
    open.delivery = delivery.map(|d| BindingId(d.into()));
    open
}

async fn rows(db: &TestDb) -> (i64, i64) {
    (
        db.scalar("select count(*) from bot_approvals").await,
        db.scalar("select count(*) from bot_card_jobs").await,
    )
}

async fn phase_of(db: &TestDb, ctx: &RunContext) -> CallPhase {
    calls(db)
        .lookup(ctx.attempt.clone(), call("c1"))
        .await
        .unwrap()
        .unwrap()
        .phase
}

#[tokio::test]
async fn a_replacement_that_commits_first_leaves_neither_the_local_card_nor_the_slack_job() {
    for (delivery, jobs) in [(None, 0), (Some("team"), 1)] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let ctx = api_run(&db, "k1", Some("T1")).await;
        let handle = pinned(&db);
        db.sql("update bot_active set revision = 'set-2'").await;
        let got =
            CallLedger::open_approval(&handle, open_as(&ctx, "n1", &principal("build"), delivery))
                .await;
        assert!(matches!(got, Err(Error::Stale)), "{delivery:?}: {got:?}");
        assert_eq!(
            rows(&db).await,
            (0, 0),
            "{delivery:?}: neither card nor job should be created"
        );
        assert!(matches!(
            phase_of(&db, &ctx).await,
            CallPhase::Open { approval: None }
        ));
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
        assert_eq!(
            lookup(&db, "k1", "build").await.unwrap().state,
            ApiState::Active
        );

        db.sql("update bot_active set revision = 'set-1'").await;
        CallLedger::open_approval(&handle, open_as(&ctx, "n1", &principal("build"), delivery))
            .await
            .unwrap();
        assert_eq!(rows(&db).await, (1, jobs), "{delivery:?}");
        assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "waiting");
    }
}

/// Attempts `open_approval` while another connection holds `bot_active FOR UPDATE`.
async fn open_during_a_replacement(
    delivery: Option<&str>,
    replace: bool,
) -> Option<(Result<(), Error>, (i64, i64), String)> {
    let db = TestDb::new().await?;
    let ctx = api_run(&db, "k1", Some("T1")).await;
    let handle = pinned(&db);
    let open = open_as(&ctx, "n1", &principal("build"), delivery);
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&handle, open).await });
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = db
            .scalar(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_active%'",
                db.schema
            ))
            .await;
        if waiting == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "open_approval did not wait for the active-set lock"
        );
        assert!(!task.is_finished(), "finished without waiting for the lock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if replace {
        tx.execute("update bot_active set revision = 'set-2'")
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let result = task.await.unwrap();
    Some((
        result,
        rows(&db).await,
        state_of_run(&db, &ctx.attempt.run).await,
    ))
}

#[tokio::test]
async fn opening_waits_for_a_replacement_in_flight_and_judges_after_it() {
    for (delivery, jobs) in [(None, 0), (Some("team"), 1)] {
        let Some((result, counted, state)) = open_during_a_replacement(delivery, true).await else {
            return;
        };
        assert!(
            matches!(result, Err(Error::Stale)),
            "{delivery:?}: {result:?}"
        );
        assert_eq!(counted, (0, 0), "{delivery:?}");
        assert_eq!(state, "leased");
        let (result, counted, state) = open_during_a_replacement(delivery, false)
            .await
            .expect("DB");
        result.unwrap();
        assert_eq!(counted, (1, jobs), "{delivery:?}");
        assert_eq!(state, "waiting");
    }
}

#[tokio::test]
async fn a_handle_that_pins_no_set_cannot_open_an_api_origin_approval_but_a_legacy_run_still_opens()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    for delivery in [None, Some("team")] {
        let got = calls(&db)
            .open_approval(open_as(&ctx, "n1", &principal("build"), delivery))
            .await;
        assert!(matches!(got, Err(Error::Denied)), "{delivery:?}: {got:?}");
        assert_eq!(rows(&db).await, (0, 0), "{delivery:?}");
    }
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");

    // Legacy runs still open approvals with unpinned handles and after set changes.
    let legacy = leased(&db, "legacy").await;
    begin_call(&db, &legacy.attempt, "c9").await;
    db.sql("update bot_active set revision = 'set-2'").await;
    calls(&db)
        .open_approval(open_approval(binding(&legacy.attempt, "c9", "n9")))
        .await
        .unwrap();
    assert_eq!(rows(&db).await, (1, 0));
    assert_eq!(state_of_run(&db, &legacy.attempt.run).await, "waiting");
}

#[tokio::test]
async fn the_requester_and_the_delivery_team_come_from_the_original_request_row() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    for requester in [principal("other"), PrincipalId("local:owner".into())] {
        for delivery in [None, Some("team")] {
            let got =
                CallLedger::open_approval(&pinned(&db), open_as(&ctx, "n1", &requester, delivery))
                    .await;
            assert!(
                matches!(got, Err(Error::Denied)),
                "{requester:?}/{delivery:?}: {got:?}"
            );
            assert_eq!(rows(&db).await, (0, 0));
        }
    }
    assert_eq!(state_of_run(&db, &ctx.attempt.run).await, "leased");
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    assert_eq!(rows(&db).await, (1, 1));

    // Without a pinned admission team, Slack DM delivery is denied while local approval is allowed.
    let bare = api_run(&db, "k2", None).await;
    let got = CallLedger::open_approval(
        &pinned(&db),
        open_as(&bare, "n2", &principal("build"), Some("team")),
    )
    .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rows(&db).await, (1, 1), "no second approval or job");
    assert_eq!(state_of_run(&db, &bare.attempt.run).await, "leased");
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&bare, "n2", &principal("build"), None),
    )
    .await
    .unwrap();
    assert_eq!(rows(&db).await, (2, 1));
}

async fn decide_as(db: &TestDb, nonce: &str, actor: &str) -> Result<ApprovalState, Error> {
    calls(db)
        .decide(Decision {
            nonce: ApprovalId(nonce.into()),
            actor: PrincipalId(actor.into()),
            allow: true,
        })
        .await
}

fn posted(team: &str) -> CardOutcome {
    CardOutcome::Posted {
        team: team.into(),
        dm: "D1".into(),
        ts: "1.5".into(),
    }
}

async fn sending(db: &TestDb, nonce: &str) -> CardKey {
    let job = CardOutbox::claim(&db.store, 8).await.unwrap().remove(0);
    assert_eq!(job.key.nonce.0, nonce);
    CardOutbox::begin(&db.store, job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    job.key
}

#[tokio::test]
async fn a_receipt_from_another_team_is_no_receipt_and_closes_the_card_as_a_changed_route() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    let key = sending(&db, "n1").await;
    let got = CardOutbox::settle(&db.store, key.clone(), posted("T2")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        "closed|route_changed"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs where team is not null or dm is not null or ts is not null")
            .await,
        0,
        "no receipt must be recorded"
    );
    assert!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .delivery
            .is_none()
    );
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::ApprovalBlocked(ApprovalBlock::RouteChanged)
    );
    assert!(decide_as(&db, "n1", APPROVER).await.is_err());
    assert_eq!(
        db.scalar::<String>("select state || '|' || decided_by from bot_approvals")
            .await,
        "denied|system"
    );
    let again = CardOutbox::settle(&db.store, key.clone(), posted("T2")).await;
    assert!(matches!(again, Err(Error::Conflict)), "{again:?}");
    let right = CardOutbox::settle(&db.store, key, posted("T1")).await;
    assert!(matches!(right, Err(Error::Conflict)), "{right:?}");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
}

#[tokio::test]
async fn a_card_job_without_the_pinned_team_never_becomes_a_receipt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    // Simulate a legacy row missing `origin_team`; any posted team must be rejected.
    db.sql("update bot_card_jobs set origin_team = null").await;
    let key = sending(&db, "n1").await;
    let got = CardOutbox::settle(&db.store, key, posted("T1")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        "closed|route_changed"
    );
    assert!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .delivery
            .is_none()
    );
    assert_ne!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
}

#[tokio::test]
async fn the_pinned_team_matches_the_receipt_and_a_redelivered_settlement_is_the_same_result() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ctx = api_run(&db, "k1", Some("T1")).await;
    CallLedger::open_approval(
        &pinned(&db),
        open_as(&ctx, "n1", &principal("build"), Some("team")),
    )
    .await
    .unwrap();
    let key = sending(&db, "n1").await;
    CardOutbox::settle(&db.store, key.clone(), posted("T1"))
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    CardOutbox::settle(&db.store, key, posted("T1"))
        .await
        .unwrap();
    assert_eq!(
        decide_as(&db, "n1", APPROVER).await.unwrap(),
        ApprovalState::Approved
    );
}
