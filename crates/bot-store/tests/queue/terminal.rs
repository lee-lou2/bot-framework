use super::*;
use bot_core::ledger::{FinishCall, Outcome};

async fn cancel(db: &TestDb, at: &Attempt) {
    db.store
        .cancel(
            at.profile.clone(),
            at.run.clone(),
            PrincipalId("operator".into()),
            "중지".into(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_committed_cancel_wins_before_finish_and_creates_no_reply() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = leased_in(&db, "final", ConversationKind::Direct)
        .await
        .attempt;
    begin_call(&db, &at, "c1").await;
    calls(&db).start(start_call(&at, "c1", None)).await.unwrap();
    cancel(&db, &at).await;
    calls(&db)
        .finish(FinishCall {
            attempt: at.clone(),
            call: call("c1"),
            outcome: Outcome::Done(JsonPayload(r#"{"receipt":"one"}"#.into())),
        })
        .await
        .unwrap();
    let end = db
        .store
        .finish_with(
            at.clone(),
            done("늦은 답"),
            RunOutcome {
                truncated: true,
                usage: Some(TokenUsage {
                    prompt: 3,
                    completion: 2,
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(end, Completion::Cancelled);
    assert_eq!(state_of_run(&db, &at.run).await, "cancelled");
    assert!(
        db.scalar::<bool>(
            "select result is null and truncated is null and usage_prompt is null from bot_runs"
        )
        .await
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);
    assert_eq!(
        db.scalar::<String>("select result from bot_calls").await,
        r#"{"receipt":"one"}"#
    );
    assert!(stale(db.store.finish(at, done("재전달")).await));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.cancelled'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_finish_committed_first_keeps_its_reply_and_cancelled_child_wakes_its_parent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = leased_in(&db, "finish-first", ConversationKind::Direct)
        .await
        .attempt;
    assert_eq!(
        db.store.finish(first.clone(), done("완료")).await.unwrap(),
        Completion::Succeeded
    );
    cancel(&db, &first).await;
    assert_eq!(state_of_run(&db, &first.run).await, "succeeded");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 1);

    let parent = leased(&db, "parent").await.attempt;
    db.store
        .delegate(Delegate {
            parent: parent.clone(),
            step: "child".into(),
            child: AgentId("main/a".into()),
            instruction: "조사".into(),
            scopes: scopes(&["run"]),
            checked_revision: Revision("r1".into()),
        })
        .await
        .unwrap();
    let child = db.store.claim().await.unwrap().unwrap().attempt;
    cancel(&db, &child).await;
    db.store
        .finish(child.clone(), done("늦은 자식 답"))
        .await
        .unwrap();
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, parent.run);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&resumed.work.steps[0].body.0).unwrap()["state"],
        "cancelled"
    );
    assert!(stale(db.store.finish(child, done("다시")).await));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
}
