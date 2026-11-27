//! 승인 만료 청소(`expire_approvals`, 워커 claim마다 돈다)의 잠금 순서(PG). 소유자의 결정·취소·시작과 같은 `run → 승인 → … → 감사 head`여야 한다:
//! 승인 행을 한꺼번에 먼저 잠그고 run을 기다리거나, 감사 head를 쥔 채 다음 run을 잠그면 같은 run을 반대 순서로 잠그는 쪽(소유자의 클릭)과 교착한다(검토자 A가 3/3회 재현).
//! - 재현: 만료한 승인 둘 + claim(청소) + 같은 승인에 대한 소유자 클릭이 겹쳐도 PG 교착(`Unavailable`)이 없다.
//! - 규율: 청소가 run 잠금을 기다리는 동안에는 다른 run의 승인 행도, 감사 head도 쥐고 있지 않다. 잠근 뒤에 다시 판정하므로 그 사이 바뀐 승인은 건드리지 않는다.
//! - 반복: 청소·클릭을 여러 번 겹쳐도 교착으로 끊긴 호출이 0이다.

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

/// 이 시험 스키마의 연결 가운데 `like`가 든 문장으로 잠금을 기다리는 수.
async fn lock_waiters(db: &TestDb, like: &str) -> i64 {
    db.scalar(&format!(
        "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%{like}%'",
        db.schema
    ))
    .await
}

