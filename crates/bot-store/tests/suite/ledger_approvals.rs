use crate::support::*;
use bot_core::{command::*, id::*, ledger::*, port::*, state::*};
use std::time::Duration;

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

async fn phase(db: &TestDb, at: &Attempt, id: &str) -> CallPhase {
    calls(db)
        .begin(at.clone(), call(id), tool("web.search"), [1; 32])
        .await
        .unwrap()
        .phase
}

#[tokio::test]
async fn opening_a_new_approval_supersedes_the_old_one_and_decisions_wake_the_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    assert_eq!(
        state_of_run(&db, &run.attempt.run).await,
        "waiting",
        "opening an approval parks the run"
    );
    assert_eq!(
        db.scalar::<String>("select wait_kind from bot_runs").await,
        "approval"
    );
    assert!(
        matches!(
            runs(&db).renew(run.attempt.clone()).await,
            Err(Error::Stale)
        ),
        "worker lease must be released"
    );

    db.sql("update bot_runs set state = 'ready', wait_kind = null, keep_generation = true")
        .await;
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &resumed.attempt);
    calls(&db)
        .open_approval(open_approval(binding(&resumed.attempt, "c1", "n2")))
        .await
        .unwrap();
    let old = calls(&db).approval(ApprovalId("n1".into())).await.unwrap();
    assert_eq!(old.state, ApprovalState::Superseded);
    assert_eq!(
        db.scalar::<String>("select approval from bot_calls").await,
        "n2",
        "call must point to the latest approval"
    );
    assert!(
        matches!(
            calls(&db).decide(decision("n1", true)).await,
            Err(Error::Conflict)
        ),
        "superseded approval cannot be decided"
    );

    let stranger = Decision {
        actor: PrincipalId("local:guest".into()),
        ..decision("n2", true)
    };
    assert!(matches!(
        calls(&db).decide(stranger).await,
        Err(Error::Denied)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(
        calls(&db).decide(decision("n2", true)).await.unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    assert!(matches!(
        calls(&db).decide(decision("n2", true)).await,
        Err(Error::Conflict)
    ));
    let woken = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &woken.attempt);
    assert_eq!(
        db.scalar::<i32>("select attempts from bot_runs").await,
        0,
        "approval wait must not increment retry attempts"
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn denial_and_revocation_close_the_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    assert_eq!(
        calls(&db).decide(decision("n1", false)).await.unwrap(),
        ApprovalState::Denied
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let start = start_call(
        &resumed.attempt,
        "c1",
        Some((
            binding(&resumed.attempt, "c1", "n1"),
            vec!["irreversible".into()],
        )),
    );
    assert!(
        matches!(calls(&db).start(start).await, Err(Error::Denied)),
        "denied approval cannot start a call"
    );

    // Denying an already-approved call before start revokes it.
    calls(&db)
        .open_approval(open_approval(binding(&resumed.attempt, "c1", "n2")))
        .await
        .unwrap();
    calls(&db).decide(decision("n2", true)).await.unwrap();
    assert_eq!(
        calls(&db).decide(decision("n2", false)).await.unwrap(),
        ApprovalState::Revoked
    );
    let again = runs(&db).claim().await.unwrap().unwrap();
    let start = start_call(
        &again.attempt,
        "c1",
        Some((
            binding(&again.attempt, "c1", "n2"),
            vec!["irreversible".into()],
        )),
    );
    assert!(matches!(calls(&db).start(start).await, Err(Error::Denied)));
}

#[tokio::test]
async fn start_consumes_an_approval_once_with_the_same_binding() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db).decide(decision("n1", true)).await.unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let reasons = vec!["irreversible".to_owned()];

    // Mismatched args, policy revision, or reasons fail without consuming the approval.
    let mut changed_args = binding(&resumed.attempt, "c1", "n1");
    changed_args.args_sha256 = [9; 32];
    let mut changed_policy = binding(&resumed.attempt, "c1", "n1");
    changed_policy.policy_revision = Revision("p2".into());
    for (expected, why) in [
        (changed_args, reasons.clone()),
        (changed_policy, reasons.clone()),
        (binding(&resumed.attempt, "c1", "n1"), vec!["policy".into()]),
    ] {
        let result = calls(&db)
            .start(start_call(&resumed.attempt, "c1", Some((expected, why))))
            .await;
        assert!(matches!(result, Err(Error::Stale)));
    }
    assert!(matches!(
        phase(&db, &resumed.attempt, "c1").await,
        CallPhase::Open { .. }
    ));
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Approved
    );

    calls(&db)
        .start(start_call(
            &resumed.attempt,
            "c1",
            Some((binding(&resumed.attempt, "c1", "n1"), reasons.clone())),
        ))
        .await
        .unwrap();
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Consumed
    );
    assert!(matches!(
        phase(&db, &resumed.attempt, "c1").await,
        CallPhase::Started { .. }
    ));

    begin_call(&db, &resumed.attempt, "c2").await;
    let other_call = start_call(
        &resumed.attempt,
        "c2",
        Some((binding(&resumed.attempt, "c2", "n1"), reasons.clone())),
    );
    assert!(matches!(
        calls(&db).start(other_call).await,
        Err(Error::Denied)
    ));
    let reuse = FinishCall {
        attempt: resumed.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Released,
    };
    calls(&db).finish(reuse).await.unwrap();
    let again = start_call(
        &resumed.attempt,
        "c1",
        Some((binding(&resumed.attempt, "c1", "n1"), reasons)),
    );
    assert!(
        matches!(calls(&db).start(again).await, Err(Error::Conflict)),
        "consumed approval cannot be reused"
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_crash_handover_invalidates_old_approvals_and_buttons() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db).decide(decision("n1", true)).await.unwrap();
    // Crash takeover bumps generation and supersedes prior-generation approvals.
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.generation, Generation(1));
    let new = db.takeover(&run.attempt.run).await;
    assert_eq!(new.attempt.generation, Generation(2));
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Superseded
    );
    assert!(
        matches!(
            calls(&db).decide(decision("n1", false)).await,
            Err(Error::Stale)
        ),
        "stale approval button must be rejected"
    );
    let start = start_call(
        &new.attempt,
        "c1",
        Some((
            binding(&new.attempt, "c1", "n1"),
            vec!["irreversible".into()],
        )),
    );
    assert!(matches!(calls(&db).start(start).await, Err(Error::Stale)));

    calls(&db)
        .open_approval(open_approval(binding(&new.attempt, "c1", "n2")))
        .await
        .unwrap();
    calls(&db).decide(decision("n2", true)).await.unwrap();
    let after = runs(&db).claim().await.unwrap().unwrap();
    assert_eq!(after.attempt.generation, Generation(2));
    let ok = start_call(
        &after.attempt,
        "c1",
        Some((
            binding(&after.attempt, "c1", "n2"),
            vec!["irreversible".into()],
        )),
    );
    calls(&db).start(ok).await.unwrap();
}

