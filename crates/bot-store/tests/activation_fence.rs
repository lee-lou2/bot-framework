mod support;

use bot_core::{
    admin::{AdminAct, HoldRef, HoldVerdict},
    id::*,
    intake::{DeliveryOutcome, DeliveryScope, Outbox, PushTransportId, RequestKey},
    ledger::{ApprovalState, CallLedger, Decision, PrivateLedger, PrivateOutcome},
    port::{Error, RunAdmin, RunStore},
    profile::ActiveRevisions,
    state::ConversationKind,
};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};
use support::*;

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

/// 프로필 파일은 그대로 두고 호스트의 신원 매핑만 교체한다.
async fn remap(db: &TestDb) {
    let profiles = BTreeMap::from([
        (profile_id(), rev("r1")),
        (ProfileId("work".into()), rev("r1")),
    ]);
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            profiles,
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn admission_waits_for_a_mapping_only_change_and_uses_the_committed_mapping() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let gateway = db.peer("gateway").await.for_activation(rev("set-1"));
    let request = accept("personal", "new", "hello");
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = 'set-2' where singleton returning pg_backend_pid()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let task = tokio::spawn(async move { gateway.accept(request).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            ).bind(writer).fetch_one(db.store.pool()).await.unwrap();
            if waiting { break }
            assert!(!task.is_finished());
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
    db.store
        .for_activation(rev("set-2"))
        .accept(accept("personal", "new", "hello"))
        .await
        .unwrap();
}

#[tokio::test]
async fn remapping_revokes_replay_status_approval_and_operator_authority() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = db.store.for_activation(rev("set-1"));
    let run = leased(&db, "existing").await;
    begin_call(&db, &run.attempt, "c1").await;
    let nonce = ApprovalId("n1".into());
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    remap(&db).await;
    let request = accept("personal", "existing", "hi");
    let key = RequestKey {
        profile: profile_id(),
        binding: request.event.binding.clone(),
        principal: request.event.principal.clone(),
        request: request.event.request.clone(),
    };
    assert!(matches!(old.accept(request).await, Err(Error::Stale)));
    assert!(matches!(old.status(key.clone()).await, Err(Error::Stale)));
    let actor = PrincipalId(OWNER.into());
    let decision = || Decision {
        nonce: nonce.clone(),
        actor: actor.clone(),
        allow: true,
    };
    assert!(matches!(old.decide(decision()).await, Err(Error::Stale)));
    assert!(matches!(
        old.supersede(nonce.clone(), actor.clone()).await,
        Err(Error::Stale)
    ));
    let id = &run.attempt.run;
    assert!(matches!(
        old.cancel(profile_id(), id.clone(), actor.clone(), "cancel".into())
            .await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        old.resume(profile_id(), id.clone(), actor.clone(), "resume".into())
            .await,
        Err(Error::Stale)
    ));
    let hold = HoldRef {
        hold_id: HoldId("hold-1".into()),
        profile: profile_id(),
        run: id.clone(),
        call: call("c1"),
        effect: None,
    };
    assert!(matches!(
        old.resolve_hold(
            actor.clone(),
            hold,
            HoldVerdict::Confirmed {
                receipt: "ok".into()
            }
        )
        .await,
        Err(Error::Stale)
    ));
    let new = db.store.for_activation(rev("set-2"));
    assert_eq!(new.status(key).await.unwrap().unwrap().run, *id);
    assert!(matches!(
        new.approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Pending
    ));
    assert!(matches!(
        new.decide(decision()).await.unwrap(),
        ApprovalState::Approved
    ));
}

#[tokio::test]
async fn remapping_fences_private_and_reply_starts_but_accepts_prior_receipts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = db.store.for_activation(rev("set-1"));
    let reserved = private::reserve(&db, "reserved", 1).await;
    let started = private::started(&db, "started", 2).await;
    for name in ["sent", "unsent"] {
        let run = leased_in(&db, name, ConversationKind::Direct).await;
        runs(&db).finish(run.attempt, done(name)).await.unwrap();
    }
    let scope = DeliveryScope::all(PushTransportId::SLACK);
    let mut replies = Outbox::claim(&old, &scope, 2).await.unwrap();
    assert_eq!(replies.len(), 2);
    let sent = replies.pop().unwrap().key;
    let unsent = replies.pop().unwrap().key;
    Outbox::begin(&old, sent.clone(), rev("r1")).await.unwrap();
    remap(&db).await;
    assert!(matches!(
        PrivateLedger::begin(&old, private::request("new", 3)).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        PrivateLedger::begin(&old, private::request("reserved", 1)).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        PrivateLedger::start(&old, private::start_of(&reserved, 1)).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        Outbox::claim(&old, &scope, 1).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        Outbox::begin(&old, unsent.clone(), rev("r1")).await,
        Err(Error::Stale)
    ));
    PrivateLedger::finish(&old, started, PrivateOutcome::Completed)
        .await
        .unwrap();
    old.settle(
        sent,
        DeliveryOutcome::Confirmed {
            receipt: "received".into(),
        },
    )
    .await
    .unwrap();
    let new = db.store.for_activation(rev("set-2"));
    PrivateLedger::start(&new, private::start_of(&reserved, 1))
        .await
        .unwrap();
    Outbox::begin(&new, unsent, rev("r1")).await.unwrap();
}