/// 조건이 될 때까지(최대 10초) 기다린다.
async fn until(what: &str, mut done: impl AsyncFnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done().await {
        assert!(started.elapsed() < Duration::from_secs(10), "{what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// 승인 대기 run 하나(승인은 한 시간 뒤 만료로 열린다).
async fn waiting(db: &TestDb, request: &str, call_id: &str, nonce: &str) -> RunContext {
    let run = leased(db, request).await;
    begin_call(db, &run.attempt, call_id).await;
    calls(db)
        .open_approval(open_approval(binding(&run.attempt, call_id, nonce)))
        .await
        .unwrap();
    run
}

/// 소유자가 이 승인을 «허용»으로 누른다.
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

/// 이 run을 다른 트랜잭션이 쥐고 있다(정상 처리 중인 아무 트랜잭션).
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

/// `nowait`로 한 줄을 잠가 본다: 잠겨 있으면(`55P03`) false, 잠글 수 있었으면 true(곧바로 놓는다).
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
    // 승인 대기 run 둘(r1·r2), 각자 승인 n1·n2가 방금 만료됐다
    let r1 = waiting(&db, "q1", "c1", "n1").await;
    let r2 = waiting(&db, "q2", "c2", "n2").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let mut holder = hold(&db, &r1).await;

    // 워커 claim → 만료 청소: r1을 기다린다
    let sweeper = db.peer("w-expire").await;
    let sweep = tokio::spawn(async move { RunStore::claim(&sweeper).await.map(|_| ()) });
    until(
        "청소가 run 잠금을 기다리지 않는다",
        async || lock_waiters(&db, "bot_runs").await >= 1,
    )
    .await;

    // 소유자가 n2를 누른다: r2 → n2 순서로 잠근다. (옛 청소는 n1·n2를 먼저 잠가 둔 채 r1을 기다려서 여기서 n2를 기다렸다)
    let clicked = tokio::spawn(click(db.peer("w-click").await, "n2".into()));
    until(
        "클릭이 끝나지도 기다리지도 않는다",
        async || clicked.is_finished() || lock_waiters(&db, "bot_approvals").await >= 1,
    )
    .await;

    // r1을 놓는다: 옛 청소는 r1 → 감사 head → r2로 가서 클릭과 서로 기다렸다
    holder.execute("commit").await.unwrap();
    let sweep = tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("청소가 끝나지 않는다")
        .unwrap();
    let clicked = tokio::time::timeout(Duration::from_secs(20), clicked)
        .await
        .expect("클릭이 끝나지 않는다")
        .unwrap();
    assert!(sweep.is_ok(), "교착으로 청소가 끊겼다: {sweep:?}");
    // 이미 만료한 승인을 누른 것이므로 결과는 «낡음»이다(교착으로 끊긴 `Unavailable`이 아니다)
    assert!(
        matches!(clicked, Err(Error::Stale)),
        "교착으로 클릭이 끊겼다: {clicked:?}"
    );
    // 두 승인 모두 만료로 닫혔고 승인 대기 run은 깨어났다(둘 중 하나는 이 claim이 집었을 수 있다)
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
    // 청소는 (프로필, run) 순서로 한 run씩 처리한다: 나중 run을 쥐어 두면 먼저 run은 끝내고 나중 run에서 기다린다
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
    until(
        "청소가 run 잠금을 기다리지 않는다",
        async || lock_waiters(&db, "bot_runs").await >= 1,
    )
    .await;

    // 기다리는 동안 청소는 아무것도 쥐고 있지 않다: 기다리는 run의 승인 행도, 모두가 마지막에 잠그는 감사 head도
    // (옛 청소는 만료한 승인을 모두 먼저 잠그고 head까지 쥔 채 기다렸다)
    assert!(
        free(
            &db,
            &format!(
                "select 1 from bot_approvals where nonce = '{second_nonce}' for update nowait"
            )
        )
        .await,
        "청소가 기다리는 run의 승인 행을 미리 잠가 두었다"
    );
    assert!(
        free(
            &db,
            "select 1 from bot_audit_head where singleton for update nowait"
        )
        .await,
        "청소가 감사 head를 쥔 채 다음 run을 기다린다"
    );
    // 먼저 run은 이미 한 트랜잭션으로 끝나 커밋됐다(승인 만료·깨움·감사)
    assert_eq!(state(&db, &first_nonce).await, "expired");
    assert_eq!(state(&db, &second_nonce).await, "pending");
    assert_eq!(audits(&db, "approval.expire").await, 1);

    holder.execute("commit").await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("청소가 끝나지 않는다")
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
    // 다른 트랜잭션이 run을 쥔 채 승인을 대체로 닫는다(청소가 후보를 읽은 뒤, run을 잠그기 전에)
    let mut holder = hold(&db, &run).await;
    let sweeper = db.peer("w-expire").await;
    let sweep = tokio::spawn(async move { RunStore::claim(&sweeper).await.map(|_| ()) });
    until(
        "청소가 run 잠금을 기다리지 않는다",
        async || lock_waiters(&db, "bot_runs").await >= 1,
    )
    .await;
    holder
        .execute("update bot_approvals set state = 'superseded' where nonce = 'n1'")
        .await
        .unwrap();
    holder.execute("commit").await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), sweep)
        .await
        .expect("청소가 끝나지 않는다")
        .unwrap()
        .unwrap();
    // 잠근 뒤에 다시 판정했다: 이미 닫힌 승인은 만료로 덮지 않고, 감사도 깨움도 없다
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
    // 만료하지 않은 승인과 그 run은 그대로다. 만료한 쪽만 닫히고 run이 깨어나 claim이 집는다
    let picked = RunStore::claim(&db.store)
        .await
        .unwrap()
        .expect("깨어난 run");
    assert_eq!(picked.attempt.run, gone.attempt.run);
    assert_eq!(state(&db, "n-gone").await, "expired");
    assert_eq!(state(&db, "n-alive").await, "pending");
    assert_eq!(
        state_of_run(&db, &alive.attempt.run).await,
        "waiting",
        "만료하지 않은 승인을 기다리는 run은 그대로다"
    );
    assert_eq!(audits(&db, "approval.expire").await, 1);
    // 같은 청소를 또 해도(다음 claim) 아무것도 늘지 않는다
    assert!(RunStore::claim(&db.store).await.unwrap().is_none());
    assert_eq!(audits(&db, "approval.expire").await, 1);
    // 결정을 받아 소비 전인 승인도 만료하면 닫힌다
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
    // 승인 대기 run 여럿이 한꺼번에 만료하는 라운드를 되풀이하며 청소(claim) 셋과 클릭 여럿을 겹친다. 교착이면 PG가 한쪽을 끊어 `Unavailable`이 된다
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
                .expect("호출이 끝나지 않는다")
                .unwrap();
            if matches!(result, Err(Error::Unavailable)) {
                unavailable.push(round);
            }
        }
        // 어느 쪽이 이겼든 이 라운드의 승인은 모두 닫혔다(청소가 가장 늦은 호출이라도 한 번 더 돈다)
        let _ = RunStore::claim(&db.store).await;
        assert_eq!(
            db.scalar::<i64>(&format!(
                "select count(*) from bot_approvals where nonce like 'n{round}-%' and state in ('pending', 'approved')"
            ))
            .await,
            0,
            "round {round}"
        );
        // 다음 라운드가 새 run을 집도록 이 라운드가 남긴 run을 정리한다
        db.sql("update bot_runs set state = 'succeeded', lease_until_ms = null, lease_owner = null, wait_kind = null where state in ('ready', 'leased')")
            .await;
    }
    assert!(
        unavailable.is_empty(),
        "교착(또는 다른 이유)으로 끊긴 호출이 있다: 라운드 {unavailable:?}"
    );
}
