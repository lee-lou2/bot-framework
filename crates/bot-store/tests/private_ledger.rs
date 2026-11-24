//! 민감 DM 전용 원장(PrivateLedger)의 상태 기계(PG): 예약 → 시작 → 완료·거절·보류, 반납과 재점유,
//! 리스가 끝난 접수는 보류, 일반 큐·호출·효과·기억·위임 경로는 private 토큰을 거절한다.

mod support;

use bot_core::{
    command::*, id::*, ledger::*, port::*, profile::ActiveRevisions, state::ConversationKind,
};
use std::collections::BTreeMap;
use support::{private::*, *};

#[tokio::test]
async fn a_reservation_is_a_private_run_that_workers_never_pick() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = reserve(&db, "r1", 1).await;
    assert_eq!((at.generation.0, epoch(&at)), (1, 1));
    assert_eq!(row(&db).await, "leased:-:1:1:false");
    assert!(
        runs(&db).claim().await.unwrap().is_none(),
        "워커는 집지 않는다"
    );
    assert!(
        runs(&db).history(at.clone(), 5).await.is_err(),
        "이력에도 열리지 않는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.begin'")
            .await,
        1
    );
    // 같은 키의 재전달은 점유 중이다(새 효과 없음)
    assert!(matches!(
        ledger(&db).begin(request("r1", 1)).await.unwrap(),
        PrivatePermit::InProgress
    ));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
}

#[tokio::test]
async fn only_a_new_key_is_checked_against_the_active_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut stale = request("r2", 2);
    stale.revision = Revision("r0".into());
    assert!(matches!(ledger(&db).begin(stale).await, Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0,
        "어긋난 새 접수는 행도 남기지 않는다"
    );

    let at = reserve(&db, "r1", 1).await;
    // 활성 판본이 바뀐 뒤의 재전달은 새 접수가 아니므로 검사하지 않는다
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(profile_id(), Revision("r2".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        ledger(&db).begin(request("r1", 1)).await.unwrap(),
        PrivatePermit::InProgress
    ));
    // 그러나 전송 직전의 시작은 지금 활성 판본과 대조한다
    assert!(matches!(
        ledger(&db).start(start_of(&at, 1)).await,
        Err(Error::Stale)
    ));
    assert_eq!(row(&db).await, "leased:-:1:1:false");
}

