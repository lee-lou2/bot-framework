//! 한 모델 차례를 읽는 프로세스는 커밋된 레코드·도구·그룹만 본다.
mod support;

use bot_core::{
    cli::{CliGroup, CliUse, GROUP_PREFIX, RECORD_PREFIX, USE_PREFIX},
    command::{JsonPayload, Step},
    id::{ProfileId, Revision},
    ledger::CallLedger,
    port::Error,
};
use serde_json::json;
use support::*;

fn turn() -> Vec<Step> {
    let group = CliGroup {
        seq: 1,
        uses: vec!["u1".into()],
    };
    let plan = CliUse {
        seq: 1,
        turn: 1,
        pos: 0,
        name: "mcp__relay__agenda_list".into(),
        input: json!({}),
    };
    vec![
        Step {
            key: format!("{RECORD_PREFIX}1"),
            body: JsonPayload(json!("validated line").to_string()),
        },
        Step {
            key: format!("{USE_PREFIX}u1"),
            body: JsonPayload(serde_json::to_string(&plan).unwrap()),
        },
        Step {
            key: format!("{GROUP_PREFIX}1"),
            body: JsonPayload(serde_json::to_string(&group).unwrap()),
        },
    ]
}

#[tokio::test]
async fn a_committed_provider_response_needs_a_durable_cli_group_before_dispatch() {
    use bot_broker::front::{
        CallBook,
        cli::PlannedCalls,
        observe::{CheckpointSink, Observed, ObservedUse, TurnSink},
    };
    use bot_core::{
        cli::{CliResponse, RESPONSE_PREFIX},
        id::CallId,
    };
    use std::{sync::Arc, time::Duration};

    let Some(db) = TestDb::new().await else {
        return;
    };
    let attempt = leased(&db, "observed").await.attempt;
    let sink = CheckpointSink::new(Arc::new(db.store.clone()));
    let book = PlannedCalls::new(Arc::new(db.peer("broker").await), Duration::from_secs(2));
    let original = Observed {
        id: "msg-1".into(),
        uses: vec![ObservedUse {
            id: "u1".into(),
            name: "mcp__relay__agenda_list".into(),
            input: "{}".into(),
        }],
    };
    for _ in 0..2 {
        sink.completed(attempt.clone(), original.clone())
            .await
            .unwrap();
    }
    let proof = calls(&db)
        .step(attempt.clone(), format!("{RESPONSE_PREFIX}msg-1"))
        .await
        .unwrap()
        .unwrap();
    let proof: CliResponse = serde_json::from_str(&proof.0).unwrap();
    assert!(proof.is_valid());
    assert_eq!(proof.uses[0].name, original.uses[0].name);
    let mut changed = original;
    changed.uses[0].input = r#"{"changed":true}"#.into();
    assert!(matches!(
        sink.completed(attempt.clone(), changed).await,
        Err(Error::Conflict)
    ));
    let dispatch =
        |at, name: &str| book.call_id(at, "u1".into(), name.into(), JsonPayload("{}".into()));
    assert!(matches!(
        dispatch(attempt.clone(), "agenda_list").await,
        Err(Error::Unavailable)
    ));
    runs(&db)
        .checkpoint_batch(attempt.clone(), turn())
        .await
        .unwrap();
    let logical = CallId::cli(1, 0);
    assert_eq!(
        dispatch(attempt.clone(), "agenda_list").await.unwrap(),
        logical
    );
    assert!(matches!(
        dispatch(attempt.clone(), "agenda_delete").await,
        Err(Error::Conflict)
    ));
    let next = db.takeover(&attempt.run).await.attempt;
    assert!(matches!(
        dispatch(attempt, "agenda_list").await,
        Err(Error::Stale)
    ));
    assert_eq!(dispatch(next, "agenda_list").await.unwrap(), logical);
}

