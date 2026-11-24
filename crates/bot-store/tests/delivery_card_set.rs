//! 전송 승인 카드는 호스트 활성 집합 잠금 아래에서 열린다: 카드에 보인 수신자를 확인한 등록부가 속한 집합이 **카드를 기록하는 같은 커밋에서도** 현재 집합이어야 한다.
//! - 초안을 동결한 집합이 더는 현재가 아니면 카드를 열지 않는다(`Conflict`): 옛 집합의 수신자 매핑으로 만든 초안은 소유자에게 보이지 않는다.
//! - 고정 핸들(`for_activation`)의 집합이 낡았으면 카드를 열지 않는다(`Stale`), 집합 교체가 진행 중이면 그 커밋을 기다린 뒤 판정한다(직렬화).
//! - 다른 승인(일반·생성·기억 공유)은 집합이 낡았어도 예전처럼 열린다(이미 집은 run은 끝까지 한다).

mod support;

use bot_core::{
    id::*,
    ledger::*,
    port::*,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
    state::*,
};
use sqlx::{Connection, Executor};
use std::time::Duration;
use support::{delivery::*, *};

/// 초안을 동결하고 호출을 기록한 상태(카드는 아직 열지 않았다)와 열 카드.
async fn frozen(db: &TestDb) -> (RunContext, OpenApproval) {
    let run = destination_run(db, ConversationKind::Direct).await;
    let draft = replies(db)
        .draft(run.attempt.clone(), Some(answers(&["보낼 답"])))
        .await
        .unwrap()
        .expect("초안");
    assert_eq!(draft.activation.0, "set-1");
    let hash = draft.sha256().unwrap();
    calls(db)
        .begin(
            run.attempt.clone(),
            call(DELIVER_CALL),
            tool(DELIVER_TOOL),
            hash,
        )
        .await
        .unwrap();
    let approval = delivery_approval(&run.attempt, "n1", hash);
    let open = open_delivery(&approval, &draft.approval_json().unwrap());
    (run, open)
}

async fn approvals(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_approvals").await
}

#[tokio::test]
async fn a_draft_frozen_under_another_set_is_never_shown_to_the_owner() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, open) = frozen(&db).await;
    // 초안을 동결한 뒤 호스트 활성 집합이 바뀌었다
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        calls(&db).open_approval(open).await,
        Err(Error::Conflict)
    ));
    // 카드·승인 대기·run 상태 변화가 모두 없다
    assert_eq!(approvals(&db).await, 0);
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call(DELIVER_CALL))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
}

#[tokio::test]
async fn a_card_opens_when_the_set_is_the_one_the_draft_was_frozen_under() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, open) = frozen(&db).await;
    calls(&db).open_approval(open).await.unwrap();
    assert_eq!(approvals(&db).await, 1);
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
}

#[tokio::test]
async fn a_handle_pinned_to_an_older_set_cannot_open_a_delivery_card_but_other_approvals_still_open()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let (run, open) = frozen(&db).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    // 전송 카드: 이 핸들의 집합이 현재가 아니다
    assert!(matches!(
        CallLedger::open_approval(&pinned, open).await,
        Err(Error::Stale)
    ));
    assert_eq!(approvals(&db).await, 0);
    // 일반 승인은 예전처럼 열린다(낡은 핸들로도): 이 호출은 전송 카드가 아닌 다른 호출이다
    CallLedger::begin(
        &pinned,
        run.attempt.clone(),
        call("c2"),
        tool("web.search"),
        [1; 32],
    )
    .await
    .unwrap();
    let other = binding(&run.attempt, "c2", "n2");
    CallLedger::open_approval(&pinned, open_approval(other))
        .await
        .unwrap();
    assert_eq!(approvals(&db).await, 1);
}

/// 집합 교체가 진행 중인 동안(활성 집합 행 잠금을 쥔 별도 연결) 핸들이 카드를 열려 한다(`pinned`면 집합을 고정한 핸들, 아니면 고정하지 않은 공개 핸들).
/// `replace`면 교체가 커밋되고, 아니면 잠금만 놓는다.
async fn open_during_a_replacement(
    pinned: bool,
    replace: bool,
) -> (Result<(), Error>, i64, String) {
    let Some(db) = TestDb::new().await else {
        return (Ok(()), -1, String::new());
    };
    let (run, open) = frozen(&db).await;
    let handle = if pinned {
        db.store.for_activation(Revision("set-1".into()))
    } else {
        db.store.clone()
    };
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&handle, open).await });
    // 카드 열기가 그 잠금을 기다리는 순간까지(시간 추정 없이) 기다린다
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = db
            .scalar(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_active%'",
                db.schema
            ))
            .await;
        if waiting == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "카드 열기가 집합 잠금을 기다리지 않는다"
        );
        assert!(!task.is_finished(), "잠금을 기다리지 않고 끝났다");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if replace {
        tx.execute("update bot_active set revision = 'set-2'")
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let result = task.await.unwrap();
    (
        result,
        approvals(&db).await,
        state_of_run(&db, &run.attempt.run).await,
    )
}

#[tokio::test]
async fn opening_a_card_waits_for_a_set_replacement_in_flight_and_judges_after_it() {
    // 교체가 커밋되면: 카드 열기가 그 커밋을 기다린 뒤 낡은 집합으로 판정해 열지 않는다
    let (result, cards, state) = open_during_a_replacement(true, true).await;
    if cards < 0 {
        return; // DB가 없다
    }
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!(cards, 0, "교체가 끝난 뒤에는 카드가 없다");
    assert_eq!(state, "leased");
    // 대조군: 교체 없이 잠금만 쥐었다 놓으면 기다린 뒤 같은 집합으로 카드를 연다
    let (result, cards, state) = open_during_a_replacement(true, false).await;
    result.unwrap();
    assert_eq!(cards, 1);
    assert_eq!(state, "waiting");
}

#[tokio::test]
async fn a_handle_that_pins_no_set_also_waits_for_the_replacement_and_never_opens_an_old_sets_card()
{
    // 집합을 고정하지 않은 공개 핸들도 같은 경합에서 옛 카드를 만들지 않는다: 교체가 커밋되면 초안을 동결한 집합이 더는 현재가 아니라 `Conflict`다
    let (result, cards, state) = open_during_a_replacement(false, true).await;
    if cards < 0 {
        return;
    }
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    assert_eq!(cards, 0, "카드 0");
    assert_eq!(state, "leased");
    // 대조군: 교체가 없으면 잠금이 풀린 뒤 같은 집합으로 연다
    let (result, cards, state) = open_during_a_replacement(false, false).await;
    result.unwrap();
    assert_eq!((cards, state.as_str()), (1, "waiting"));
}
