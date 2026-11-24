//! 구판이 된 프로세스의 반납(PG): 활성 집합이 바뀌어 생긴 반납은 시도 횟수를 쓰지 않고 대기열로 돌아가 새 판본의 워커가 이어 간다.
//! 판단은 저장소가 한다 — 이 핸들의 집합이 더는 현재가 아니고 이 시도가 지금 소유자일 때만. 아니면 거절하고 호출자는 일반 실패로 간다.

mod support;

use bot_core::{
    command::{Failure, Retry},
    id::*,
    port::{Error, RunStore},
    profile::ActiveRevisions,
};
use bot_store::{Config, Store};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};
use support::*;

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

/// 프로필 판본은 그대로 두고 집합 판본만 올린다(호스트 설정이 바뀐 활성화).
async fn remap(db: &TestDb, from: &str, to: &str) {
    let profiles = BTreeMap::from([
        (profile_id(), rev("r1")),
        (ProfileId("work".into()), rev("r1")),
    ]);
    db.store
        .replace(Some(rev(from)), rev(to), profiles, vec![], BTreeMap::new())
        .await
        .unwrap();
}

/// 마지막 시도에서 실패하면 바로 dead letter가 되는 설정
fn one_attempt() -> Config {
    Config {
        max_attempts: 1,
        ..Config::default()
    }
}

