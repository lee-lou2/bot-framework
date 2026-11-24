//! 리스 만료 인계와 호스트 활성 집합(PG): 활성 집합이 교체되는 동안 옛 워커가 반납하지 못해 리스가 끝난 run은 run의 잘못이 아니니 그 인계는 시도를
//! 쓰지 않는다. 직전 claim의 집합과 지금 집는 핸들의 집합이 둘 다 있고 다를 때만, 이번 한 번만이다. 같은 집합·구판 행(기록 없음)·고정하지 않은
//! 핸들은 지금처럼 한 번 쓴다. 집합을 확인하지 못하면 아무것도 쓰지 않고 실패한다. 취소·세대 교체·옛 승인 무효화는 두 경우에 똑같다.

mod support;

use bot_core::{
    id::*,
    port::{Error, RunStore},
    profile::ActiveRevisions,
    state::RunContext,
};
use bot_store::{Config, Store};
use sqlx::{Connection, postgres::PgPoolOptions};
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

/// 시도 한도 1: 인계가 시도를 쓰면 바로 dead letter다.
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

async fn recorded_set(db: &TestDb, run: &RunId) -> Option<String> {
    db.scalar::<Option<String>>(&format!(
        "select claim_set from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn reclaims(db: &TestDb, word: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = 'run.reclaim' and verdict = '{word}'"
    ))
    .await
}

/// 접수해 이 핸들로 집는다.
async fn claimed_by(db: &TestDb, handle: &Store, request: &str) -> RunContext {
    runs(db)
        .accept(accept("personal", request, "한 번 더"))
        .await
        .unwrap();
    RunStore::claim(handle).await.unwrap().expect("집을 run")
}

/// 교체가 진행 중이다: `bot_active` 행을 쓰고 아직 커밋하지 않았다. 그 연결의 백엔드 번호를 돌려준다.
async fn replacement_in_flight<'a>(
    connection: &'a mut sqlx::PgConnection,
    to: &str,
) -> (sqlx::Transaction<'a, sqlx::Postgres>, i32) {
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = $1 where singleton returning pg_backend_pid()",
    )
    .bind(to)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    (tx, writer)
}

#[tokio::test]
async fn a_lease_that_ended_while_the_active_set_was_replaced_does_not_spend_the_attempt() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    assert_eq!(
        recorded_set(&db, &id).await.as_deref(),
        Some("set-1"),
        "집을 때 이 핸들이 고정한 집합을 같은 UPDATE에 기록한다"
    );

    // 교체가 행 잠금을 쥔 동안 옛 워커의 반납은 막혀 시간 초과로 포기된다(워커의 반납 시간 상한을 흉내 낸다)
    let mut connection = db.raw().await;
    let (tx, _writer) = replacement_in_flight(&mut connection, "set-2").await;
    let attempt = run.attempt.clone();
    let handed_back =
        tokio::time::timeout(Duration::from_millis(500), old.release_stale(&attempt)).await;
    assert!(
        handed_back.is_err(),
        "반납이 교체 뒤에서 막혀 시간이 초과된다"
    );
    assert_eq!(state_of_run(&db, &id).await, "leased");
    tx.commit().await.unwrap();
    // 반납에 실패한 옛 워커는 끝내 응답하지 않아 리스가 끝난다
    db.expire_lease(&id).await;

    // 새 집합의 워커가 인계한다: 시도 0으로 집어 끝낸다(한도 1에서도 dead letter가 아니다)
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("새 워커가 인계한다");
    assert_eq!(again.attempt.run, id);
    assert_eq!(
        attempts(&db, &id).await,
        0,
        "집합 교체로 생긴 만료는 시도를 쓰지 않아요"
    );
    assert_eq!(
        recorded_set(&db, &id).await.as_deref(),
        Some("set-2"),
        "다음 claim의 집합으로 갱신한다"
    );
    assert_eq!(
        again.attempt.generation.0,
        run.attempt.generation.0 + 1,
        "인계는 세대를 올린다"
    );
    assert!(again.attempt.claim.unwrap().0 > run.attempt.claim.unwrap().0);
    assert_eq!(reclaims(&db, "set_replaced").await, 1);
    assert_eq!(reclaims(&db, "lease_expired").await, 0);
    assert!(
        matches!(old.renew(run.attempt.clone()).await, Err(Error::Stale)),
        "옛 claim은 더 아무것도 못 한다"
    );
    RunStore::finish(&new, again.attempt.clone(), done("이어서 마쳤어요"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &id).await, "succeeded");
}

#[tokio::test]
async fn an_expiry_under_the_same_set_spends_the_attempt_and_dead_letters_at_the_limit() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    db.expire_lease(&id).await;
    let same = db.peer("same-set").await.for_activation(rev("set-1"));
    assert!(
        RunStore::claim(&same).await.unwrap().is_none(),
        "집합이 같으면 시도를 쓰고, 한도에 닿으면 집지 않고 dead letter다"
    );
    assert_eq!(state_of_run(&db, &id).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "lease_expired"
    );
}

