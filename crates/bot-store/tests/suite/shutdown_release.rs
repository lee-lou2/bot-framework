//! Graceful shutdown release (`RunStore::release_for_shutdown`, PG): returns a live leased run
//! to `ready` without spending an attempt, bumps generation once (kept by the next claim),
//! supersedes stale approvals, and ignores stale or cancel-requested runs.

use crate::support::*;
use bot_core::{
    id::*,
    ledger::ApprovalState,
    port::{Error, RunStore},
    profile::ActiveRevisions,
    state::RunContext,
};
use bot_store::{Config, Store};
use std::collections::BTreeMap;

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

fn one_attempt() -> Config {
    Config {
        max_attempts: 1,
        ..Config::default()
    }
}

async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> RunContext {
    runs(db)
        .accept(accept("personal", request, "one more time"))
        .await
        .unwrap();
    RunStore::claim(handle)
        .await
        .unwrap()
        .expect("run to claim")
}

struct Row {
    state: String,
    attempts: i32,
    generation: i64,
    claim_epoch: i64,
    keep_generation: bool,
    leased: bool,
}

async fn row(db: &TestDb, run: &RunId) -> Row {
    use sqlx::Row as _;
    let r = sqlx::query(&format!(
        "select state, attempts, generation, claim_epoch, keep_generation,
                (lease_owner is not null or lease_until_ms is not null) as leased
         from bot_runs where id = '{}'",
        run.0
    ))
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    Row {
        state: r.get("state"),
        attempts: r.get("attempts"),
        generation: r.get("generation"),
        claim_epoch: r.get("claim_epoch"),
        keep_generation: r.get("keep_generation"),
        leased: r.get("leased"),
    }
}

async fn releases(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_audit where action = 'run.release'")
        .await
}

#[tokio::test]
async fn the_current_owner_returns_the_run_to_ready_and_the_next_claim_keeps_the_released_generation()
 {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    let before = row(&db, &id).await;
    assert_eq!(before.state, "leased");

    RunStore::release_for_shutdown(&old, run.attempt.clone())
        .await
        .unwrap();
    let released = row(&db, &id).await;
    assert_eq!(released.state, "ready");
    assert_eq!(released.attempts, 0, "release must not spend an attempt");
    assert_eq!(
        released.generation,
        before.generation + 1,
        "release bumps generation once"
    );
    assert!(
        released.keep_generation,
        "next claim must preserve the released generation"
    );
    assert!(!released.leased, "lease must be cleared");
    assert_eq!(releases(&db).await, 1);

    assert!(matches!(
        old.renew(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        RunStore::release_for_shutdown(&old, run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(releases(&db).await, 1);

    let new = db.peer("new-worker").await.for_activation(rev("set-1"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("next worker claims released run");
    assert_eq!(again.attempt.run, id);
    assert_eq!(
        again.attempt.generation.0 as i64, released.generation,
        "next claim must not bump generation again"
    );
    assert_eq!(
        again.attempt.claim.unwrap().0 as i64,
        before.claim_epoch + 1,
        "next claim receives a new claim_epoch"
    );
    let resumed = row(&db, &id).await;
    assert_eq!((resumed.attempts, resumed.keep_generation), (0, false));
    assert!(matches!(
        RunStore::finish(&old, run.attempt.clone(), done("stale result")).await,
        Err(Error::Stale)
    ));
    RunStore::finish(&new, again.attempt.clone(), done("finished after handoff"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
}

#[tokio::test]
async fn a_handle_whose_active_set_was_replaced_can_still_release_its_own_claim() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            BTreeMap::from([(profile_id(), rev("r1"))]),
            vec![],
            BTreeMap::new(),
        )
        .await
        .unwrap();
    RunStore::release_for_shutdown(&old, run.attempt.clone())
        .await
        .expect("live owner must be able to release regardless of active set");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
}

#[tokio::test]
async fn only_the_live_owner_may_release_and_nobody_else_is_touched() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();

    let owner_ctx = db.takeover(&id).await;
    let before = row(&db, &id).await;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    let mut wrong_claim = owner_ctx.attempt.clone();
    wrong_claim.claim = Some(ClaimEpoch(owner_ctx.attempt.claim.unwrap().0 + 7));
    assert!(matches!(
        RunStore::release_for_shutdown(&old, wrong_claim).await,
        Err(Error::Stale)
    ));
    let mut no_claim = owner_ctx.attempt.clone();
    no_claim.claim = None;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, no_claim).await,
        Err(Error::Stale)
    ));
    let after = row(&db, &id).await;
    assert_eq!(
        (
            &after.state,
            after.generation,
            after.claim_epoch,
            after.attempts
        ),
        (
            &before.state,
            before.generation,
            before.claim_epoch,
            before.attempts
        ),
        "new owner's row must stay untouched"
    );
    assert_eq!(releases(&db).await, 0);

    db.expire_lease(&id).await;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, owner_ctx.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(row(&db, &id).await.state, "leased");

    let next = db.takeover(&id).await;
    RunStore::finish(&old, next.attempt.clone(), done("done"))
        .await
        .unwrap();
    assert!(matches!(
        RunStore::release_for_shutdown(&old, next.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
    assert_eq!(releases(&db).await, 0);
}

#[tokio::test]
async fn a_run_with_a_cancel_request_is_not_requeued() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        run.attempt.run.0
    ))
    .await;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, run.attempt.clone()).await,
        Err(Error::Denied)
    ));
    let after = row(&db, &run.attempt.run).await;
    assert_eq!(
        after.state, "leased",
        "normal cancel path handles finishing"
    );
    assert_eq!(releases(&db).await, 0);
}