async fn attempts(db: &TestDb, run: &RunId) -> i32 {
    db.scalar::<i32>(&format!(
        "select attempts from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

/// 이 핸들로 접수한 run을 집는다.
async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> bot_core::state::RunContext {
    runs(db)
        .accept(accept("personal", request, "한 번 더"))
        .await
        .unwrap();
    RunStore::claim(handle).await.unwrap().expect("집을 run")
}

#[tokio::test]
async fn a_stale_worker_hands_the_last_attempt_back_without_spending_it_and_a_new_worker_finishes_it()
 {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;

    // 대조: 같은 상황에서 일반 실패는 마지막 시도라 바로 dead letter다 — 구판 반납이 이렇게 죽으면 안 된다
    let general = db.peer("general").await;
    let control = claimed_by(&db, &general, "control").await;
    assert!(matches!(
        RunStore::fail(&general, control.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));

    // 구판 워커의 반납: 대기열로 돌아가고 시도 횟수는 그대로다
    old.release_stale(&run.attempt).await.unwrap();
    let id = &run.attempt.run;
    assert_eq!(state_of_run(&db, id).await, "ready");
    assert_eq!(
        attempts(&db, id).await,
        0,
        "활성 집합 교체로 생긴 반납은 시도를 쓰지 않아요"
    );
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_runs where id = '{}' and lease_owner is null and lease_until_ms is null",
            id.0
        ))
        .await,
        1,
        "리스를 놓았다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        1
    );
    // 옛 claim은 더 아무것도 못 한다
    assert!(matches!(
        old.renew(run.attempt.clone()).await,
        Err(Error::Stale)
    ));

    // 새 판본의 워커가 집어 끝낸다(세대가 오르고 시도 횟수는 여전히 0)
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("새 워커가 집는다");
    assert_eq!(&again.attempt.run, id);
    assert_eq!(again.attempt.generation.0, run.attempt.generation.0 + 1);
    assert_eq!(attempts(&db, id).await, 0);
    RunStore::finish(&new, again.attempt.clone(), done("이어서 마쳤어요"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, id).await, "succeeded");
}

#[tokio::test]
async fn a_worker_of_the_current_set_is_refused_and_takes_the_normal_failure_path() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let current = db.peer("current").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &current, "r1").await;
    let id = &run.attempt.run;
    // 집합이 같다: 거절하고 아무것도 바꾸지 않는다
    assert!(matches!(
        current.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(state_of_run(&db, id).await, "leased");
    assert_eq!(attempts(&db, id).await, 0);
    // 고정하지 않은 핸들도 «구판»일 수 없다
    assert!(matches!(
        db.store.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    // 호출자는 일반 실패로 간다: 마지막 시도라 dead letter다
    assert!(matches!(
        RunStore::fail(&current, run.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, id).await, "dead");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        0
    );
}

#[tokio::test]
async fn an_old_claim_or_generation_cannot_hand_a_run_back() {
    // 인계(리스 만료)는 시도를 쓰므로 한도가 1이면 인계 대신 dead letter다: 소유 판정만 보려고 기본 한도를 쓴다
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    // 리스가 끝나 다른 소유자가 인계했다(세대·claim이 올랐다): 옛 시도는 소유자가 아니다
    let owner = db.takeover(&id).await;
    assert!(owner.attempt.generation.0 > run.attempt.generation.0);
    assert!(matches!(
        old.release_stale(&run.attempt).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        state_of_run(&db, &id).await,
        "leased",
        "새 소유자의 run을 건드리지 않았어요"
    );
    // 세대만 맞고 claim이 다른 시도·claim이 없는 시도도 반납하지 못한다
    let mut wrong_claim = owner.attempt.clone();
    wrong_claim.claim = Some(ClaimEpoch(owner.attempt.claim.unwrap().0 + 7));
    assert!(matches!(
        old.release_stale(&wrong_claim).await,
        Err(Error::Stale)
    ));
    let mut no_claim = owner.attempt.clone();
    no_claim.claim = None;
    assert!(matches!(
        old.release_stale(&no_claim).await,
        Err(Error::Stale)
    ));
    // 리스가 이미 끝난 구판의 시도도 소유자가 아니다
    db.expire_lease(&id).await;
    assert!(matches!(
        old.release_stale(&owner.attempt).await,
        Err(Error::Stale)
    ));
    // 한 번 돌려준 시도는 다시 돌려줄 수 없다(대기열에 있다)
    let fresh = db.peer("fresh-old").await.for_activation(rev("set-1"));
    let taken = RunStore::claim(&fresh).await;
    assert!(
        matches!(taken, Err(Error::Stale)),
        "구판 핸들은 집지도 못한다: {taken:?}"
    );
    let next = db.peer("next").await.for_activation(rev("set-2"));
    let reclaimed = RunStore::claim(&next).await.unwrap().expect("인계");
    let stale_again = db.peer("stale-again").await.for_activation(rev("set-1"));
    stale_again.release_stale(&reclaimed.attempt).await.unwrap();
    assert!(matches!(
        stale_again.release_stale(&reclaimed.attempt).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn a_run_with_a_cancel_request_is_not_requeued_and_ends_as_cancelled_by_the_normal_path() {
    let Some(db) = TestDb::with(Config::default()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        run.attempt.run.0
    ))
    .await;
    assert!(matches!(
        old.release_stale(&run.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
    // 일반 실패 경로는 취소로 끝낸다
    assert!(matches!(
        RunStore::fail(&old, run.attempt.clone(), Failure::Transient("x")).await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "cancelled");
}

#[tokio::test]
async fn the_decision_waits_for_a_replacement_in_flight_and_sees_the_committed_set() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    // 교체가 진행 중이다: 같은 행을 쓰고 아직 커밋하지 않았다
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = 'set-2' where singleton returning pg_backend_pid()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let attempt = run.attempt.clone();
    let task = tokio::spawn(async move { old.release_stale(&attempt).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            )
            .bind(writer)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            if blocked {
                break;
            }
            assert!(!task.is_finished(), "반납이 교체를 기다리지 않고 끝났다");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("반납이 교체 뒤에서 기다려야 한다");
    tx.commit().await.unwrap();
    // 커밋된 집합(set-2)으로 판정한다: 구판이므로 대기열로 돌아간다
    task.await.unwrap().unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    assert_eq!(attempts(&db, &run.attempt.run).await, 0);
}

// ---- 실패 보고와 반납을 한 DB 판정으로(`fail_or_release`) ----

#[tokio::test]
async fn one_decision_hands_a_stale_owner_back_and_reports_everyone_else_as_an_ordinary_failure() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    // 구판이고 소유자: 영구든 일시든 실패가 어느 쪽이든 시도를 쓰지 않고 대기열로 돌아간다(마지막 시도에서도)
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let first = claimed_by(&db, &old, "r1").await;
    remap(&db, "set-1", "set-2").await;
    let retry = old
        .fail_or_release(&first.attempt, Failure::Permanent("model_denied"))
        .await
        .unwrap();
    assert_eq!(retry, Retry::After(Duration::ZERO));
    assert_eq!(state_of_run(&db, &first.attempt.run).await, "ready");
    assert_eq!(attempts(&db, &first.attempt.run).await, 0);
    // 소유자가 아니면 아무것도 바꾸지 않고 Stale이다
    assert!(matches!(
        old.fail_or_release(&first.attempt, Failure::Transient("x"))
            .await,
        Err(Error::Stale)
    ));
    // 현재 판본의 핸들·고정하지 않은 핸들은 `fail`과 같은 일반 실패다
    let current = db.peer("current").await.for_activation(rev("set-2"));
    let second = RunStore::claim(&current)
        .await
        .unwrap()
        .expect("새 워커가 집는다");
    assert_eq!(second.attempt.run, first.attempt.run);
    assert!(matches!(
        current
            .fail_or_release(&second.attempt, Failure::Transient("x"))
            .await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &second.attempt.run).await, "dead");
    let plain = claimed_by(&db, &db.store, "r2").await;
    assert!(matches!(
        db.store
            .fail_or_release(&plain.attempt, Failure::Permanent("x"))
            .await,
        Ok(Retry::Dead)
    ));
    // 구판이어도 취소 요청이 선 run은 일반 경로(취소)다(set-2에서 집은 뒤 set-3으로 교체돼 구판이 된다)
    let old2 = db.peer("old-2").await.for_activation(rev("set-2"));
    let third = claimed_by(&db, &old2, "r3").await;
    remap(&db, "set-2", "set-3").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        third.attempt.run.0
    ))
    .await;
    assert!(matches!(
        old2.fail_or_release(&third.attempt, Failure::Transient("x"))
            .await,
        Ok(Retry::Dead)
    ));
    assert_eq!(state_of_run(&db, &third.attempt.run).await, "cancelled");
}

/// 활성 집합 행 잠금을 기다리다 `lock_timeout`(ms)을 넘기게 하는 핸들(저장소 불통을 흉내 낸다)
async fn impatient(db: &TestDb, millis: u32) -> Store {
    use sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;
    let url = std::env::var("BOT_TEST_DATABASE_URL").expect("시험 DB");
    let options = PgConnectOptions::from_str(&url)
        .unwrap()
        .options([
            ("search_path", db.schema.as_str()),
            ("lock_timeout", &millis.to_string()),
        ])
        .application_name(&db.schema);
    let pool = Store::pool_options()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    Store::from_pool(pool, one_attempt())
}

#[tokio::test]
async fn a_lock_timeout_in_the_decision_records_no_failure_and_the_repeat_hands_the_run_back() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = impatient(&db, 300).await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    // 교체가 오래 걸린다: 같은 행을 쓰고 아직 커밋하지 않았다 → 판정(`bot_active` FOR SHARE)이 `lock_timeout`을 넘긴다
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    sqlx::query("update bot_active set revision = 'set-2' where singleton")
        .execute(&mut *tx)
        .await
        .unwrap();
    // 마지막 시도라 일반 실패였다면 지금 dead letter다. 판정을 못 했으니 아무것도 기록하지 않고 오류만 돌려준다
    let timed_out = old
        .fail_or_release(&run.attempt, Failure::Permanent("model_denied"))
        .await;
    assert!(
        matches!(timed_out, Err(Error::Unavailable)),
        "{timed_out:?}"
    );
    assert_eq!(
        state_of_run(&db, &id).await,
        "leased",
        "실패를 기록하면 안 돼요"
    );
    assert_eq!(attempts(&db, &id).await, 0);
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action in ('run.retry', 'run.dead', 'run.fail')"
        )
        .await,
        0
    );
    // 교체가 끝난 뒤 같은 호출을 다시 하면 판정이 되고, 구판의 소유자라 대기열로 돌아간다(시도 0)
    tx.commit().await.unwrap();
    let again = old
        .fail_or_release(&run.attempt, Failure::Permanent("model_denied"))
        .await;
    assert!(
        matches!(again, Ok(Retry::After(d)) if d.is_zero()),
        "{again:?}"
    );
    assert_eq!(state_of_run(&db, &id).await, "ready");
    assert_eq!(attempts(&db, &id).await, 0);
    // 새 판본의 워커가 이어받아 끝낸다
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let resumed = RunStore::claim(&new).await.unwrap().expect("이어받는다");
    RunStore::finish(&new, resumed.attempt.clone(), done("마쳤어요"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
}
