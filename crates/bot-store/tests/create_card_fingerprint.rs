//! 예약 생성 승인 카드는 카드에 **보인** 인자(`인자` 행의 정규 JSON 원문)의 SHA-256이 승인 지문과 호출을 연 지문과 같을 때만 열린다.
//! 보이는 것(날짜·수신자·지시문)과 승인·소비되는 것이 다르면 카드도 기록도 0이고 run은 멈추지 않는다. 읽을 수 없는 행은 `Invalid`(카드 시험이 맡는다).

mod support;

use bot_core::{
    id::*,
    ledger::*,
    port::*,
    schedule::{DeliveryTarget, ScheduleSpec, Timing},
    state::*,
};
use std::time::Duration;
use support::{schedule::*, *};

const OWNER: &str = "local:owner";

fn approval_for(run: &RunContext, call_id: &str, hash: [u8; 32]) -> Approval {
    Approval {
        tool: tool("schedule.create"),
        args_sha256: hash,
        ..binding(&run.attempt, call_id, &format!("n-{call_id}"))
    }
}

/// 카드도 승인도 열리지 않았다: 호출은 열린 채 승인이 없고, run은 계속 실행 중이며, 승인 감사도 없다.
async fn nothing_opened(db: &TestDb, run: &RunContext, call_id: &str) {
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert!(matches!(
        calls(db)
            .lookup(run.attempt.clone(), call(call_id))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(db, &run.attempt.run).await, "leased");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'approval.open'")
            .await,
        0
    );
}

fn a_day_later(spec: &ScheduleSpec) -> ScheduleSpec {
    let mut later = spec.clone();
    later.timing = match &spec.timing {
        Timing::At(at) => Timing::At(*at + Duration::from_secs(86_400)),
        other => other.clone(),
    };
    later
}

#[tokio::test]
async fn a_card_that_shows_other_arguments_than_the_sealed_ones_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 3_600_000).await;
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;

    // 날짜만 하루 다른 카드: 승인·호출 지문은 원래 명세의 것이다
    let shown = a_day_later(&spec).seal_create().unwrap().canonical;
    assert_ne!(shown, spec.seal_create().unwrap().canonical);
    let got = calls(&db)
        .open_approval(open_create_text(approval_for(&run, "c1", hash), &shown))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;

    // 같은 뜻이어도 원문 바이트가 다르면(공백·키 순서) 다른 지문이다: 보인 원문 그대로의 해시만 맞는다
    let canonical = spec.seal_create().unwrap().canonical;
    let spaced = canonical.replacen(',', ", ", 1);
    assert_ne!(spaced, canonical);
    for shown in [
        spaced,
        format!("{canonical} "),
        canonical.replace("\"quiet\"", "\"quiet \""),
    ] {
        let got = calls(&db)
            .open_approval(open_create_text(approval_for(&run, "c1", hash), &shown))
            .await;
        assert!(matches!(got, Err(Error::Conflict)), "{shown}: {got:?}");
    }
    nothing_opened(&db, &run, "c1").await;

    // 보인 인자가 봉인한 인자 그대로이면 열린다
    calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
}

#[tokio::test]
async fn the_call_that_was_opened_must_carry_the_same_fingerprint_as_the_card_and_the_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 3_600_000).await;
    let other = a_day_later(&spec);
    // 호출은 다른 명세의 지문으로 열렸고, 카드와 승인은 서로 같은 명세를 가리킨다
    begin_tool(&db, &run, "c1", "schedule.create", sealed(&other)).await;
    let got = calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", sealed(&spec)), &spec))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
}

#[tokio::test]
async fn a_long_card_split_over_continuation_lines_is_judged_by_its_whole_text() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let mut spec = spec_in(&db, 3_600_000).await;
    spec.instruction = "긴 지시 ".repeat(300); // 줄 상한(480바이트)을 여러 번 넘는다
    let canonical = spec.seal_create().unwrap().canonical;
    assert!(canonical.len() > 2000);
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    // 이어지는 줄 하나의 글자를 바꾸면 전체 지문이 달라진다
    let tampered = canonical.replacen(
        "긴 지시 긴 지시 긴 지시 긴 지시 긴 지시",
        "긴 지시 긴 지시 긴 지시 긴 지시 다른",
        1,
    );
    assert_ne!(tampered, canonical);
    let got = calls(&db)
        .open_approval(open_create_text(approval_for(&run, "c1", hash), &tampered))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
    // 그대로면 여러 줄을 이어 붙여도 같은 지문이라 열린다
    calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
}

#[tokio::test]
async fn a_routed_create_card_naming_another_recipient_than_the_sealed_one_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let target = |id: &str| DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: bot_core::state::Conversation {
            kind: ConversationKind::Channel,
            id: id.into(),
            thread: None,
        },
    };
    let mut spec = spec_in(&db, 3_600_000).await;
    spec.destination = Some(target("T1/C9"));
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    // 카드에는 다른 채널이 보인다
    let mut elsewhere = spec.clone();
    elsewhere.destination = Some(target("T1/C1"));
    let shown = elsewhere.seal_create().unwrap().canonical;
    let got = CallLedger::open_approval(
        &pinned,
        open_create_text(approval_for(&run, "c1", hash), &shown),
    )
    .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
    // 보인 수신자가 봉인한 수신자이면 열린다
    CallLedger::open_approval(&pinned, open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
}
