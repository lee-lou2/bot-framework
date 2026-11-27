//! Lock-ordering regression tests for `expire_approvals` (`run -> approval -> audit_head`), preventing PG deadlocks with concurrent owner clicks.

use crate::support::*;
use bot_core::{
    id::*,
    ledger::{ApprovalState, CallLedger, Decision},
    port::{Error, RunStore},
    state::*,
};
use bot_store::Store;
use sqlx::Executor;
use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;

async fn lock_waiters(db: &TestDb, like: &str) -> i64 {
    db.scalar(&format!(
        "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%{like}%'",
        db.schema
    ))
    .await
}

async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done().await {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn waiting(db: &TestDb, request: &str, call_id: &str, nonce: &str) -> RunContext {
    let run = leased(db, request).await;
    begin_call(db, &run.attempt, call_id).await;
    calls(db)
        .open_approval(open_approval(binding(&run.attempt, call_id, nonce)))
        .await
        .unwrap();
    run
}

async fn click(store: Store, nonce: String) -> Result<ApprovalState, Error> {
    CallLedger::decide(
        &store,
        Decision {
            nonce: ApprovalId(nonce),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        },
    )
    .await
}

async fn audits(db: &TestDb, action: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

async fn state(db: &TestDb, nonce: &str) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_approvals where nonce = '{nonce}'"
    ))
    .await
}

async fn nonce_of(db: &TestDb, run: &RunContext) -> String {
    db.scalar::<String>(&format!(
        "select nonce from bot_approvals where run = '{}'",
        run.attempt.run.0
    ))
    .await
}

async fn hold(db: &TestDb, run: &RunContext) -> sqlx::PgConnection {
    let mut holder = db.raw().await;
    holder.execute("begin").await.unwrap();
    holder
        .execute(&*format!(
            "select 1 from bot_runs where id = '{}' for update",
            run.attempt.run.0
        ))
        .await
        .unwrap();
    holder
}

