//! Concurrency and cancellation races: serializing permission checks with active-revision swaps,
//! root private-memory flagging, and accepting receipts of already-started effects after cancellation.

use crate::support::*;
use bot_core::{
    command::*, id::*, intake::*, ledger::*, port::*, profile::ActiveRevisions, state::*,
};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};

fn owner() -> PrincipalId {
    PrincipalId("local:owner".into())
}

fn rev(s: &str) -> Revision {
    Revision(s.into())
}

async fn still_blocked<T>(task: &mut tokio::task::JoinHandle<T>) {
    let waited = tokio::time::timeout(Duration::from_millis(400), &mut *task).await;
    assert!(waited.is_err(), "finished before the lock was released");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revision_change_in_flight_blocks_a_start_until_it_commits_and_then_rejects_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;

    let mut hold = db.raw().await;
    let mut tx = hold.begin().await.unwrap();
    sqlx::query("select 1 from bot_active where singleton for update")
        .execute(&mut *tx)
        .await
        .unwrap();

    let starter = db.peer("starter").await;
    let attempt = run.attempt.clone();
    let mut start =
        tokio::spawn(
            async move { CallLedger::start(&starter, start_call(&attempt, "c1", None)).await },
        );
    still_blocked(&mut start).await;
    sqlx::query("update bot_active_profiles set revision = 'r2' where profile = 'personal'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(start.await.unwrap(), Err(Error::Stale)));
    assert!(matches!(
        calls(&db)
            .begin(run.attempt.clone(), call("c1"), tool("web.search"), [1; 32])
            .await
            .unwrap()
            .phase,
        CallPhase::Open { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revision_change_in_flight_blocks_a_send_until_it_commits_and_then_rejects_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish().await;
    let dm = accept_as(
        "personal",
        "r",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    runs(&db).accept(dm).await.unwrap();
    let got = runs(&db).claim().await.unwrap().unwrap();
    runs(&db).finish(got.attempt, done("reply")).await.unwrap();
    let key = outbox(&db)
        .claim(
            &bot_core::intake::DeliveryScope::all(bot_core::intake::PushTransportId::SLACK),
            1,
        )
        .await
        .unwrap()
        .remove(0)
        .key;

    let mut hold = db.raw().await;
    let mut tx = hold.begin().await.unwrap();
    sqlx::query("select 1 from bot_active where singleton for update")
        .execute(&mut *tx)
        .await
        .unwrap();
    let sender = db.peer("sender").await;
    let sent_key = key.clone();
    let mut begin = tokio::spawn(async move { Outbox::begin(&sender, sent_key, rev("r1")).await });
    still_blocked(&mut begin).await;
    sqlx::query("delete from bot_active_profiles where profile = 'personal'")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(begin.await.unwrap(), Err(Error::Stale)));
    assert_eq!(
        db.scalar::<String>("select state from bot_outbox").await,
        "pending",
        "must remain unsent"
    );
    let _ = key;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn marking_the_root_private_blocks_a_child_tool_start_until_it_commits() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = leased(&db, "p").await;
    let delegate = Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: parent.attempt.clone(),
        step: "1".into(),
        child: AgentId("main/research".into()),
        instruction: "research".into(),
        scopes: scopes(&["run"]),
    };
    db.store.delegate(delegate).await.unwrap();
    let child = runs(&db).claim().await.unwrap().unwrap();
    begin_call(&db, &child.attempt, "c1").await;

    let mut hold = db.raw().await;
    let mut tx = hold.begin().await.unwrap();
    sqlx::query("select 1 from bot_runs where id = $1 for update")
        .bind(&child.root.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    let starter = db.peer("starter").await;
    let attempt = child.attempt.clone();
    let mut start =
        tokio::spawn(
            async move { CallLedger::start(&starter, start_call(&attempt, "c1", None)).await },
        );
    still_blocked(&mut start).await;
    sqlx::query("update bot_runs set private_memory_read = true where id = $1")
        .bind(&child.root.0)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(start.await.unwrap(), Err(Error::Conflict)));
    assert!(matches!(
        calls(&db)
            .begin(
                child.attempt.clone(),
                call("c1"),
                tool("web.search"),
                [1; 32]
            )
            .await
            .unwrap()
            .phase,
        CallPhase::Open { .. }
    ));
}

#[tokio::test]
async fn a_cancel_request_stops_new_effects_but_keeps_the_receipt_of_a_started_one() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let step = effects(&db)
        .request(EffectRequest {
            attempt: run.attempt.clone(),
            call: call("c1"),
            step: "upload".into(),
            args: JsonPayload("{}".into()),
        })
        .await
        .unwrap();
    assert!(step.send);
    begin_call(&db, &run.attempt, "c2").await;

    db.store
        .cancel(
            ProfileId("personal".into()),
            run.attempt.run.clone(),
            owner(),
            "stop".into(),
        )
        .await
        .unwrap();
    assert!(
        runs(&db)
            .renew(run.attempt.clone())
            .await
            .unwrap()
            .cancel_requested
    );
    let stale = |r: Result<(), Error>| matches!(r, Err(Error::Stale));
    assert!(stale(
        calls(&db).start(start_call(&run.attempt, "c2", None)).await
    ));
    let more = EffectRequest {
        attempt: run.attempt.clone(),
        call: call("c1"),
        step: "notify".into(),
        args: JsonPayload("{}".into()),
    };
    assert!(matches!(
        effects(&db).request(more).await,
        Err(Error::Stale)
    ));
    assert!(stale(
        calls(&db)
            .begin(run.attempt.clone(), call("c3"), tool("web.search"), [1; 32])
            .await
            .map(|_| ())
    ));
    assert!(stale(
        calls(&db)
            .open_approval(open_approval(binding(&run.attempt, "c2", "n1")))
            .await
    ));

    // Receipts and completion of already-started effects must still be recorded after cancel.
    let receipt = EffectStatus {
        effect: step.status.effect.clone(),
        state: EffectState::Confirmed,
        resolution: None,
        receipt: Some("file-17".into()),
    };
    effects(&db)
        .record(run.attempt.clone(), call("c1"), receipt)
        .await
        .unwrap();
    assert!(
        effects(&db)
            .unresolved(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .is_empty()
    );
    let done = FinishCall {
        attempt: run.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Done(JsonPayload(r#"{"receipt":"file-17"}"#.into())),
    };
    calls(&db).finish(done).await.unwrap();
    assert_eq!(
        db.scalar::<String>("select state from bot_calls where call = 'c1'")
            .await,
        "done"
    );

    assert_eq!(
        runs(&db)
            .fail(run.attempt.clone(), Failure::Permanent("cancelled"))
            .await
            .unwrap(),
        Retry::Dead
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "cancelled"
    );
}

#[tokio::test]
async fn an_active_set_published_later_does_not_resurrect_a_revoked_profile() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    db.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            BTreeMap::from([(ProfileId("work".into()), rev("w1"))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        calls(&db).start(start_call(&run.attempt, "c1", None)).await,
        Err(Error::Stale)
    ));
}