#[tokio::test]
async fn point_read_returns_only_the_named_step_of_the_claimed_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = leased(&db, "first").await.attempt;
    let second = leased(&db, "second").await.attempt;
    let key = format!("{USE_PREFIX}u1");
    let entry = |label: &str| Step {
        key: key.clone(),
        body: JsonPayload(json!({"name":label}).to_string()),
    };
    runs(&db)
        .checkpoint_batch(
            first.clone(),
            vec![
                Step {
                    key: format!("{RECORD_PREFIX}0"),
                    body: JsonPayload(json!("x".repeat(880 * 1024)).to_string()),
                },
                entry("first"),
            ],
        )
        .await
        .unwrap();
    runs(&db)
        .checkpoint(second.clone(), entry("second"))
        .await
        .unwrap();
    let audits = db.scalar::<i64>("select count(*) from bot_audit").await;
    for (attempt, expected) in [(first.clone(), "first"), (second, "second")] {
        assert_eq!(
            calls(&db).step(attempt, key.clone()).await.unwrap(),
            Some(entry(expected).body)
        );
    }
    assert_eq!(
        calls(&db)
            .step(first.clone(), "unknown".into())
            .await
            .unwrap(),
        None
    );
    let mut cross_profile = first.clone();
    cross_profile.profile = ProfileId("work".into());
    assert!(matches!(
        calls(&db).step(cross_profile, key.clone()).await,
        Err(Error::Stale)
    ));
    let old_host = db.store.for_activation(Revision("superseded-set".into()));
    assert!(matches!(old_host.step(first, key).await, Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        audits
    );
}

#[tokio::test]
async fn point_read_fences_private_cancelled_expired_and_replaced_claims() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = leased(&db, "owner").await.attempt;
    let group = turn().pop().unwrap();
    runs(&db)
        .checkpoint(first.clone(), group.clone())
        .await
        .unwrap();
    let read = |at| calls(&db).step(at, group.key.clone());
    let mut no_claim = first.clone();
    no_claim.claim = None;
    assert!(matches!(read(no_claim).await, Err(Error::Stale)));
    db.expire_lease(&first.run).await;
    assert!(matches!(read(first.clone()).await, Err(Error::Stale)));
    let second = runs(&db).claim().await.unwrap().unwrap().attempt;
    assert!(matches!(read(first).await, Err(Error::Stale)));
    assert!(read(second.clone()).await.unwrap().is_some());
    db.sql("update bot_runs set cancel_requested = true").await;
    assert!(matches!(read(second).await, Err(Error::Stale)));
    let private = support::private::reserve(&db, "private", 1).await;
    assert!(matches!(read(private).await, Err(Error::Denied)));
}

#[tokio::test]
async fn a_conflicting_or_stale_writer_cannot_leave_part_of_a_cli_turn() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "checkpoint").await;
    let batch = turn();
    let original = batch[1].clone();
    runs(&db)
        .checkpoint(run.attempt.clone(), original.clone())
        .await
        .unwrap();
    let mut conflicting = batch.clone();
    conflicting[1].body = JsonPayload("null".into());
    assert!(matches!(
        runs(&db)
            .checkpoint_batch(run.attempt.clone(), conflicting)
            .await,
        Err(Error::Conflict)
    ));
    let after = calls(&db).context(run.attempt.clone()).await.unwrap();
    assert_eq!(after.work.steps.len(), 1, "첫 rec까지 롤백해야 한다");
    assert_eq!(after.work.steps[0].key, original.key);

    let next = db.takeover(&run.attempt.run).await;
    assert!(matches!(
        runs(&db).checkpoint_batch(run.attempt, batch.clone()).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        calls(&db)
            .context(next.attempt.clone())
            .await
            .unwrap()
            .work
            .steps
            .len(),
        1
    );
    for _ in 0..2 {
        runs(&db)
            .checkpoint_batch(next.attempt.clone(), batch.clone())
            .await
            .unwrap();
    }
    let saved = calls(&db).context(next.attempt).await.unwrap().work.steps;
    assert_eq!(saved.len(), batch.len());
    for step in &batch {
        let found = saved.iter().find(|s| s.key == step.key).expect("모든 key");
        let value = |s: &Step| serde_json::from_str::<serde_json::Value>(&s.body.0).unwrap();
        assert_eq!(value(found), value(step));
    }
}