/// Probes a row lock with `NOWAIT`; returns `false` on `55P03` (locked), `true` if free.
async fn free(db: &TestDb, query: &str) -> bool {
    let mut probe = db.raw().await;
    probe.execute("begin").await.unwrap();
    let got = probe.execute(query).await;
    let _ = probe.execute("rollback").await;
    match got {
        Ok(_) => true,
        Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("55P03") => false,
        Err(other) => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expiry_sweep_and_an_owner_click_on_the_expired_approvals_do_not_deadlock() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let r1 = waiting(&db, "q1", "c1", "n1").await;
    let r2 = waiting(&db, "q2", "c2", "n2").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let mut holder = hold(&db, &r1).await;

    let sweeper = db.peer("w-expire").await;
    let sweep = tokio::spawn(async move { RunStore::claim(&sweeper).await.map(|_| ()) });
    until("sweep is not waiting on run lock", async || {
        lock_waiters(&db, "bot_runs").await >= 1
    })
    .await;

    // Owner clicks n2 (locks r2 -> n2); old sweep locked all expired approvals before r1 and deadlocked here.
    let clicked = tokio::spawn(click(db.peer("w-click").await, "n2".into()));
    until("click neither finished nor blocked", async || {
        clicked.is_finished() || lock_waiters(&db, "bot_approvals").await >= 1
    })
    .await;

    // Release r1; old sweep held audit_head across runs and deadlocked with the click.
    holder.execute("commit").await.unwrap();
    let sweep = tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("sweep timed out")
        .unwrap();
    let clicked = tokio::time::timeout(Duration::from_secs(20), clicked)
        .await
        .expect("click timed out")
        .unwrap();
    assert!(
        sweep.is_ok(),
        "sweep aborted (possible deadlock): {sweep:?}"
    );
    // Clicking an already-expired approval returns Stale, not Unavailable (deadlock).
    assert!(
        matches!(clicked, Err(Error::Stale)),
        "click aborted (possible deadlock): {clicked:?}"
    );
    assert_eq!(state(&db, "n1").await, "expired");
    assert_eq!(state(&db, "n2").await, "expired");
    for run in [&r1, &r2] {
        let state = state_of_run(&db, &run.attempt.run).await;
        assert!(matches!(state.as_str(), "ready" | "leased"), "{state}");
    }
    assert_eq!(audits(&db, "approval.expire").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sweep_waiting_for_a_run_lock_holds_no_approval_row_and_no_audit_head() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ra = waiting(&db, "q1", "c1", "na").await;
    let rb = waiting(&db, "q2", "c2", "nb").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    // Sweep processes runs in (profile, id) C-collation order; lock the second run so the first commits first.
    let order: Vec<String> = sqlx::query_scalar(&format!(
        "select id from bot_runs where id in ('{}', '{}') order by profile collate \"C\", id collate \"C\"",
        ra.attempt.run.0, rb.attempt.run.0
    ))
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    let (first, second) = if order[0] == ra.attempt.run.0 {
        (&ra, &rb)
    } else {
        (&rb, &ra)
    };
    let (first_nonce, second_nonce) = (nonce_of(&db, first).await, nonce_of(&db, second).await);
    let mut holder = hold(&db, second).await;

    let sweeper = db.peer("w-expire").await;
    let sweep = tokio::spawn(async move { RunStore::claim(&sweeper).await.map(|_| ()) });
    until("sweep is not waiting on run lock", async || {
        lock_waiters(&db, "bot_runs").await >= 1
    })
    .await;

    // While blocked on the second run, the sweep must hold neither its approval row nor bot_audit_head.
    assert!(
        free(
            &db,
            &format!(
                "select 1 from bot_approvals where nonce = '{second_nonce}' for update nowait"
            )
        )
        .await,
        "sweep pre-locked the waiting run's approval row"
    );
    assert!(
        free(
            &db,
            "select 1 from bot_audit_head where singleton for update nowait"
        )
        .await,
        "sweep held bot_audit_head while waiting for the next run"
    );
    assert_eq!(state(&db, &first_nonce).await, "expired");
    assert_eq!(state(&db, &second_nonce).await, "pending");
    assert_eq!(audits(&db, "approval.expire").await, 1);

    holder.execute("commit").await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("sweep timed out")
        .unwrap()
        .unwrap();
    assert_eq!(state(&db, &second_nonce).await, "expired");
    assert_eq!(audits(&db, "approval.expire").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_that_someone_else_closed_while_the_sweep_waited_for_the_run_is_left_alone() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = waiting(&db, "q1", "c1", "n1").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    // Close the approval concurrently after the sweep collected candidates and before it acquires the run lock.
    let mut holder = hold(&db, &run).await;
    let sweeper = db.peer("w-expire").await;
    let sweep = tokio::spawn(async move { RunStore::claim(&sweeper).await.map(|_| ()) });
    until("sweep is not waiting on run lock", async || {
        lock_waiters(&db, "bot_runs").await >= 1
    })
    .await;
    holder
        .execute("update bot_approvals set state = 'superseded' where nonce = 'n1'")
        .await
        .unwrap();
    holder.execute("commit").await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("sweep timed out")
        .unwrap()
        .unwrap();
    // Sweep rechecks under the lock and must leave already-closed approvals untouched.
    assert_eq!(state(&db, "n1").await, "superseded");
    assert_eq!(audits(&db, "approval.expire").await, 0);
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sweep_closes_only_what_expired_and_wakes_only_runs_with_nothing_left_to_wait_for() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let gone = waiting(&db, "q1", "c1", "n-gone").await;
    let alive = waiting(&db, "q2", "c2", "n-alive").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1 where nonce = 'n-gone'")
        .await;
    let picked = RunStore::claim(&db.store)
        .await
        .unwrap()
        .expect("woken run");
    assert_eq!(picked.attempt.run, gone.attempt.run);
    assert_eq!(state(&db, "n-gone").await, "expired");
    assert_eq!(state(&db, "n-alive").await, "pending");
    assert_eq!(
        state_of_run(&db, &alive.attempt.run).await,
        "waiting",
        "run with non-expired approval must stay waiting"
    );
    assert_eq!(audits(&db, "approval.expire").await, 1);
    assert!(RunStore::claim(&db.store).await.unwrap().is_none());
    assert_eq!(audits(&db, "approval.expire").await, 1);
    // Approved but unconsumed approvals also expire.
    db.sql("update bot_approvals set state = 'approved', expires_ms = bot_now_ms() - 1 where nonce = 'n-alive'")
        .await;
    let _ = RunStore::claim(&db.store).await.unwrap();
    assert_eq!(state(&db, "n-alive").await, "expired");
    assert_eq!(audits(&db, "approval.expire").await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn overlapping_sweeps_and_owner_clicks_never_end_in_a_deadlock() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Repeatedly overlap 3 sweeps and 4 owner clicks; any PG deadlock surfaces as Error::Unavailable.
    let mut unavailable = Vec::new();
    for round in 0..6 {
        let nonces: Vec<String> = (0..4).map(|n| format!("n{round}-{n}")).collect();
        for (n, nonce) in nonces.iter().enumerate() {
            waiting(
                &db,
                &format!("q{round}-{n}"),
                &format!("c{round}-{n}"),
                nonce,
            )
            .await;
        }
        db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
            .await;
        let barrier = Arc::new(Barrier::new(3 + nonces.len()));
        let mut tasks = Vec::new();
        for s in 0..3 {
            let store = db.peer(&format!("w-sweep-{round}-{s}")).await;
            let barrier = barrier.clone();
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                RunStore::claim(&store).await.map(|_| ())
            }));
        }
        for nonce in &nonces {
            let store = db.peer(&format!("w-click-{nonce}")).await;
            let (barrier, nonce) = (barrier.clone(), nonce.clone());
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                click(store, nonce).await.map(|_| ())
            }));
        }
        for task in tasks {
            let result = tokio::time::timeout(Duration::from_secs(30), task)
                .await
                .expect("call timed out")
                .unwrap();
            if matches!(result, Err(Error::Unavailable)) {
                unavailable.push(round);
            }
        }
        let _ = RunStore::claim(&db.store).await;
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select count(*) from bot_approvals where nonce like 'n{round}-%' and state in ('pending', 'approved')"
            ))
            .await,
            0,
            "round {round}"
        );
        db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, wait_kind = null where state in ('ready', 'leased')")
            .await;
    }
    assert!(
        unavailable.is_empty(),
        "calls failed with Unavailable (possible deadlock) in rounds {unavailable:?}"
    );
}