#[tokio::test]
async fn expired_approvals_are_closed_and_wake_the_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    let mut short = binding(&run.attempt, "c1", "n1");
    short.expires_at = std::time::SystemTime::now() + Duration::from_secs(1);
    calls(&db)
        .open_approval(open_approval(short))
        .await
        .unwrap();
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    assert!(
        matches!(
            calls(&db).decide(decision("n1", true)).await,
            Err(Error::Stale)
        ),
        "decision after expiry must be rejected"
    );
    // Claim sweeps expired approvals and wakes the waiting run.
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    assert_resumed(&run.attempt, &resumed.attempt);
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Expired
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'approval.expire'")
            .await,
        1
    );
}

#[tokio::test]
async fn supersede_closes_the_current_approval_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    let stranger = PrincipalId("local:guest".into());
    assert!(matches!(
        calls(&db)
            .supersede(ApprovalId("n1".into()), stranger)
            .await,
        Err(Error::Denied)
    ));
    calls(&db)
        .supersede(ApprovalId("n1".into()), owner())
        .await
        .unwrap();
    assert_eq!(
        state_of_run(&db, &run.attempt.run).await,
        "ready",
        "supersede must wake the run"
    );
    calls(&db)
        .supersede(ApprovalId("n1".into()), owner())
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'approval.supersede'")
            .await,
        1,
        "repeated supersede must be idempotent"
    );

    let resumed = runs(&db).claim().await.unwrap().unwrap();
    calls(&db)
        .open_approval(open_approval(binding(&resumed.attempt, "c1", "n2")))
        .await
        .unwrap();
    calls(&db).decide(decision("n2", true)).await.unwrap();
    let again = runs(&db).claim().await.unwrap().unwrap();
    let start = start_call(
        &again.attempt,
        "c1",
        Some((
            binding(&again.attempt, "c1", "n2"),
            vec!["irreversible".into()],
        )),
    );
    calls(&db).start(start).await.unwrap();
    assert!(matches!(
        calls(&db).supersede(ApprovalId("n2".into()), owner()).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn approval_cards_are_queued_for_push_conversations_only() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let dm = leased_in(&db, "dm", ConversationKind::Direct).await;
    begin_call(&db, &dm.attempt, "c1").await;
    let mut bad = open_approval(binding(&dm.attempt, "c1", "n1"));
    bad.card = JsonPayload("{}".into());
    assert!(
        matches!(calls(&db).open_approval(bad).await, Err(Error::Invalid)),
        "push conversation requires valid broker card payload"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0,
        "rejected open must leave no row"
    );
    calls(&db)
        .open_approval(open_approval(binding(&dm.attempt, "c1", "n1")))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_outbox where state = 'pending' and conv_kind = 'direct'"
        )
        .await,
        1
    );
    let body = db.scalar::<String>("select body from bot_outbox").await;
    assert!(body.contains(r#""nonce":"n1""#));

    let cli = leased(&db, "cli").await;
    begin_call(&db, &cli.attempt, "c1").await;
    let mut card = open_approval(binding(&cli.attempt, "c1", "n2"));
    card.card = JsonPayload("{}".into());
    calls(&db).open_approval(card).await.unwrap();
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn revocation_and_start_are_serialized() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db).decide(decision("n1", true)).await.unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let (revoker, starter) = (db.peer("a").await, db.peer("b").await);
    let attempt = resumed.attempt.clone();
    let (left, right) = tokio::join!(
        tokio::spawn(async move { CallLedger::decide(&revoker, decision("n1", false)).await }),
        tokio::spawn(async move {
            let start = start_call(
                &attempt,
                "c1",
                Some((binding(&attempt, "c1", "n1"), vec!["irreversible".into()])),
            );
            CallLedger::start(&starter, start).await
        }),
    );
    let (revoked, started) = (left.unwrap(), right.unwrap());
    let approval = calls(&db)
        .approval(ApprovalId("n1".into()))
        .await
        .unwrap()
        .state;
    match (revoked.is_ok(), started.is_ok()) {
        (true, false) => assert_eq!(approval, ApprovalState::Revoked),
        (false, true) => assert_eq!(approval, ApprovalState::Consumed),
        other => panic!("exactly one side must win: {other:?} {approval:?}"),
    }
}

#[tokio::test]
async fn a_call_pointing_at_a_live_approval_cannot_start_without_presenting_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .open_approval(open_approval(binding(&run.attempt, "c1", "n1")))
        .await
        .unwrap();
    calls(&db).decide(decision("n1", true)).await.unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap();
    let result = calls(&db)
        .start(start_call(&resumed.attempt, "c1", None))
        .await;
    assert!(matches!(result, Err(Error::Denied)));
    assert!(matches!(
        phase(&db, &resumed.attempt, "c1").await,
        CallPhase::Open { .. }
    ));
    assert_eq!(
        calls(&db)
            .approval(ApprovalId("n1".into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Approved
    );
    // Once superseded (e.g., policy changed), the call may start without presenting the approval.
    calls(&db)
        .supersede(ApprovalId("n1".into()), PrincipalId("local:owner".into()))
        .await
        .unwrap();
    calls(&db)
        .start(start_call(&resumed.attempt, "c1", None))
        .await
        .unwrap();
}
