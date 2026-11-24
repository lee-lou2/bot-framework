mod support;

use bot_core::{command::*, id::*, ledger::*, port::*, state::*};
use std::time::Duration;
use support::*;

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
        "승인 요청은 run을 대기로 넘긴다"
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
        "워커 리스를 놓았다"
    );

    // 새 조건으로 다시 열면 이전 승인은 대체된다(여기서는 같은 세대로 재개한 뒤)
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
        "호출은 가장 최근 승인을 가리킨다"
    );
    assert!(
        matches!(
            calls(&db).decide(decision("n1", true)).await,
            Err(Error::Conflict)
        ),
        "옛 승인은 결정할 수 없다"
    );

    // 승인자 본인만 결정하고, 결정하면 기다리는 run이 같은 세대로 깨어난다
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
        "승인 대기는 시도 횟수를 쓰지 않는다"
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
        "거절된 승인으로는 시작할 수 없다"
    );

    // 승인한 뒤 실행 전의 거절은 철회다
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

    // 인자·정책·이유가 달라졌으면 소비하지 못하고, 승인은 그대로 남는다
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

    // 소비한 승인은 다시 쓸 수 없고, 다른 호출의 승인으로도 시작할 수 없다
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
        "소비한 승인은 다시 쓸 수 없다"
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
    // 승인된 뒤 재개한 워커가 시작하기 전에 죽었다: 인계하면 세대가 오르고 옛 승인은 무효다
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
        "옛 버튼은 거절"
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

    // 새 승인은 새 세대로 한 번 열고 한 번 소비한다
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
        "만료 뒤 결정은 거절"
    );
    let resumed = runs(&db).claim().await.unwrap().unwrap(); // claim이 만료를 정리하고 깨운다
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
        "대기를 푼다"
    );
    calls(&db)
        .supersede(ApprovalId("n1".into()), owner())
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'approval.supersede'")
            .await,
        1,
        "재호출은 새 감사·깨움이 없다"
    );

    // 소비한 승인은 대체할 수 없다
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
    // Slack 대화: 카드가 승인과 같은 커밋으로 회신 행에 쌓인다
    let dm = leased_in(&db, "dm", ConversationKind::Direct).await;
    begin_call(&db, &dm.attempt, "c1").await;
    let mut bad = open_approval(binding(&dm.attempt, "c1", "n1"));
    bad.card = JsonPayload("{}".into());
    assert!(
        matches!(calls(&db).open_approval(bad).await, Err(Error::Invalid)),
        "브로커의 카드 형식이 아니면 받지 않는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0,
        "거절되면 아무것도 남지 않는다"
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

    // CLI 대화: 카드는 승인 조회로 보고 회신 행은 없다
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
        other => panic!("정확히 한쪽만 이겨야 한다: {other:?} {approval:?}"),
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
    // 승인이 연결돼 있는데 내지 않으면(인자를 비워 소비를 건너뛰면) 시작하지 못하고 승인은 그대로다
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
    // 정책이 바뀌어 승인이 대체되면 더는 필요하지 않다: 승인 없이 시작한다
    calls(&db)
        .supersede(ApprovalId("n1".into()), PrincipalId("local:owner".into()))
        .await
        .unwrap();
    calls(&db)
        .start(start_call(&resumed.attempt, "c1", None))
        .await
        .unwrap();
}
