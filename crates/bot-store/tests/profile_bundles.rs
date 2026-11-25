//! 프로필 판본 묶음의 보관(PG)과 작업이 고정한 판본 목록: 재배포 뒤 옛 판본 복원의 저장소 쪽 근거.
//! - 묶음은 `(프로필, 판본)`으로 보관하고 판본이 바이트의 SHA-256이 아니면 거절한다. 같은 바이트는 다시 보관해도 되고, 바꾸거나 지울 수 없다.
//! - 고정 판본 목록은 활성 집합·끝나지 않은 실행·재개할 수 있는 dead letter·대기 카드·정적 구간(열린 구간과 커서가 끝에 못 이른 구간)·활성 사용자 예약의 생성 실행을 한 번에 읽는다.

mod support;

use bot_core::{
    id::*,
    port::Error,
    profile::{ActiveRevisions, ProfileBundles},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use support::*;

fn sha(bytes: &[u8]) -> Revision {
    Revision(
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

fn personal() -> ProfileId {
    ProfileId("personal".into())
}

/// `/v1/models`의 `created`가 쓰는 값: 처음 보관한 시각이고, 다시 보관하거나 읽어도 바뀌지 않으며, 없는 판본은 None이다(지어내지 않는다).
#[tokio::test]
async fn the_first_store_time_is_stable_and_missing_bundles_have_none() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let store = &db.store;
    let bytes = b"BOTPROFILE1\nstamp".to_vec();
    let revision = sha(&bytes);
    assert_eq!(
        store.created_ms(&personal(), &revision).await.unwrap(),
        None
    );
    store
        .put_bundle(personal(), revision.clone(), bytes.clone())
        .await
        .unwrap();
    let first = store
        .created_ms(&personal(), &revision)
        .await
        .unwrap()
        .expect("보관한 묶음의 시각");
    assert!(
        first > 1_700_000_000_000,
        "DB 시각(밀리초)이어야 해요: {first}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    store
        .put_bundle(personal(), revision.clone(), bytes)
        .await
        .unwrap();
    assert_eq!(
        store.created_ms(&personal(), &revision).await.unwrap(),
        Some(first),
        "같은 바이트를 다시 보관해도 처음 시각 그대로"
    );
    assert_eq!(
        store
            .created_ms(&ProfileId("work".into()), &revision)
            .await
            .unwrap(),
        None,
        "다른 프로필의 같은 판본은 따로다"
    );
}

#[tokio::test]
async fn a_bundle_is_stored_by_its_own_hash_once_and_never_changes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let store = &db.store;
    let bytes = b"BOTPROFILE1\nfirst".to_vec();
    let revision = sha(&bytes);
    assert_eq!(store.bundle(&personal(), &revision).await.unwrap(), None);
    store
        .put_bundle(personal(), revision.clone(), bytes.clone())
        .await
        .unwrap();
    assert_eq!(
        store.bundle(&personal(), &revision).await.unwrap(),
        Some(bytes.clone())
    );
    // 같은 키로 같은 바이트는 다시 보관해도 된다
    store
        .put_bundle(personal(), revision.clone(), bytes.clone())
        .await
        .unwrap();
    // 다른 프로필의 같은 판본은 따로다
    assert_eq!(
        store
            .bundle(&ProfileId("work".into()), &revision)
            .await
            .unwrap(),
        None
    );
    // 판본이 바이트의 해시가 아니면 보관하지 않는다
    for wrong in [
        Revision("r1".into()),
        sha(b"other"),
        Revision(revision.0.to_uppercase()),
    ] {
        assert!(
            matches!(
                store
                    .put_bundle(personal(), wrong.clone(), bytes.clone())
                    .await,
                Err(Error::Invalid)
            ),
            "{wrong:?}"
        );
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_profile_bundles")
            .await,
        1
    );
    // DB가 불변과 키 = 해시를 강제한다(저장소 코드를 거치지 않는 쓰기도 막힌다)
    for sql in [
        "update bot_profile_bundles set bundle = '\\x00'",
        "delete from bot_profile_bundles",
        "insert into bot_profile_bundles (profile, revision, bundle) values ('personal', 'not-the-hash', '\\x01')",
    ] {
        assert!(
            sqlx::query(sql).execute(db.store.pool()).await.is_err(),
            "{sql}"
        );
    }
    assert_eq!(
        store.bundle(&personal(), &revision).await.unwrap(),
        Some(bytes)
    );
}

#[tokio::test]
async fn different_bytes_under_a_stored_key_are_refused_as_corruption() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let bytes = b"BOTPROFILE1\noriginal".to_vec();
    let revision = sha(&bytes);
    // 손상 흉내: 키 = 해시 검사를 끄고 같은 키에 다른 바이트를 둔다
    db.sql("alter table bot_profile_bundles drop constraint bot_profile_bundles_check")
        .await;
    sqlx::query(
        "insert into bot_profile_bundles (profile, revision, bundle) values ('personal', $1, $2)",
    )
    .bind(&revision.0)
    .bind(b"tampered".to_vec())
    .execute(db.store.pool())
    .await
    .unwrap();
    assert!(matches!(
        db.store
            .put_bundle(personal(), revision.clone(), bytes)
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.store.bundle(&personal(), &revision).await.unwrap(),
        Some(b"tampered".to_vec()),
        "기존 바이트는 그대로다"
    );
}

/// 이 실행 하나를 정한 판본 · 상태로 만든다(접수한 뒤 SQL로 맞춘다).
async fn run_at(db: &TestDb, request: &str, revision: &str, state: &str, private: bool) -> RunId {
    let run = runs(db)
        .accept(accept("personal", request, "hi"))
        .await
        .unwrap();
    let id = run.0.clone();
    let extra = match state {
        "waiting" => ", wait_kind = 'approval'",
        _ => "",
    };
    db.sql(&format!(
        "update bot_runs set revision = '{revision}', state = '{state}', private = {private}, lease_until_ms = null, lease_owner = null{extra} where id = '{id}'"
    ))
    .await;
    run
}

/// 이번에 게시·합류할 집합(`Publication::profiles`): 두 프로필 모두 판본 r1.
fn both() -> BTreeMap<ProfileId, Revision> {
    BTreeMap::from([
        (personal(), Revision("r1".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ])
}

/// 목록을 읽는다(이번 집합 = personal·work r1).
async fn pinned(db: &TestDb) -> BTreeSet<(ProfileId, Revision)> {
    db.store.pinned_revisions(&both()).await.unwrap()
}

fn pairs(items: &[(&str, &str)]) -> BTreeSet<(ProfileId, Revision)> {
    items
        .iter()
        .map(|(p, r)| (ProfileId((*p).into()), Revision((*r).into())))
        .collect()
}

#[tokio::test]
async fn pinned_revisions_cover_the_active_set_and_every_run_that_can_still_use_a_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 아무 작업도 없을 때는 이번 집합의 현재 쌍뿐이다
    assert_eq!(
        pinned(&db).await,
        pairs(&[("personal", "r1"), ("work", "r1")])
    );
    for (request, revision, state, private) in [
        ("ready", "r-ready", "ready", false),
        ("retry", "r-retry", "retry", false),
        ("waiting", "r-waiting", "waiting", false),
        ("dead", "r-dead", "dead", false),
        // 끝난 실행·취소·재개할 수 없는 dead(비공개 접수)는 판본을 붙들지 않는다
        ("done", "r-done", "succeeded", false),
        ("cancelled", "r-cancelled", "cancelled", false),
        ("private-dead", "r-private-dead", "dead", true),
    ] {
        run_at(&db, request, revision, state, private).await;
    }
    assert_eq!(
        pinned(&db).await,
        pairs(&[
            ("personal", "r1"),
            ("work", "r1"),
            ("personal", "r-ready"),
            ("personal", "r-retry"),
            ("personal", "r-waiting"),
            ("personal", "r-dead"),
        ])
    );
}

#[tokio::test]
async fn pinned_revisions_include_static_segments_that_are_open_or_not_yet_caught_up() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // (트리거, 구간 끝, 커서, 판본)
    for (trigger, until, cursor, revision) in [
        ("open", "null", 500, "r-open"),
        ("pending", "100", 50, "r-pending"),
        ("caught-up", "100", 100, "r-caught-up"),
    ] {
        db.sql(&format!(
            "insert into bot_schedule_segments (profile, agent, trigger, from_ms, until_ms, revision, cron, timezone, principal, target)
             values ('personal', 'main', '{trigger}', 1, {until}, '{revision}', '* * * * *', 'UTC', 'service:x', '{{}}'::jsonb)"
        ))
        .await;
        db.sql(&format!(
            "insert into bot_schedule_cursors (profile, agent, trigger, cursor_ms) values ('personal', 'main', '{trigger}', {cursor})"
        ))
        .await;
    }
    let pinned = pinned(&db).await;
    assert!(pinned.contains(&(personal(), Revision("r-open".into()))));
    assert!(pinned.contains(&(personal(), Revision("r-pending".into()))));
    assert!(
        !pinned.contains(&(personal(), Revision("r-caught-up".into()))),
        "구간을 다 따라잡았으면 붙들지 않는다"
    );
}

#[tokio::test]
async fn pinned_revisions_include_the_run_that_created_an_active_user_schedule_and_a_pending_approval()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 활성 사용자 예약의 생성 실행은 끝났어도 그 판본이 필요하다. 취소한 예약은 아니다
    for (id, revision, state) in [
        ("keeps", "r-created-keeps", "active"),
        ("gone", "r-created-gone", "canceled"),
    ] {
        let origin = run_at(&db, &format!("origin-{id}"), revision, "succeeded", false).await;
        db.sql(&format!(
            "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                    ceiling, at_ms, instruction, cursor_ms, next_ms, state)
             values ('personal', '{id}', 'local:owner', 'main', '{}', 'c-{id}', 'desk', 'direct', 'c1', '{{run}}', 1000, '확인', 1,
                     {}, '{state}')",
            origin.0,
            if state == "active" { "1000" } else { "null" }
        ))
        .await;
    }
    // 승인 카드가 대기 중이면 그 실행의 판본이 필요하다(실행이 끝난 것으로 보여도 카드는 그 판본의 도구·정책을 가리킨다)
    let ctx = leased(&db, "approval").await;
    begin_call(&db, &ctx.attempt, "c1").await;
    db.sql(&format!(
        "update bot_runs set revision = 'r-card', state = 'succeeded', lease_until_ms = null, lease_owner = null where id = '{}'",
        ctx.attempt.run.0
    ))
    .await;
    db.sql(&format!(
        "insert into bot_approvals (nonce, profile, run, generation, call, requester, approver, risk, policy_revision, tool, tool_revision, args_sha256, expires_ms, card, state)
         values ('n1', 'personal', '{}', 0, 'c1', 'local:owner', 'local:owner', 'write', 'p1', 'web.search', 't1', '\\x{}', 9999999999999, '{{}}', 'pending')",
        ctx.attempt.run.0,
        "01".repeat(32)
    ))
    .await;
    let pinned = pinned(&db).await;
    assert!(pinned.contains(&(personal(), Revision("r-created-keeps".into()))));
    assert!(!pinned.contains(&(personal(), Revision("r-created-gone".into()))));
    assert!(pinned.contains(&(personal(), Revision("r-card".into()))));
    db.sql("update bot_approvals set state = 'consumed'").await;
    assert!(
        !db.store
            .pinned_revisions(&both())
            .await
            .unwrap()
            .contains(&(personal(), Revision("r-card".into()))),
        "소비한 카드는 붙들지 않는다"
    );
}

#[tokio::test]
async fn an_old_revision_nobody_references_is_not_required_even_though_it_is_the_one_active_now() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // DB의 활성 판본은 personal r1 · work r1이다. 이번에 게시할 집합은 personal r2 · work r1: 아무 작업도 r1을 참조하지 않으면 personal r1은 필수가 아니다
    let next = BTreeMap::from([
        (personal(), Revision("r2".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    assert_eq!(
        db.store.pinned_revisions(&next).await.unwrap(),
        pairs(&[("personal", "r2"), ("work", "r1")])
    );
    // 집합에 없는 프로필(여기서는 work)의 작업은 읽지 않는다
    let work = runs(&db)
        .accept(accept("work", "gone", "hi"))
        .await
        .unwrap();
    db.sql(&format!(
        "update bot_runs set revision = 'r-work-old', state = 'dead', lease_until_ms = null, lease_owner = null where id = '{}'",
        work.0
    ))
    .await;
    let only_personal = BTreeMap::from([(personal(), Revision("r2".into()))]);
    assert_eq!(
        db.store.pinned_revisions(&only_personal).await.unwrap(),
        pairs(&[("personal", "r2")]),
        "빠진 프로필의 dead letter는 필수가 아니다"
    );
    // 집합에 들면 읽는다
    assert!(
        db.store
            .pinned_revisions(&next)
            .await
            .unwrap()
            .contains(&(ProfileId("work".into()), Revision("r-work-old".into())))
    );
}

/// 이 집합(`next`)을 준비한 목록으로 게시한다.
async fn publish(
    db: &TestDb,
    active: &Option<Revision>,
    next: &BTreeMap<ProfileId, Revision>,
    prepared: BTreeSet<(ProfileId, Revision)>,
) -> Result<(), Error> {
    db.store
        .replace_prepared(
            active.clone(),
            Revision("set-2".into()),
            next.clone(),
            Vec::new(),
            BTreeMap::new(),
            prepared,
        )
        .await
}

#[tokio::test]
async fn publishing_rechecks_the_pinned_list_under_the_active_row_lock_and_refuses_what_the_preparation_missed()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let next = BTreeMap::from([
        (personal(), Revision("r2".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    // 준비: 옛 판본 personal r1을 아무도 참조하지 않아 필수 목록은 새 집합의 현재 쌍뿐이다(옛 번들 없이 준비가 끝난다)
    let prepared = db.store.pinned_revisions(&next).await.unwrap();
    // 준비 뒤 게시 전에 옛 프로세스(여전히 활성인 r1)가 새 실행을 접수한다
    runs(&db)
        .accept(accept("personal", "late", "hi"))
        .await
        .unwrap();
    let active = db.store.current().await.unwrap();
    // 준비한 목록에 없는 새 고정 참조가 있으면 아무것도 쓰지 않고 Stale이다
    assert!(matches!(
        publish(&db, &active, &next, prepared.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.store.current().await.unwrap(),
        active,
        "활성 집합은 그대로다"
    );
    assert_eq!(
        db.scalar::<String>("select revision from bot_active_profiles where profile = 'personal'")
            .await,
        "r1"
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_audit where action = 'active.replace' and verdict = 'r2'"
        )
        .await,
        0
    );
    // 다시 준비하면(그 참조를 덮으면) 게시된다
    let again = db.store.pinned_revisions(&next).await.unwrap();
    assert!(again.contains(&(personal(), Revision("r1".into()))));
    publish(&db, &active, &next, again).await.unwrap();
    assert_eq!(
        db.store.current().await.unwrap(),
        Some(Revision("set-2".into()))
    );
    assert_eq!(
        db.scalar::<String>("select revision from bot_active_profiles where profile = 'personal'")
            .await,
        "r2"
    );
}

#[tokio::test]
async fn a_plain_replace_does_not_recheck_and_a_prepared_replace_accepts_a_superset() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let next = BTreeMap::from([(personal(), Revision("r2".into()))]);
    runs(&db)
        .accept(accept("personal", "late", "hi"))
        .await
        .unwrap();
    // 준비한 목록이 필수 목록보다 넓어도 된다
    let wide = pairs(&[("personal", "r1"), ("personal", "r2"), ("work", "r1")]);
    db.store
        .replace_prepared(
            db.store.current().await.unwrap(),
            Revision("set-2".into()),
            next.clone(),
            Vec::new(),
            BTreeMap::new(),
            wide,
        )
        .await
        .unwrap();
    // 대조 없는 `replace`는 기존 동작 그대로다(다른 곳의 시험·도구가 쓴다)
    db.store
        .replace(
            db.store.current().await.unwrap(),
            Revision("set-3".into()),
            BTreeMap::from([(personal(), Revision("r3".into()))]),
            Vec::new(),
            BTreeMap::new(),
        )
        .await
        .unwrap();
}
