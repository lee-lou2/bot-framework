//! End-to-end `BrokerService` memory-share flow over PG: card render, owner approval, single commit, replay, and post-card edits.

use crate::support::{admin::*, *};
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot},
};
use bot_core::{
    admin::AdminRead,
    cards::LABEL_BODY,
    command::*,
    id::*,
    ledger::ApprovalState,
    port::{AuditLog, Broker, Error},
    state::{ConversationKind, RunContext},
};
use sha2::Digest;
use std::sync::Arc;

fn rig(db: &TestDb) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "memory.share"])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run", "memory.share"])
            .agent("main")
            .build(),
    );
    let store = Arc::new(db.store.clone());
    let schedules = store.clone();
    BrokerService::new(
        Parts {
            catalog,
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store,
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(schedules)
}

async fn share_run(db: &TestDb, request: &str) -> RunContext {
    db.publish().await;
    let mut a = accept_as(
        "personal",
        request,
        OWNER,
        ConversationKind::Cli,
        "c1",
        "please share",
    );
    a.scopes = scopes(&["run", "memory.share"]);
    a.approver = Some(PrincipalId(OWNER.into()));
    runs(db).accept(a).await.unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

fn share_call(attempt: &Attempt, call: &str, id: &MemoryId) -> ToolCall {
    ToolCall {
        attempt: attempt.clone(),
        call: CallId(call.into()),
        tool: ToolId("memory.share".into()),
        args: JsonPayload(serde_json::json!({ "id": id.0 }).to_string()),
    }
}

async fn visibility(db: &TestDb, id: &MemoryId) -> String {
    db.scalar::<String>(&format!(
        "select visibility from bot_memories where id = '{}'",
        id.0
    ))
    .await
}

#[tokio::test]
async fn the_owner_sees_the_whole_body_approves_and_the_share_commits_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let broker = rig(&db);
    let body = "shared note\nsecond line";
    let run = share_run(&db, "r1").await;
    let id = create(&db, &run, body).await;

    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("expected Awaiting approval")
    };
    let card = AdminRead::approval(&db.store, &owner(), nonce.clone())
        .await
        .unwrap();
    let shown: Vec<_> = card
        .render
        .iter()
        .filter(|(l, _)| l == LABEL_BODY)
        .collect();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].1, body, "full body must be shown to owner");
    assert_eq!(
        memory_share_hash(&id, &sha2::Sha256::digest(shown[0].1.as_bytes()).into()),
        card.args_sha256,
        "rendered body must match approval fingerprint"
    );
    assert!(
        AdminRead::approval(&db.store, &operator(), nonce.clone())
            .await
            .unwrap()
            .render
            .is_empty(),
        "non-owner operator must not see private body"
    );
    assert_eq!(
        visibility(&db, &id).await,
        "private",
        "must stay private before approval"
    );

    broker
        .decide(nonce.clone(), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &resumed.attempt);
    let CallResult::Done(receipt) = broker
        .call(share_call(&resumed.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("expected Done receipt")
    };
    assert_eq!(receipt.0, format!(r#"{{"id":"{}"}}"#, id.0));
    assert_eq!(visibility(&db, &id).await, "profile");
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Consumed
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1
    );

    let again = broker
        .call(share_call(&resumed.attempt, "s1", &id))
        .await
        .unwrap();
    assert!(matches!(again, CallResult::Done(r) if r == receipt));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1,
        "must not re-share"
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_edit_after_the_card_makes_the_same_call_id_conflict_and_leaves_the_approval_unconsumed()
{
    let Some(db) = TestDb::new().await else {
        return;
    };
    let broker = rig(&db);
    let run = share_run(&db, "r1").await;
    let id = create(&db, &run, "body shown on card").await;
    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("expected Awaiting approval")
    };
    let editor = share_run(&db, "r2").await;
    memories(&db)
        .change(
            editor.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "body changed after card".into(),
            },
        )
        .await
        .unwrap();
    broker
        .decide(nonce.clone(), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, run.attempt.run);
    let result = broker.call(share_call(&resumed.attempt, "s1", &id)).await;
    assert!(
        matches!(result, Err(Error::Conflict)),
        "call fingerprint must conflict with modified body: {result:?}"
    );
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        calls(&db).approval(nonce).await.unwrap().state,
        ApprovalState::Approved,
        "approval must remain unconsumed"
    );
}

#[tokio::test]
async fn a_finished_share_replays_its_receipt_through_edits_deletes_and_activation_changes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let broker = rig(&db);
    let run = share_run(&db, "r1").await;
    let id = create(&db, &run, "shared note\nsecond line").await;
    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("expected Awaiting approval")
    };
    broker
        .decide(nonce, PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let CallResult::Done(receipt) = broker
        .call(share_call(&resumed.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("expected Done receipt")
    };
    let shares = || async {
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await
    };
    assert_eq!(shares().await, 1);

    let other = create(&db, &resumed, "unrelated").await;
    assert!(matches!(
        broker
            .call(share_call(&resumed.attempt, "s1", &other))
            .await,
        Err(Error::Conflict)
    ));
    memories(&db)
        .change(
            resumed.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "changed after approval".into(),
            },
        )
        .await
        .unwrap();
    let edited = broker.call(share_call(&resumed.attempt, "s1", &id)).await;
    assert!(
        matches!(&edited, Ok(CallResult::Done(r)) if *r == receipt),
        "edited replay: {edited:?}"
    );
    assert_eq!(visibility(&db, &id).await, "private");
    memories(&db)
        .change(
            resumed.attempt.clone(),
            MemoryChange::Delete { id: id.clone() },
        )
        .await
        .unwrap();
    let deleted = broker.call(share_call(&resumed.attempt, "s1", &id)).await;
    assert!(
        matches!(&deleted, Ok(CallResult::Done(r)) if *r == receipt),
        "deleted replay: {deleted:?}"
    );
    assert_eq!(shares().await, 1);

    // Active revision mismatch refuses cached receipt (Conflict); inactive profile returns Denied.
    use bot_core::profile::ActiveRevisions;
    let personal = ProfileId("personal".to_string());
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            [(personal, Revision("r2".into()))].into(),
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        broker.call(share_call(&resumed.attempt, "s1", &id)).await,
        Err(Error::Conflict)
    ));
    db.store
        .replace(
            Some(Revision("set-2".into())),
            Revision("set-3".into()),
            Default::default(),
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        broker.call(share_call(&resumed.attempt, "s1", &id)).await,
        Err(Error::Denied)
    ));
    assert_eq!(shares().await, 1);
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}