#[tokio::test]
async fn the_release_closes_an_approval_of_the_old_generation_in_the_same_commit() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db)
        .decide(bot_core::ledger::Decision {
            nonce: ApprovalId("n1".into()),
            actor: PrincipalId("local:owner".into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &resumed.attempt);
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Approved
    );

    runs(&db)
        .release_for_shutdown(resumed.attempt.clone())
        .await
        .unwrap();
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Superseded,
        "old-generation approval must be superseded in the same commit"
    );
    assert_eq!(state_of_run(&db, &resumed.attempt.run).await, "ready");
}

#[tokio::test]
async fn a_released_run_still_spends_an_attempt_when_a_later_lease_expires_under_the_same_set() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let first = db.peer("first").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &first, "r1").await;
    let id = run.attempt.run.clone();
    RunStore::release_for_shutdown(&first, run.attempt.clone())
        .await
        .unwrap();
    let second = db.peer("second").await.for_activation(rev("set-1"));
    RunStore::claim(&second)
        .await
        .unwrap()
        .expect("reclaimed run");
    assert_eq!(row(&db, &id).await.attempts, 0);
    db.expire_lease(&id).await;
    let third = db.peer("third").await.for_activation(rev("set-1"));
    assert!(RunStore::claim(&third).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &id).await, "dead");
}

#[tokio::test]
async fn the_started_calls_of_an_attempt_are_listed_by_owner_for_the_cleanup() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let run = leased(&db, "r").await;
    assert!(
        db.store
            .started_owners(&run.attempt)
            .await
            .unwrap()
            .is_empty()
    );
    for id in ["c2", "c1"] {
        begin_call(&db, &run.attempt, id).await;
        calls(&db)
            .start(start_call(&run.attempt, id, None))
            .await
            .unwrap();
    }
    begin_call(&db, &run.attempt, "c3").await;
    let owners = db.store.started_owners(&run.attempt).await.unwrap();
    assert_eq!(
        owners,
        [
            format!("{}.{}.c1", run.attempt.run.0, run.attempt.generation.0),
            format!("{}.{}.c2", run.attempt.run.0, run.attempt.generation.0)
        ],
        "only started calls, ordered by call ID"
    );
    let mut other = run.attempt.clone();
    other.generation = Generation(run.attempt.generation.0 + 1);
    assert!(db.store.started_owners(&other).await.unwrap().is_empty());
    calls(&db)
        .finish(bot_core::ledger::FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: bot_core::ledger::Outcome::Done(bot_core::command::JsonPayload("{}".into())),
        })
        .await
        .unwrap();
    assert_eq!(
        db.store.started_owners(&run.attempt).await.unwrap().len(),
        1
    );
}