#[tokio::test]
async fn a_row_without_a_recorded_set_spends_the_attempt_even_when_the_set_changed() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    // 이 열이 생기기 전의 행: 기록이 없다
    db.sql(&format!(
        "update bot_runs set claim_set = null where id = '{}'",
        id.0
    ))
    .await;
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(RunStore::claim(&new).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &id).await, "dead");
}

#[tokio::test]
async fn an_unpinned_handle_neither_records_a_set_nor_forgives_one() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    // 고정하지 않은 핸들이 집었다: 기록이 없다 → 집합이 바뀌어도 용서하지 않는다
    let plain = claimed_by(&db, &db.store, "plain").await;
    let plain_id = plain.attempt.run.clone();
    assert_eq!(recorded_set(&db, &plain_id).await, None);
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&plain_id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(RunStore::claim(&new).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &plain_id).await, "dead");

    // 집합을 고정했던 claim이라도 고정하지 않은 핸들이 인계하면 «지금 집합»을 알 수 없다 → 용서하지 않는다
    let pinned = db.peer("pinned").await.for_activation(rev("set-2"));
    let run = claimed_by(&db, &pinned, "pinned").await;
    let id = run.attempt.run.clone();
    assert_eq!(recorded_set(&db, &id).await.as_deref(), Some("set-2"));
    remap(&db, "set-2", "set-3").await;
    db.expire_lease(&id).await;
    assert!(RunStore::claim(&db.store).await.unwrap().is_none());
    assert_eq!(state_of_run(&db, &id).await, "dead");
}

#[tokio::test]
async fn only_the_first_expiry_after_a_replacement_is_forgiven() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let first = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("첫 인계는 용서");
    assert_eq!(attempts(&db, &first.attempt.run).await, 0);
    // 새 집합에서 또 만료되면 시도를 쓴다
    db.expire_lease(&id).await;
    let next = db.peer("next-worker").await.for_activation(rev("set-2"));
    assert!(
        RunStore::claim(&next).await.unwrap().is_none(),
        "같은 집합의 두 번째 만료는 시도를 쓴다"
    );
    assert_eq!(state_of_run(&db, &id).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "lease_expired"
    );
}

#[tokio::test]
async fn a_cancel_request_still_ends_the_run_when_the_set_changed() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    remap(&db, "set-1", "set-2").await;
    db.sql(&format!(
        "update bot_runs set cancel_requested = true where id = '{}'",
        id.0
    ))
    .await;
    db.expire_lease(&id).await;
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    assert!(
        RunStore::claim(&new).await.unwrap().is_none(),
        "취소가 먼저다: 집지 않고 취소로 끝낸다"
    );
    assert_eq!(state_of_run(&db, &id).await, "cancelled");
}

#[tokio::test]
async fn an_unreadable_set_writes_nothing_and_the_retry_after_the_replacement_is_forgiven() {
    let Some(db) = TestDb::with(one_attempt()).await else {
        return;
    };
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    let run = claimed_by(&db, &old, "r1").await;
    let id = run.attempt.run.clone();
    db.expire_lease(&id).await;
    let before = (
        state_of_run(&db, &id).await,
        attempts(&db, &id).await,
        db.scalar::<i64>("select claim_epoch from bot_runs").await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );

    // 교체가 진행 중이라 지금 집합(set-1)을 확인하는 잠금이 시간 안에 풀리지 않는다(읽기 실패)
    let mut connection = db.raw().await;
    let (tx, _writer) = replacement_in_flight(&mut connection, "set-2").await;
    let options = (*db.store.pool().connect_options())
        .clone()
        .options([("lock_timeout", "300")]);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .unwrap();
    let hasty = Store::from_pool(pool, db.store.config().clone()).for_activation(rev("set-1"));
    let unread = RunStore::claim(&hasty).await;
    assert!(
        matches!(unread, Err(Error::Unavailable)),
        "집합을 읽지 못하면 집지 않고 실패한다: {unread:?}"
    );
    let after = (
        state_of_run(&db, &id).await,
        attempts(&db, &id).await,
        db.scalar::<i64>("select claim_epoch from bot_runs").await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );
    assert_eq!(
        before, after,
        "아무것도 바꾸지 않았다(시도·세대·감사 그대로)"
    );

    // 교체가 커밋되면 옛 집합의 핸들은 더 집지 못하고(Stale), 새 집합의 워커가 인계해 용서받는다
    tx.commit().await.unwrap();
    assert!(matches!(RunStore::claim(&hasty).await, Err(Error::Stale)));
    let new = db.peer("new-worker").await.for_activation(rev("set-2"));
    let again = RunStore::claim(&new)
        .await
        .unwrap()
        .expect("새 워커가 인계한다");
    assert_eq!(again.attempt.run, id);
    assert_eq!(attempts(&db, &id).await, 0);
}