#[tokio::test]
async fn start_checks_lease_epoch_tool_fingerprint_and_revision_then_commits_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = reserve(&db, "r1", 1).await;
    let mut wrong_tool = start_of(&at, 1);
    wrong_tool.tool = ToolId("inbox.delete".into());
    assert!(matches!(
        ledger(&db).start(wrong_tool).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        ledger(&db).start(start_of(&at, 9)).await,
        Err(Error::Denied)
    ));
    let mut other_epoch = start_of(&at, 1);
    other_epoch.attempt.claim = Some(ClaimEpoch(9));
    assert!(matches!(
        ledger(&db).start(other_epoch).await,
        Err(Error::Stale)
    ));
    let mut no_claim = start_of(&at, 1);
    no_claim.attempt.claim = None;
    assert!(matches!(
        ledger(&db).start(no_claim).await,
        Err(Error::Stale)
    ));
    let mut other_revision = start_of(&at, 1);
    other_revision.checked_revision = Revision("r2".into());
    assert!(matches!(
        ledger(&db).start(other_revision).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        row(&db).await,
        "leased:-:1:1:false",
        "거절은 아무것도 남기지 않는다"
    );

    ledger(&db).start(start_of(&at, 1)).await.unwrap();
    assert_eq!(row(&db).await, "leased:-:1:1:true");
    assert!(matches!(
        ledger(&db).start(start_of(&at, 1)).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.start'")
            .await,
        1
    );

    // 리스가 끝난 뒤에는 시작할 수 없다
    let late = reserve(&db, "r2", 2).await;
    db.expire_lease(&late.run).await;
    assert!(matches!(
        ledger(&db).start(start_of(&late, 2)).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn start_is_refused_when_the_profile_is_not_in_the_active_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = reserve(&db, "r1", 1).await;
    db.store
        .replace(
            Some(Revision("set-1".into())),
            Revision("set-2".into()),
            BTreeMap::from([(ProfileId("work".into()), Revision("r1".into()))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    assert!(matches!(
        ledger(&db).start(start_of(&at, 1)).await,
        Err(Error::Stale)
    ));
}

#[tokio::test]
async fn finish_follows_the_transition_table() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 시작 전: 반납·거절만 된다
    let at = reserve(&db, "a", 1).await;
    for outcome in [
        PrivateOutcome::Completed,
        PrivateOutcome::NotSent,
        PrivateOutcome::Held,
    ] {
        assert!(matches!(
            ledger(&db).finish(at.clone(), outcome).await,
            Err(Error::Conflict)
        ));
    }
    ledger(&db)
        .finish(at.clone(), PrivateOutcome::Rejected)
        .await
        .unwrap();
    assert_eq!(row(&db).await, "dead:-:1:1:false");
    assert!(matches!(
        ledger(&db).begin(request("a", 1)).await.unwrap(),
        PrivatePermit::Rejected
    ));

    // 시작 뒤: 미전송 반납·완료·보류·거절만 되고 시작 전 반납은 안 된다
    let at = started(&db, "b", 2).await;
    assert!(matches!(
        ledger(&db)
            .finish(at.clone(), PrivateOutcome::RetryBeforeStart)
            .await,
        Err(Error::Conflict)
    ));
    let mut other = at.clone();
    other.claim = Some(ClaimEpoch(7));
    assert!(matches!(
        ledger(&db).finish(other, PrivateOutcome::Completed).await,
        Err(Error::Stale)
    ));
    ledger(&db)
        .finish(at.clone(), PrivateOutcome::Completed)
        .await
        .unwrap();
    assert!(matches!(
        ledger(&db)
            .finish(at.clone(), PrivateOutcome::Completed)
            .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        ledger(&db).begin(request("b", 2)).await.unwrap(),
        PrivatePermit::Completed
    ));

    // 결과를 모르면 보류한다. 보류에서 거절·미전송으로 바꾸는 길은 없다(운영자 판정뿐)
    let at = started(&db, "c", 3).await;
    ledger(&db)
        .finish(at.clone(), PrivateOutcome::Held)
        .await
        .unwrap();
    assert!(matches!(
        ledger(&db).begin(request("c", 3)).await.unwrap(),
        PrivatePermit::Held
    ));
    for outcome in [
        PrivateOutcome::Rejected,
        PrivateOutcome::NotSent,
        PrivateOutcome::RetryBeforeStart,
        PrivateOutcome::Held,
    ] {
        assert!(matches!(
            ledger(&db).finish(at.clone(), outcome).await,
            Err(Error::Conflict)
        ));
    }
    // 시작이 확인된 행의 늦은 성공은 받는다
    ledger(&db)
        .finish(at, PrivateOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'private.finish'")
            .await,
        4
    );
}

#[tokio::test]
async fn a_released_row_is_reclaimed_only_by_the_same_redelivery_with_a_new_epoch() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 시작 전 반납
    let first = reserve(&db, "r1", 1).await;
    ledger(&db)
        .finish(first.clone(), PrivateOutcome::RetryBeforeStart)
        .await
        .unwrap();
    assert_eq!(row(&db).await, "ready:-:1:1:false");
    assert!(runs(&db).claim().await.unwrap().is_none());
    let PrivatePermit::Start(second) = ledger(&db).begin(request("r1", 1)).await.unwrap() else {
        panic!("반납된 행은 같은 입력의 재전달이 다시 점유한다")
    };
    assert_eq!(
        (second.run.clone(), second.generation.0, epoch(&second)),
        (first.run.clone(), 1, 2)
    );
    // 옛 소유자는 시작도 결과 기록도 못 한다
    assert!(matches!(
        ledger(&db).start(start_of(&first, 1)).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        ledger(&db)
            .finish(first, PrivateOutcome::RetryBeforeStart)
            .await,
        Err(Error::Stale)
    ));
    // 시작한 뒤 전송 전 실패가 확정되면 시작 표시를 지우고 반납한다
    ledger(&db).start(start_of(&second, 1)).await.unwrap();
    ledger(&db)
        .finish(second, PrivateOutcome::NotSent)
        .await
        .unwrap();
    assert_eq!(row(&db).await, "ready:-:1:2:false");
    let PrivatePermit::Start(third) = ledger(&db).begin(request("r1", 1)).await.unwrap() else {
        panic!("재점유")
    };
    assert_eq!(epoch(&third), 3);
    ledger(&db).start(start_of(&third, 1)).await.unwrap();
    assert_eq!(row(&db).await, "leased:-:1:3:true");
}

#[tokio::test]
async fn a_redelivery_must_match_the_first_request_exactly() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    reserve(&db, "r1", 1).await;
    type Edit = fn(&mut PrivateRequest);
    let edits: [(&str, Edit); 5] = [
        ("지문", |r| r.fingerprint = [2; 32]),
        ("에이전트", |r| r.agent = AgentId("other".into())),
        ("도구", |r| r.tool = ToolId("inbox.delete".into())),
        ("대화", |r| r.conversation.id = "dm-2".into()),
        ("스레드", |r| r.conversation.thread = Some("t2".into())),
    ];
    for (what, edit) in edits {
        let mut other = request("r1", 1);
        edit(&mut other);
        assert!(
            matches!(ledger(&db).begin(other).await, Err(Error::Conflict)),
            "{what}가 다르면 충돌"
        );
    }
    // 개인 DM이 아닌 대화는 첫 접수부터 받지 않는다
    let mut channel = request("r2", 1);
    channel.conversation.kind = ConversationKind::Channel;
    assert!(matches!(
        ledger(&db).begin(channel).await,
        Err(Error::Invalid)
    ));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
}

#[tokio::test]
async fn a_private_attempt_cannot_use_any_general_path() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = reserve(&db, "r1", 1).await;
    let denied = |e: &Error| matches!(e, Error::Denied | Error::Stale | Error::Missing);

    // 호출 원장: 맥락·호출·승인 카드·시작 모두 거절(모델 프록시도 맥락을 얻지 못한다)
    assert!(matches!(
        calls(&db).context(at.clone()).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        calls(&db)
            .begin(at.clone(), call("p-1"), tool(TOOL), [1; 32])
            .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        calls(&db)
            .open_approval(open_approval(binding(&at, "p-1", "n1")))
            .await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        calls(&db).start(start_call(&at, "p-1", None)).await,
        Err(Error::Denied)
    ));
    // 큐: 갱신·체크포인트·완료·실패·이력
    assert!(matches!(
        runs(&db).renew(at.clone()).await,
        Err(Error::Denied)
    ));
    let step = Step {
        key: "k".into(),
        body: JsonPayload("{}".into()),
    };
    assert!(matches!(
        runs(&db).checkpoint(at.clone(), step).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        runs(&db).finish(at.clone(), done("x")).await,
        Err(Error::Denied)
    ));
    assert!(matches!(
        runs(&db).fail(at.clone(), Failure::Permanent("x")).await,
        Err(Error::Denied)
    ));
    assert!(runs(&db).history(at.clone(), 5).await.is_err());
    // 단계 원장·기억·위임
    let effect = EffectRequest {
        attempt: at.clone(),
        call: call("p-1"),
        step: "s1".into(),
        args: JsonPayload("{}".into()),
    };
    assert!(
        effects(&db)
            .request(effect)
            .await
            .is_err_and(|e| denied(&e))
    );
    assert!(
        memories(&db)
            .search(at.clone(), "x".into())
            .await
            .is_err_and(|e| denied(&e))
    );
    let change = bot_core::command::MemoryChange::Delete {
        id: MemoryId("m-1".into()),
    };
    assert!(
        memories(&db)
            .change(at.clone(), change)
            .await
            .is_err_and(|e| denied(&e))
    );
    let delegate = Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: at.clone(),
        step: "d1".into(),
        child: AgentId("main/research".into()),
        instruction: "x".into(),
        scopes: scopes(&["run"]),
    };
    assert!(
        bot_core::port::DelegationStore::delegate(&db.store, delegate)
            .await
            .is_err_and(|e| denied(&e))
    );
    // 거절은 어떤 호출·단계·승인·체크포인트 행도 만들지 않았다
    for table in ["bot_calls", "bot_effects", "bot_approvals", "bot_steps"] {
        let sql = format!("select count(*) from {table}");
        assert_eq!(db.scalar::<i64>(&sql).await, 0, "{table}");
    }
    assert_eq!(row(&db).await, "leased:-:1:1:false");
}
