//! 소유자 DM 승인 카드 전달 작업(PG): 채널 출처의 «명시 목적지 생성»·«전송 초안» 승인은 공개 채널에 올리지 않고 소유자 DM으로 전달한다.
//! 카드는 게시를 확인한 영수증(팀·DM 채널·메시지 ts)이 한 커밋에서 열 때까지 닫혀 있고(결정·관리 목록·클릭 대상 아님), 그 커밋에서만 원래 채널·스레드에
//! «DM으로 승인 요청을 보냈어요» 한 줄이 등록된다. 전달 실패 쪽은 `card_failure.rs`.

mod support;

use bot_core::{admin::AdminRead, id::*, ledger::*, port::*, state::*};
use std::collections::BTreeSet;
use support::{admin::*, cards::*, delivery::*, *};

#[tokio::test]
async fn a_channel_card_waits_closed_and_stays_off_the_public_channel() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    // 공개 채널에는 카드도 관리 화면 안내도 가지 않는다
    assert_eq!(public_lines(&db, &run).await, Vec::<String>::new());
    assert_eq!(
        (
            state(&db, "bot_approvals", "n1").await,
            state(&db, "bot_card_jobs", "n1").await
        ),
        ("pending".to_owned(), "staged".to_owned())
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    // 작업은 승인 식별자·소유자·원래 프로필·바인딩·세대를 묶는다
    assert_eq!(
        db.scalar::<String>("select profile || '|' || binding || '|' || owner || '|' || generation from bot_card_jobs")
            .await,
        format!("personal|desk|{OWNER}|{}", run.attempt.generation.0)
    );
    // 열리지 않은 카드는 누르거나 API로 결정해도 승인을 소비하지 않는다
    let got = decide_as_owner(&db, &approval).await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    assert_eq!(state(&db, "bot_approvals", "n1").await, "pending");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    // 관리 목록과 대기 수에서도 빠진다
    assert_eq!(owner_pending(&db).await, Vec::<String>::new());
    let status = AdminRead::status(&db.store, &operator()).await.unwrap();
    assert!(
        status.iter().all(|p| p.approvals_pending == 0),
        "{status:?}"
    );
}

