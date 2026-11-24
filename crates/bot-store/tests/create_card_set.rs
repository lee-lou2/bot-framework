//! 수신자를 지정한 `schedule.create` 승인 카드도 전송 카드처럼 호스트 활성 집합 잠금 아래에서 열린다: 수신자를 확인한 등록부가 속한 집합이 카드를 기록하는
//! **같은 커밋에서도** 현재 집합이어야 한다. 다른 프로세스의 집합 교체가 먼저 커밋되면 카드도 예약도 0이다.
//! 수신자 없는 일반 생성(원래 대화)과 다른 승인은 집합이 낡았어도 예전처럼 열린다(이미 집은 run은 끝까지 한다).
//! 수신자를 지정한 생성인지는 카드의 `인자` 행(브로커가 봉인한 정규 인자 JSON)의 최상위 `destination`으로 읽는다.
//! 수신자를 지정한 생성은 확인한 등록부의 집합을 고정한 핸들(`for_activation`)로만 연다: 고정하지 않은 핸들은 그 집합을 몰라 비교할 수 없으므로
//! 아무것도 기록하기 전에 거절한다. `인자` 행을 읽을 수 없으면 일반 생성으로 보지 않고 `Invalid`로 닫는다.

mod support;

use bot_core::{id::*, ledger::*, port::*, state::*};
use serde_json::json;
use sqlx::{Connection, Executor};
use std::time::Duration;
use support::*;

const TOOL: &str = "schedule.create";
const PLAIN: &str = r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","quiet":false}"#;
const ROUTED: &str = r#"{"at":"2030-01-01T12:00:00Z","destination":{"binding":"slack","conversation":{"id":"T1/C9","kind":"channel","thread":null}},"instruction":"x","quiet":false}"#;

/// `args` 한 줄을 `인자` 행에 담은 생성 승인 카드(이어지는 줄은 공백 두 칸으로 시작한다).
fn card(nonce: &str, args: &str) -> bot_core::command::JsonPayload {
    let mut lines = vec![
        "도구: schedule.create".to_owned(),
        "위험: irreversible".to_owned(),
        "이유: irreversible".to_owned(),
        "요청자: local:owner".to_owned(),
        "승인자: local:owner".to_owned(),
    ];
    let mut rest = args;
    let mut first = true;
    while !rest.is_empty() {
        let prefix = if first { "인자: " } else { "  " };
        first = false;
        let mut cut = (200 - prefix.len()).min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        lines.push(format!("{prefix}{head}"));
        rest = tail;
    }
    bot_core::command::JsonPayload(
        json!({"approval": {"nonce": nonce, "title": "승인 요청: schedule.create", "lines": lines}})
            .to_string(),
    )
}

/// 호출을 기록하고 열 카드를 만든다(카드는 아직 열지 않았다).
async fn create_card(db: &TestDb, run: &RunContext, call_id: &str, args: &str) -> OpenApproval {
    // 카드에 보인 인자(`인자` 행)의 SHA-256이 호출을 연 지문이자 승인 지문이다(저장소가 세 값을 대조한다)
    let hash = bot_core::canon::sha256(args);
    calls(db)
        .begin(run.attempt.clone(), call(call_id), tool(TOOL), hash)
        .await
        .unwrap();
    let approval = Approval {
        tool: tool(TOOL),
        risk: Risk::Irreversible,
        args_sha256: hash,
        ..binding(&run.attempt, call_id, &format!("n-{call_id}"))
    };
    OpenApproval {
        reasons: vec!["irreversible".into()],
        card: card(&approval.nonce.0, args),
        approval,
    }
}

async fn approvals(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_approvals").await
}

async fn schedules(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_user_schedules")
        .await
}

#[tokio::test]
async fn a_replacement_committed_first_leaves_no_card_and_no_schedule_for_a_routed_create() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let open = create_card(&db, &run, "c1", ROUTED).await;
    // 다른 프로세스의 집합 교체가 먼저 커밋됐다
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        CallLedger::open_approval(&pinned, open).await,
        Err(Error::Stale)
    ));
    assert_eq!(approvals(&db).await, 0, "카드 0");
    assert_eq!(schedules(&db).await, 0, "예약 0");
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
}

#[tokio::test]
async fn a_plain_create_and_other_approvals_still_open_on_an_older_pinned_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let plain = create_card(&db, &run, "c1", PLAIN).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    // 수신자 없는 생성(원래 대화): 카드에 `destination`이 없으므로 예전처럼 열린다
    CallLedger::open_approval(&pinned, plain).await.unwrap();
    assert_eq!(approvals(&db).await, 1);
    // 지시문이 `destination`이라는 글자를 담아도 최상위 키가 아니라 수신자 지정으로 읽지 않는다
    let run2 = run_in(&db, "r2", OWNER, ConversationKind::Direct).await;
    let tricky = create_card(
        &db,
        &run2,
        "c2",
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"\"destination\":{\"binding\":\"x\"}","quiet":false}"#,
    )
    .await;
    CallLedger::open_approval(&pinned, tricky).await.unwrap();
    assert_eq!(approvals(&db).await, 2);
    // 수신자 없는 생성은 고정하지 않은 핸들로도 예전처럼 열린다
    let run3 = run_in(&db, "r3", OWNER, ConversationKind::Direct).await;
    let plain_again = create_card(&db, &run3, "c3", PLAIN).await;
    CallLedger::open_approval(&db.store, plain_again)
        .await
        .unwrap();
    assert_eq!(approvals(&db).await, 3);
}

