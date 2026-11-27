use crate::support::*;
use bot_core::{command::*, id::*, ledger::*, port::*};

fn owner() -> PrincipalId {
    PrincipalId("local:owner".into())
}

fn decision(nonce: &str, allow: bool) -> Decision {
    Decision {
        nonce: ApprovalId(nonce.into()),
        actor: owner(),
        allow,
    }
}

#[tokio::test]
async fn a_resumed_run_rejects_the_previous_claim_even_with_the_same_generation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let old = leased(&db, "r").await;
    begin_call(&db, &old.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&old.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db).decide(decision("n1", true)).await.unwrap();
    let new = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&old.attempt, &new.attempt);

    // After wait/resume, the old claim token is stale even though the generation is unchanged.
    let stale = |r: Result<(), Error>| matches!(r, Err(Error::Stale));
    let step = Step {
        key: "k".into(),
        body: JsonPayload("{}".into()),
    };
    assert!(stale(
        runs(&db).renew(old.attempt.clone()).await.map(|_| ())
    ));
    assert!(stale(runs(&db).checkpoint(old.attempt.clone(), step).await));
    assert!(stale(
        runs(&db).history(old.attempt.clone(), 5).await.map(|_| ())
    ));
    assert!(stale(
        runs(&db)
            .finish(old.attempt.clone(), done("late"))
            .await
            .map(|_| ())
    ));
    assert!(stale(
        runs(&db)
            .fail(old.attempt.clone(), Failure::Permanent("x"))
            .await
            .map(|_| ())
    ));
    assert!(stale(
        calls(&db).context(old.attempt.clone()).await.map(|_| ())
    ));
    assert!(stale(
        calls(&db)
            .begin(old.attempt.clone(), call("c2"), tool("web.search"), [1; 32])
            .await
            .map(|_| ())
    ));
    assert!(stale(
        calls(&db).start(start_call(&old.attempt, "c1", None)).await
    ));

    let anonymous = Attempt {
        claim: None,
        ..new.attempt.clone()
    };
    assert!(stale(runs(&db).renew(anonymous.clone()).await.map(|_| ())));
    assert!(stale(calls(&db).context(anonymous).await.map(|_| ())));

    runs(&db).renew(new.attempt.clone()).await.unwrap();
    calls(&db).context(new.attempt.clone()).await.unwrap();
}

#[tokio::test]
async fn approval_context_reads_a_parked_run_for_the_approver_without_a_token() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");

    let context = calls(&db)
        .approval_context(ApprovalId("n1".into()), owner())
        .await
        .unwrap();
    assert_eq!(
        (context.principal.0.as_str(), context.agent.0.as_str()),
        ("local:owner", "main")
    );
    assert_eq!(
        (
            &context.attempt.run,
            context.attempt.generation,
            context.attempt.claim
        ),
        (&run.attempt.run, run.attempt.generation, None)
    );
    let stranger = PrincipalId("local:guest".into());
    assert!(matches!(
        calls(&db)
            .approval_context(ApprovalId("n1".into()), stranger)
            .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        calls(&db)
            .approval_context(ApprovalId("nope".into()), owner())
            .await,
        Err(Error::Missing)
    ));

    let stored = calls(&db).approval(ApprovalId("n1".into())).await.unwrap();
    assert_eq!(stored.approval.attempt.claim, None);

    db.store
        .cancel(
            profile_id(),
            run.attempt.run.clone(),
            owner(),
            "stop".into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        calls(&db)
            .approval_context(ApprovalId("n1".into()), owner())
            .await,
        Err(Error::Stale)
    ));
}