#[tokio::test]
async fn a_direct_conversation_and_a_plain_channel_approval_are_unchanged() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 채널 출처여도 수신자를 지정하지 않은 일반 승인은 예전처럼 채널에 카드를 올린다
    let channel = run_in(&db, "r2", OWNER, ConversationKind::Channel).await;
    begin_call(&db, &channel.attempt, "d1").await;
    let plain = Approval {
        tool: tool("web.search"),
        ..binding(&channel.attempt, "d1", "n2")
    };
    calls(&db)
        .open_approval(open_approval(plain))
        .await
        .unwrap();
    assert_eq!(public_lines(&db, &channel).await.len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    // 1:1 대화의 전송 카드는 그 자리에 카드로 간다(전달 작업 없음)
    let direct = destination_run_as(&db, "r1", ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &direct, answers(&["하나"])).await;
    assert_eq!(public_lines(&db, &direct).await.len(), 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert!(decide_as_owner(&db, &approval).await.is_ok());
}

#[tokio::test]
async fn a_confirmed_receipt_opens_the_card_and_adds_exactly_one_public_line() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let jobs = claim(&db.store, 8).await.unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(
        (
            job.nonce.0.as_str(),
            job.owner.0.as_str(),
            job.profile.0.as_str(),
            job.binding.0.as_str()
        ),
        ("n1", OWNER, "personal", "desk")
    );
    // 카드는 승인 문서와 같은 바이트의 `{nonce,title,lines}`다
    let card: serde_json::Value = serde_json::from_str(&job.card.0).unwrap();
    assert_eq!(
        (card["nonce"].as_str(), card["title"].as_str()),
        (Some("n1"), Some("예약 회신 승인"))
    );
    // 점유한 작업은 리스 동안 다시 점유되지 않는다. begin 없이 한 게시 확인은 받지 않는다. 최초 전송자만 begin이 통과한다
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    let early = settle(&db.store, &job.nonce, posted()).await;
    assert!(matches!(early, Err(Error::Conflict)), "{early:?}");
    begin(&db.store, &job.nonce, rev()).await.unwrap();
    let again = begin(&db.store, &job.nonce, rev()).await;
    assert!(matches!(again, Err(Error::Conflict)), "{again:?}");
    // 올리는 중에는 아직 열리지 않았다. begin 없이 한 정산은 받지 않는다(점유만 하고 올린 적이 없다)
    assert!(matches!(
        decide_as_owner(&db, &approval).await,
        Err(Error::Conflict)
    ));
    assert_eq!(receipt(&db, &job.nonce).await, None);
    assert!(public_lines(&db, &run).await.is_empty());

    settle(&db.store, &job.nonce, posted()).await.unwrap();
    // 영수증(클릭 대조용)과 공개 한 줄: 초안·수신자·해시·버튼·승인 식별자는 없다
    let receipt = receipt(&db, &job.nonce).await.expect("영수증");
    assert_eq!(
        (
            receipt.team.as_str(),
            receipt.dm.as_str(),
            receipt.ts.as_str(),
            receipt.owner.0.as_str(),
            receipt.profile.0.as_str(),
            receipt.binding.0.as_str()
        ),
        ("T1", "D77", "1700000000.000100", OWNER, "personal", "desk")
    );
    assert_eq!(
        public_lines(&db, &run).await,
        vec![r#"{"text":"DM으로 승인 요청을 보냈어요"}"#.to_owned()]
    );
    // 같은 정산의 재전달은 같은 결과이고 줄이 늘지 않는다. 다른 영수증은 충돌이다
    settle(&db.store, &job.nonce, posted()).await.unwrap();
    assert_eq!(public_lines(&db, &run).await.len(), 1);
    let other = bot_core::cards::CardOutcome::Posted {
        team: "T1".into(),
        dm: "D77".into(),
        ts: "9.9".into(),
    };
    assert!(matches!(
        settle(&db.store, &job.nonce, other).await,
        Err(Error::Conflict)
    ));
    // 열린 카드는 늦게 온 «전달 실패»로 닫지 않는다
    for late in [
        bot_core::cards::CardOutcome::Unavailable,
        bot_core::cards::CardOutcome::Held,
    ] {
        let got = settle(&db.store, &job.nonce, late).await;
        assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    }
    // 이제 열렸다: 관리 목록에 보이고 소유자가 결정한다. run은 같은 세대로 깨어난다
    assert_eq!(owner_pending(&db).await, vec!["n1".to_owned()]);
    assert_eq!(
        decide_as_owner(&db, &approval).await.unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    // 닫힌 작업이 아니라 열린 작업은 다시 점유되지 않는다
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
}

#[tokio::test]
async fn concurrent_claimers_never_share_a_job_and_only_one_begin_passes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for n in 1..=4 {
        channel_card(&db, &format!("r{n}"), &format!("n{n}")).await;
        // 다음 run을 접수·점유하려면 앞 run이 대기 중이어야 한다(카드를 열면 그렇다)
    }
    let (a, b) = (db.peer("a").await, db.peer("b").await);
    let (first, second) = tokio::join!(claim(&a, 3), claim(&b, 3));
    let (first, second) = (first.unwrap(), second.unwrap());
    let seen: BTreeSet<String> = first
        .iter()
        .chain(&second)
        .map(|j| j.nonce.0.clone())
        .collect();
    assert_eq!(
        first.len() + second.len(),
        seen.len(),
        "같은 작업을 둘이 잡았다"
    );
    assert_eq!(seen.len(), 4, "점유한 작업은 한 번씩이다: {seen:?}");
    // 같은 작업에 대한 begin은 한 번만 통과한다
    let nonce = first[0].nonce.clone();
    let (x, y) = tokio::join!(begin(&a, &nonce, rev()), begin(&b, &nonce, rev()));
    assert_eq!(
        [x.is_ok(), y.is_ok()].iter().filter(|ok| **ok).count(),
        1,
        "{x:?} {y:?}"
    );
}

#[tokio::test]
async fn a_card_that_went_stale_while_posting_is_never_opened() {
    // 활성 집합이 바뀌었다(옛 경로의 카드): 올라간 DM 메시지는 남지만 영수증이 없어 카드는 열리지 않고 공개 안내도 없다
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    let got = settle(&db.store, &job.nonce, posted()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert!(public_lines(&db, &run).await.is_empty());
    assert_eq!(receipt(&db, &job.nonce).await, None);
    assert!(matches!(
        decide_as_owner(&db, &approval).await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn an_expired_or_replaced_card_is_never_opened_and_begin_checks_the_profile_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 판본이 다르면(게이트웨이가 본 프로필이 낡았다) begin이 거절하고 작업은 그대로다
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let jobs = claim(&db.store, 8).await.unwrap();
    let got = begin(&db.store, &jobs[0].nonce, Revision("r9".into())).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "leased");
    begin(&db.store, &jobs[0].nonce, rev()).await.unwrap();
    // 올리는 사이 카드가 만료됐다: 열지 않는다
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let got = settle(&db.store, &jobs[0].nonce, posted()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
}

#[tokio::test]
async fn claim_closes_cards_that_are_no_longer_pending_and_skips_them() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 0);
}

#[tokio::test]
async fn a_channel_card_whose_approver_is_not_the_requester_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Channel).await;
    let (approval, open) = create_card(&db, &run, "n1").await;
    let other = Approval {
        approver: PrincipalId(GUEST.into()),
        ..approval
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let got = CallLedger::open_approval(
        &pinned,
        bot_core::ledger::OpenApproval {
            approval: other,
            ..open
        },
    )
    .await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_card_jobs").await,
        0
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
}

#[tokio::test]
async fn begin_refuses_a_card_that_expired_after_the_claim() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let job = claim(&db.store, 8).await.unwrap().remove(0);
    // 점유한 뒤 카드가 만료됐다: 올리면 안 된다(작업은 점유 상태 그대로, 정리는 다음 점유가 한다)
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    let got = begin(&db.store, &job.nonce, rev()).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "leased");
    // 승인이 없는 작업은 알 수 없다
    let none = begin(&db.store, &ApprovalId("nope".into()), rev()).await;
    assert!(matches!(none, Err(Error::Missing)), "{none:?}");
}
