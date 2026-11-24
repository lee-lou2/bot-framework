//! 종료 반납(PG): 정상 종료 유예가 지난 뒤 호스트가 프로세스 종료와 세션 회수를 확인하고 부르는 `RunStore::release_for_shutdown`.
//! 지금도 유효한 소유자(리스가 끝나지 않았고 세대·claim_epoch가 같은 시도)만 run을 시도 횟수를 쓰지 않고 `ready`로 돌린다.
//! 같은 커밋에서 세대를 한 번 올리고 옛 승인을 무효화하며, 다음 점유는 세대를 또 올리지 않는다(새 claim_epoch만 받는다).
//! 이미 반납했거나 소유권을 잃은 시도·끝난 run·취소 요청이 선 run은 아무것도 바꾸지 못한다.

mod support;

use bot_core::{
    id::*,
    ledger::ApprovalState,
    port::{Error, RunStore},
    profile::ActiveRevisions,
    state::RunContext,
};
use bot_store::{Config, Store};
use std::collections::BTreeMap;
use support::*;

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

/// 시도 한도 1: 반납이 시도를 쓰면 다음 만료에서 바로 dead letter다.
fn one_attempt() -> Config {
    Config {
        max_attempts: 1,
        ..Config::default()
    }
}

/// 접수해 이 핸들로 집는다.
async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> RunContext {
    runs(db)
        .accept(accept("personal", request, "한 번 더"))
        .await
        .unwrap();
    RunStore::claim(handle).await.unwrap().expect("집을 run")
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
    assert_eq!(released.attempts, 0, "시도 횟수를 쓰지 않는다");
    assert_eq!(
        released.generation,
        before.generation + 1,
        "세대를 한 번 올린다"
    );
    assert!(
        released.keep_generation,
        "다음 점유는 이 세대를 그대로 쓴다"
    );
    assert!(!released.leased, "리스를 놓았다");
    assert_eq!(releases(&db).await, 1);

    // 옛 claim은 더 아무것도 못 한다(두 번째 반납도 마찬가지다)
    assert!(matches!(
        old.renew(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        RunStore::release_for_shutdown(&old, run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(releases(&db).await, 1);

    // 다른 워커가 집는다: 세대는 또 오르지 않고 claim_epoch만 오른다. 시도는 여전히 0이다
    let new = db.peer("new-worker").await.for_activation(rev("set-1"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("다른 워커가 이어받는다");
    assert_eq!(again.attempt.run, id);
    assert_eq!(
        again.attempt.generation.0 as i64, released.generation,
        "다음 점유는 세대를 또 올리지 않는다"
    );
    assert_eq!(
        again.attempt.claim.unwrap().0 as i64,
        before.claim_epoch + 1,
        "새 claim_epoch를 받는다"
    );
    let resumed = row(&db, &id).await;
    assert_eq!((resumed.attempts, resumed.keep_generation), (0, false));
    // 옛 시도의 쓰기는 새 소유자를 덮지 못한다
    assert!(matches!(
        RunStore::finish(&old, run.attempt.clone(), done("옛 결과")).await,
        Err(Error::Stale)
    ));
    RunStore::finish(&new, again.attempt.clone(), done("이어서 마쳤어요"))
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
    // 종료 신호가 오기 전에 다른 프로세스가 활성 집합을 교체했다
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
        .expect("활성 집합과 무관하게 지금 소유자의 반납은 받는다");
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

    // 리스가 끝나 다른 워커가 인계했다: 옛 시도는 소유자가 아니다
    let owner_ctx = db.takeover(&id).await;
    let before = row(&db, &id).await;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    // 세대만 맞고 claim이 다른 시도·claim이 없는 시도도 반납하지 못한다
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
        "새 소유자의 행을 건드리지 않았어요"
    );
    assert_eq!(releases(&db).await, 0);

    // 리스가 이미 끝난 시도는 소유자가 아니다(인계 규칙이 맡는다)
    db.expire_lease(&id).await;
    assert!(matches!(
        RunStore::release_for_shutdown(&old, owner_ctx.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(row(&db, &id).await.state, "leased");

    // 끝난 run은 반납할 수 없다
    let next = db.takeover(&id).await;
    RunStore::finish(&old, next.attempt.clone(), done("끝"))
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
    assert_eq!(after.state, "leased", "취소는 일반 경로가 끝낸다");
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
    // 승인자가 허용했다: run은 같은 세대로 깨어나 다시 집힌다(승인은 아직 소비되지 않았다)
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
        "세대가 오른 같은 커밋에서 옛 승인이 닫힌다"
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
    RunStore::claim(&second).await.unwrap().expect("이어받는다");
    assert_eq!(row(&db, &id).await.attempts, 0);
    // 반납은 시도를 쓰지 않았지만 이후의 만료는 평소처럼 쓴다(같은 집합이라 용서도 없다)
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
            .is_empty(),
        "시작한 호출이 없다"
    );
    for id in ["c2", "c1"] {
        begin_call(&db, &run.attempt, id).await;
        calls(&db)
            .start(start_call(&run.attempt, id, None))
            .await
            .unwrap();
    }
    // 열려 있기만 한 호출은 시작한 것이 아니다
    begin_call(&db, &run.attempt, "c3").await;
    let owners = db.store.started_owners(&run.attempt).await.unwrap();
    assert_eq!(
        owners,
        [
            format!("{}.{}.c1", run.attempt.run.0, run.attempt.generation.0),
            format!("{}.{}.c2", run.attempt.run.0, run.attempt.generation.0)
        ],
        "시작한 호출만, 호출 ID 순서로"
    );
    // 다른 세대의 시도는 이 목록에 없다(옛 세대가 시작한 호출은 다음 점유의 복구가 맡는다)
    let mut other = run.attempt.clone();
    other.generation = Generation(run.attempt.generation.0 + 1);
    assert!(db.store.started_owners(&other).await.unwrap().is_empty());
    // 끝난 호출은 빠진다
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
