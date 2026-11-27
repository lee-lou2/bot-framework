//! Read-only call ledger queries (PG): `lookup` and `active_revision` under live-claim fencing.

use crate::support::{private::*, *};
use bot_core::{
    id::*,
    ledger::{CallPhase, PrivateLedger, PrivateOutcome},
    port::*,
    profile::ActiveRevisions,
};
use std::collections::BTreeMap;

fn set(items: &[(&str, &str)]) -> BTreeMap<ProfileId, Revision> {
    items
        .iter()
        .map(|(p, r)| (ProfileId((*p).into()), Revision((*r).into())))
        .collect()
}

#[tokio::test]
async fn lookup_reads_one_existing_call_of_this_run_without_changing_anything() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "a").await;
    assert!(
        calls(&db)
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .is_none(),
        "missing call returns None"
    );
    begin_call(&db, &run.attempt, "c1").await;
    let found = calls(&db)
        .lookup(run.attempt.clone(), call("c1"))
        .await
        .unwrap()
        .expect("existing call");
    assert_eq!(
        (found.call.0.as_str(), found.tool.0.as_str()),
        ("c1", "web.search")
    );
    assert_eq!(found.args_sha256, [1; 32]);
    assert!(matches!(found.phase, CallPhase::Open { approval: None }));

    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    calls(&db)
        .finish(bot_core::ledger::FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: bot_core::ledger::Outcome::Done(bot_core::command::JsonPayload(
                r#"{"ok":true}"#.into(),
            )),
        })
        .await
        .unwrap();
    let done = calls(&db)
        .lookup(run.attempt.clone(), call("c1"))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(done.phase, CallPhase::Done(r) if r.0 == r#"{"ok":true}"#));

    let (calls_before, audits_before) = (
        db.scalar::<i64>("select count(*) from bot_calls").await,
        db.scalar::<i64>("select count(*) from bot_audit").await,
    );
    for _ in 0..3 {
        calls(&db)
            .lookup(run.attempt.clone(), call("nope"))
            .await
            .unwrap();
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls").await,
        calls_before
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits_before
    );

    let other = leased(&db, "b").await;
    assert!(
        calls(&db)
            .lookup(other.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn both_reads_refuse_a_stale_claim_a_cancelled_run_and_a_private_attempt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "a").await;
    begin_call(&db, &run.attempt, "c1").await;
    let mut no_claim = run.attempt.clone();
    no_claim.claim = None;
    let mut other_epoch = run.attempt.clone();
    other_epoch.claim = Some(ClaimEpoch(99));
    for bad in [no_claim, other_epoch] {
        assert!(matches!(
            calls(&db).lookup(bad.clone(), call("c1")).await,
            Err(Error::Stale)
        ));
        assert!(matches!(
            calls(&db).active_revision(bad).await,
            Err(Error::Stale)
        ));
    }
    let mut ghost = run.attempt.clone();
    ghost.run = RunId("ghost".into());
    assert!(matches!(
        calls(&db).active_revision(ghost).await,
        Err(Error::Stale)
    ));
    db.expire_lease(&run.attempt.run).await;
    assert!(matches!(
        calls(&db).lookup(run.attempt.clone(), call("c1")).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        calls(&db).active_revision(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    let taken = runs(&db).claim().await.unwrap().unwrap();
    assert!(matches!(
        calls(&db).active_revision(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert!(
        calls(&db)
            .lookup(taken.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .is_some()
    );
    db.sql("update bot_runs set cancel_requested = true").await;
    assert!(matches!(
        calls(&db).lookup(taken.attempt.clone(), call("c1")).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        calls(&db).active_revision(taken.attempt.clone()).await,
        Err(Error::Stale)
    ));

    // Private DM records cannot be read via the standard call ledger.
    let private = reserve(&db, "p", 1).await;
    assert!(matches!(
        calls(&db).lookup(private.clone(), call("c1")).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        calls(&db).active_revision(private.clone()).await,
        Err(Error::Denied)
    ));
    let _ = PrivateLedger::finish(&db.store, private, PrivateOutcome::RetryBeforeStart).await;
}

#[tokio::test]
async fn active_revision_follows_the_published_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "a").await;
    assert_eq!(
        calls(&db)
            .active_revision(run.attempt.clone())
            .await
            .unwrap(),
        Some(Revision("r1".into()))
    );
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            set(&[("personal", "r2"), ("work", "r1")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&db)
            .active_revision(run.attempt.clone())
            .await
            .unwrap(),
        Some(Revision("r2".into()))
    );
    db.store
        .replace(
            Some(Revision("set-2".into())),
            Revision("set-3".into()),
            set(&[("work", "r1")]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        calls(&db)
            .active_revision(run.attempt.clone())
            .await
            .unwrap(),
        None
    );
}
