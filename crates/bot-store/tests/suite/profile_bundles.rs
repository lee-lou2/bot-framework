//! Profile bundle storage and pinned revision discovery for historical restore across redeploys.

use crate::support::*;
use bot_core::{
    id::*,
    port::Error,
    profile::{ActiveRevisions, ProfileBundles},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

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
        .expect("stored bundle timestamp");
    assert!(
        first > 1_700_000_000_000,
        "expected DB timestamp in milliseconds: {first}"
    );
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    store
        .put_bundle(personal(), revision.clone(), bytes)
        .await
        .unwrap();
    assert_eq!(
        store.created_ms(&personal(), &revision).await.unwrap(),
        Some(first),
        "re-storing identical bytes must preserve initial timestamp"
    );
    assert_eq!(
        store
            .created_ms(&ProfileId("work".into()), &revision)
            .await
            .unwrap(),
        None,
        "same revision under another profile is distinct"
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
    store
        .put_bundle(personal(), revision.clone(), bytes.clone())
        .await
        .unwrap();
    assert_eq!(
        store
            .bundle(&ProfileId("work".into()), &revision)
            .await
            .unwrap(),
        None
    );
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
    // DB constraints enforce immutability and key == sha256(bundle) even on direct SQL writes.
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
        "existing bytes must remain untouched"
    );
}

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

fn both() -> BTreeMap<ProfileId, Revision> {
    BTreeMap::from([
        (personal(), Revision("r1".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ])
}

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
    assert_eq!(
        pinned(&db).await,
        pairs(&[("personal", "r1"), ("work", "r1")])
    );
    for (request, revision, state, private) in [
        ("ready", "r-ready", "ready", false),
        ("retry", "r-retry", "retry", false),
        ("waiting", "r-waiting", "waiting", false),
        ("dead", "r-dead", "dead", false),
        // Terminal runs and non-resumable private dead runs do not pin revisions.
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
        "caught-up segment must not pin its revision"
    );
}

#[tokio::test]
async fn pinned_revisions_include_the_run_that_created_an_active_user_schedule_and_a_pending_approval()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for (id, revision, state) in [
        ("keeps", "r-created-keeps", "active"),
        ("gone", "r-created-gone", "canceled"),
    ] {
        let origin = run_at(&db, &format!("origin-{id}"), revision, "succeeded", false).await;
        db.sql(&format!(
            "insert into bot_user_schedules (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                    ceiling, at_ms, instruction, cursor_ms, next_ms, state)
             values ('personal', '{id}', 'local:owner', 'main', '{}', 'c-{id}', 'desk', 'direct', 'c1', '{{run}}', 1000, 'check', 1,
                     {}, '{state}')",
            origin.0,
            if state == "active" { "1000" } else { "null" }
        ))
        .await;
    }
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
        "consumed approval must not pin its revision"
    );
}

#[tokio::test]
async fn an_old_revision_nobody_references_is_not_required_even_though_it_is_the_one_active_now() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let next = BTreeMap::from([
        (personal(), Revision("r2".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    assert_eq!(
        db.store.pinned_revisions(&next).await.unwrap(),
        pairs(&[("personal", "r2"), ("work", "r1")])
    );
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
        "dead letter in an omitted profile is not required"
    );
    assert!(
        db.store
            .pinned_revisions(&next)
            .await
            .unwrap()
            .contains(&(ProfileId("work".into()), Revision("r-work-old".into())))
    );
}

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
    let prepared = db.store.pinned_revisions(&next).await.unwrap();
    // Accept a run pinned to old r1 between preparation and publication.
    runs(&db)
        .accept(accept("personal", "late", "hi"))
        .await
        .unwrap();
    let active = db.store.current().await.unwrap();
    assert!(matches!(
        publish(&db, &active, &next, prepared.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.store.current().await.unwrap(),
        active,
        "active set must remain unchanged"
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