/// `인자` 행을 읽을 수 없는 생성 카드들(행이 없다·JSON이 아니다·객체가 아니다·행이 둘이다).
fn unreadable_cards(nonce: &str) -> Vec<(&'static str, bot_core::command::JsonPayload)> {
    let with = |lines: Vec<String>| {
        bot_core::command::JsonPayload(
            json!({"approval": {"nonce": nonce, "title": "t", "lines": lines}}).to_string(),
        )
    };
    vec![
        ("행 없음", with(vec!["도구: schedule.create".to_owned()])),
        ("JSON 아님", with(vec!["인자: {\"at\":".to_owned()])),
        ("객체 아님", with(vec!["인자: [1,2]".to_owned()])),
        ("문자열", with(vec!["인자: \"destination\"".to_owned()])),
        (
            "행이 둘",
            with(vec![format!("인자: {PLAIN}"), format!("인자: {ROUTED}")]),
        ),
    ]
}

#[tokio::test]
async fn a_create_card_whose_arguments_row_cannot_be_read_is_closed_as_invalid_not_treated_as_plain()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    for (n, handle) in [("고정", Some(&pinned)), ("고정 안 함", None)]
        .into_iter()
        .enumerate()
    {
        let (what, handle) = handle;
        for (m, (label, card)) in unreadable_cards("n").into_iter().enumerate() {
            let run = leased(&db, &format!("r{n}{m}")).await;
            let mut open = create_card(&db, &run, "c1", PLAIN).await;
            open.card = card;
            let result = match handle {
                Some(pinned) => CallLedger::open_approval(pinned, open).await,
                None => CallLedger::open_approval(&db.store, open).await,
            };
            assert!(
                matches!(result, Err(Error::Invalid)),
                "{what} / {label}: {result:?}"
            );
            assert_eq!(approvals(&db).await, 0, "{what} / {label}: 카드 0");
            assert!(
                matches!(
                    calls(&db)
                        .lookup(run.attempt.clone(), call("c1"))
                        .await
                        .unwrap()
                        .unwrap()
                        .phase,
                    CallPhase::Open { approval: None }
                ),
                "{what} / {label}"
            );
        }
    }
}

#[tokio::test]
async fn a_routed_create_cannot_be_opened_by_a_handle_that_does_not_pin_the_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let open = create_card(&db, &run, "c1", ROUTED).await;
    // 집합이 바뀌지 않았어도: 고정하지 않은 핸들은 등록부가 확인한 집합을 몰라 비교할 수 없다 → 기록 전에 거절한다
    let result = CallLedger::open_approval(&db.store, open).await;
    assert!(matches!(result, Err(Error::Denied)), "{result:?}");
    assert_eq!(approvals(&db).await, 0, "카드 0");
    assert_eq!(schedules(&db).await, 0, "예약 0");
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
    // 같은 카드를 고정한 핸들로는 열린다(집합이 그대로이므로)
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let again = create_card(&db, &run, "c1", ROUTED).await;
    CallLedger::open_approval(&pinned, again).await.unwrap();
    assert_eq!(approvals(&db).await, 1);
}

/// 집합 교체가 진행 중인 동안(활성 집합 행 잠금을 쥔 별도 연결) 고정 핸들이 수신자를 지정한 생성 카드를 열려 한다.
async fn open_during_a_replacement(replace: bool) -> Option<(Result<(), Error>, i64, i64)> {
    let db = TestDb::new().await?;
    let run = leased(&db, "r").await;
    let open = create_card(&db, &run, "c1", ROUTED).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&pinned, open).await });
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
    Some((result, approvals(&db).await, schedules(&db).await))
}

#[tokio::test]
async fn a_replacement_in_flight_is_waited_for_and_the_routed_card_is_judged_after_it() {
    // 교체가 커밋되면: 카드 열기가 그 커밋을 기다린 뒤 낡은 집합으로 판정해 열지 않는다(카드 0·예약 0)
    let Some((result, cards, made)) = open_during_a_replacement(true).await else {
        return;
    };
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!((cards, made), (0, 0));
    // 대조군: 교체 없이 잠금만 쥐었다 놓으면 기다린 뒤 같은 집합으로 카드를 연다(예약은 소유자가 승인하기 전이라 아직 0)
    let (result, cards, made) = open_during_a_replacement(false).await.unwrap();
    result.unwrap();
    assert_eq!((cards, made), (1, 0));
}
