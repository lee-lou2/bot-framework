//! 실제 BrokerService(기억 공유 경로)를 PG 위에서 돌린다: 카드 → 소유자 승인 → 한 번의 공유 커밋 → 재전달, 카드를 본 뒤 편집.

mod support;

use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot},
};
use bot_core::{
    admin::AdminRead,
    command::*,
    id::*,
    ledger::ApprovalState,
    port::{AuditLog, Broker, Error},
    state::{ConversationKind, RunContext},
};
use sha2::Digest;
use std::sync::Arc;
use support::{admin::*, *};

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

/// 소유자의 CLI 실행(기억 공유 스코프가 있다). 승인자는 소유자 본인이다.
async fn share_run(db: &TestDb, request: &str) -> RunContext {
    db.publish().await;
    let mut a = accept_as(
        "personal",
        request,
        OWNER,
        ConversationKind::Cli,
        "c1",
        "공유해 줘",
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
    let body = "공용으로 쓸 메모\n두 번째 줄";
    let run = share_run(&db, "r1").await;
    let id = create(&db, &run, body).await;

    // (1) 카드: 요청자·승인자인 소유자에게만 본문 전체가 보인다
    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("승인 대기여야 한다")
    };
    let card = AdminRead::approval(&db.store, &owner(), nonce.clone())
        .await
        .unwrap();
    let shown: Vec<_> = card.render.iter().filter(|(l, _)| l == "본문").collect();
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].1, body, "본문 전체");
    assert_eq!(
        memory_share_hash(&id, &sha2::Sha256::digest(shown[0].1.as_bytes()).into()),
        card.args_sha256,
        "보이는 본문 = 승인 지문"
    );
    assert!(
        AdminRead::approval(&db.store, &operator(), nonce.clone())
            .await
            .unwrap()
            .render
            .is_empty(),
        "다른 운영자에게는 본문이 없다"
    );
    assert_eq!(visibility(&db, &id).await, "private", "승인 전에는 공유 0");

    // 소유자가 승인하면 같은 세대로 재개되고, 같은 호출 ID가 공유를 한 번 커밋한다
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
        panic!("공유가 끝나야 한다")
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

    // (2) 완료한 호출의 재전달: 영수증 그대로, 다시 공유하지 않는다
    let again = broker
        .call(share_call(&resumed.attempt, "s1", &id))
        .await
        .unwrap();
    assert!(matches!(again, CallResult::Done(r) if r == receipt));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await,
        1,
        "재공유 0"
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
    let id = create(&db, &run, "카드에 보인 본문").await;
    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("승인 대기여야 한다")
    };
    // 소유자가 다른 실행에서 본문을 고친 뒤 승인한다
    let editor = share_run(&db, "r2").await;
    memories(&db)
        .change(
            editor.attempt.clone(),
            MemoryChange::Edit {
                id: id.clone(),
                text: "카드를 본 뒤에 바뀐 본문".into(),
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
        "같은 호출 ID의 지문은 바뀐 본문과 다르다: {result:?}"
    );
    assert_eq!(visibility(&db, &id).await, "private");
    assert_eq!(
        calls(&db).approval(nonce).await.unwrap().state,
        ApprovalState::Approved,
        "승인은 소비되지 않았다"
    );
}

#[tokio::test]
async fn a_finished_share_replays_its_receipt_through_edits_deletes_and_activation_changes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let broker = rig(&db);
    let run = share_run(&db, "r1").await;
    let id = create(&db, &run, "공용으로 쓸 메모\n두 번째 줄").await;
    let CallResult::Awaiting(nonce) = broker
        .call(share_call(&run.attempt, "s1", &id))
        .await
        .unwrap()
    else {
        panic!("승인 대기여야 한다")
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
        panic!("공유가 끝나야 한다")
    };
    let shares = || async {
        db.scalar::<i64>("select count(*) from bot_audit where action = 'memory.share'")
            .await
    };
    assert_eq!(shares().await, 1);

    // 같은 호출 ID에 다른 기억 ID는 충돌이다(영수증을 다른 기억에 돌려주지 않는다)
    let other = create(&db, &resumed, "unrelated").await;
    assert!(matches!(
        broker
            .call(share_call(&resumed.attempt, "s1", &other))
            .await,
        Err(Error::Conflict)
    ));
    // 공유를 마친 뒤 본문을 고치면 private으로 돌아가지만, 같은 호출의 재전달은 같은 영수증이고 다시 공유하지 않는다
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
    // 지운 뒤에도 같다
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

    // 로컬 카탈로그는 r1인데 PG의 활성 판본이 바뀌면 캐시된 영수증도 내주지 않는다: 다른 판본 → Conflict, 비활성 → Denied
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
