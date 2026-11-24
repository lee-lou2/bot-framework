//! 예약 회신(PG): 동결한 초안만 소유자가 승인한 뒤 목적지 outbox로 쌓이고 run이 끝난다. 승인 문서의 SHA-256은 저장된 바이트에서 다시 계산한다.

mod support;

use bot_core::{
    command::*,
    id::*,
    ledger::*,
    port::*,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
    state::*,
};
use support::{delivery::*, *};

async fn frozen(db: &TestDb, run: &RunContext) -> bot_core::schedule::DeliveryDraft {
    replies(db)
        .draft(
            run.attempt.clone(),
            Some(answers(&["아침 요약", "두 번째 답"])),
        )
        .await
        .unwrap()
        .expect("초안")
}

#[tokio::test]
async fn a_draft_is_frozen_once_from_the_raw_answers_and_reread_from_the_stored_bytes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let first = frozen(&db, &run).await;
    assert_eq!(first.activation.0, "set-1");
    assert_eq!(first.target.binding.0, "slack");
    assert_eq!(first.target.conversation.id, "D9");
    assert_eq!(first.answers.len(), 2);
    assert_eq!(
        db.scalar::<String>("select state from bot_deliveries")
            .await,
        "drafted"
    );
    assert_eq!(
        db.scalar::<Vec<u8>>("select sha256 from bot_deliveries")
            .await,
        first.sha256().unwrap().to_vec()
    );
    // 한 번 동결하면 다른 답을 내도 같은 초안이다(재개한 시도가 다른 답을 만들어도 승인한 것만 보낸다)
    let again = replies(&db)
        .draft(run.attempt.clone(), Some(answers(&["다른 답"])))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.sha256().unwrap(), first.sha256().unwrap());
    let reread = replies(&db)
        .draft(run.attempt.clone(), None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reread.approval_json().unwrap(),
        first.approval_json().unwrap()
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.draft'")
            .await,
        1
    );
}

#[tokio::test]
async fn a_run_without_a_destination_or_a_quiet_success_needs_no_draft() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let plain = run_in(&db, "p1", OWNER, ConversationKind::Direct).await;
    assert!(
        replies(&db)
            .draft(plain.attempt.clone(), Some(answers(&["답"])))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        replies(&db)
            .draft(plain.attempt.clone(), None)
            .await
            .unwrap()
            .is_none()
    );
    runs(&db)
        .finish(plain.attempt.clone(), answers(&["답"]))
        .await
        .unwrap();
    // 조용한 예약 성공(첨부 없음)은 보낼 것이 없다: 초안 없이 일반 완료가 답 없이 끝낸다
    let quiet = destination_run_as(&db, "q1", ConversationKind::Direct).await;
    db.sql(&format!(
        "update bot_runs set quiet = true where id = '{}'",
        quiet.attempt.run.0
    ))
    .await;
    assert!(
        replies(&db)
            .draft(quiet.attempt.clone(), Some(answers(&["<조용히>"])))
            .await
            .unwrap()
            .is_none()
    );
    runs(&db)
        .finish(quiet.attempt.clone(), answers(&["<조용히>"]))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>(&format!(
            "select count(*) from bot_outbox where run = '{}'",
            quiet.attempt.run.0
        ))
        .await,
        0
    );
    assert_eq!(
        db.scalar::<String>(&format!(
            "select state from bot_deliveries where run = '{}'",
            quiet.attempt.run.0
        ))
        .await,
        "dropped"
    );
}

#[tokio::test]
async fn a_normal_finish_cannot_complete_a_destination_run_and_bad_answers_freeze_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    // 승인 없이 일반 완료로 원래 대화에 답을 돌리지 못한다
    assert!(matches!(
        runs(&db).finish(run.attempt.clone(), done("답")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
    // 중복 키(JSONB로 가면 하나로 합쳐진다)·다른 MIME 첨부·빈 답은 동결하지 않는다
    for raw in [
        r#"{"text":"a","text":"b"}"#,
        r#"{"text":"x","attachments":[{"name":"a.png","type":"image/png","data":"AA=="}]}"#,
        r#"{"attachments":[{"name":"a.txt","type":"text/plain","data":"/w=="}]}"#,
        r#"{"approval":{"nonce":"n"}}"#,
    ] {
        let bad = Finish {
            result: JsonPayload(r#"{"text":"t"}"#.into()),
            answers: vec![JsonPayload(raw.into())],
        };
        assert!(
            matches!(
                replies(&db).draft(run.attempt.clone(), Some(bad)).await,
                Err(Error::Invalid)
            ),
            "{raw}"
        );
    }
    assert!(
        matches!(
            replies(&db)
                .draft(
                    run.attempt.clone(),
                    Some(Finish {
                        result: JsonPayload(r#"{"text":"t"}"#.into()),
                        answers: vec![],
                    })
                )
                .await,
            Err(Error::Invalid)
        ),
        "답이 없으면 보낼 것이 없다"
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_deliveries")
            .await,
        "pending"
    );
}

#[tokio::test]
async fn the_approved_draft_reaches_the_destination_and_ends_the_run_in_one_commit() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (draft, approval) =
        request_delivery(&db, &run, answers(&["아침 요약", "두 번째 답"])).await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    // 호출은 동결한 초안의 지문으로 봉인돼 있고, 승인 전에는 아무것도 보내지 않는다
    assert_eq!(
        db.scalar::<Vec<u8>>("select args_sha256 from bot_calls where call = 'schedule-deliver'")
            .await,
        draft.sha256().unwrap().to_vec()
    );
    assert_eq!(outbox_rows(&db, "D9").await, 0);

    let (attempt, start) = approved_start(&db, &approval).await;
    replies(&db).enqueue(start).await.unwrap();
    assert_eq!(state_of_run(&db, &attempt.run).await, "succeeded");
    assert_eq!(
        db.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            attempt.run.0
        ))
        .await,
        r#"{"text":"아침 요약/두 번째 답"}"#
    );
    // 목적지 outbox: 순서대로 두 건, 요청자는 소유자, 같은 run·고정 호출 ID
    let sent: Vec<(String, String, String, Option<String>, String)> = sqlx::query_as(
        "select binding, conv_kind, conv_id, thread, body from bot_outbox where conv_id = 'D9' order by seq",
    )
    .fetch_all(db.store.pool())
    .await
    .unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, "slack");
    assert_eq!(sent[0].1, "direct");
    assert_eq!(sent[0].3.as_deref(), Some("t1"));
    assert_eq!(sent[0].4, r#"{"text":"아침 요약"}"#);
    assert_eq!(sent[1].4, r#"{"text":"두 번째 답"}"#);
    assert_eq!(
        db.scalar::<String>("select principal from bot_outbox where conv_id = 'D9' limit 1")
            .await,
        OWNER
    );
    // 원래 대화에는 답이 가지 않는다(승인 카드 한 건만 있다)
    assert_eq!(outbox_rows(&db, "c1").await, 1);
    // 승인 소비·호출 완료·초안 확정·감사가 같은 커밋이다
    assert_eq!(
        calls(&db)
            .approval(approval.nonce.clone())
            .await
            .unwrap()
            .state,
        ApprovalState::Consumed
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_calls where call = 'schedule-deliver'")
            .await,
        "done"
    );
    assert_eq!(
        db.scalar::<String>("select result from bot_calls where call = 'schedule-deliver'")
            .await,
        r#"{"queued":2}"#
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_deliveries")
            .await,
        "enqueued"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.enqueue'")
            .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_second_enqueue_sends_nothing_more() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &run, answers(&["하나"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
    let again = StartCall {
        approval: start.approval.clone(),
        ..start_call(&attempt, DELIVER_CALL, None)
    };
    replies(&db).enqueue(start).await.unwrap();
    assert_eq!(outbox_rows(&db, "D9").await, 1);
    // run이 끝났으므로 같은 호출을 다시 내도 이어지지 않는다(이중 발송 없음)
    assert!(matches!(
        replies(&db).enqueue(again).await,
        Err(Error::Stale)
    ));
    assert_eq!(outbox_rows(&db, "D9").await, 1);
}

#[tokio::test]
async fn a_tampered_draft_a_changed_activation_or_an_unapproved_call_sends_nothing_and_keeps_the_approval()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &run, answers(&["보낼 답"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
    let again = |start: &StartCall| StartCall {
        approval: start.approval.clone(),
        ..start_call(&attempt, DELIVER_CALL, None)
    };
    let nothing_happened = |db: &TestDb| {
        let db = db.store.clone();
        async move {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "select count(*) from bot_outbox where conv_id = 'D9'"
                )
                .fetch_one(db.pool())
                .await
                .unwrap(),
                0
            );
        }
    };

    // 승인 없이는 시작하지 못한다(승인한 호출을 가리키고 있다)
    let mut missing = again(&start);
    missing.approval = None;
    assert!(matches!(
        replies(&db).enqueue(missing).await,
        Err(Error::Denied)
    ));
    nothing_happened(&db).await;

    // 저장된 답이 바뀌면 PG가 다시 계산한 지문이 달라 보내지 않는다
    db.sql(r#"update bot_deliveries set answers = array['{"text":"바뀐 답"}']"#)
        .await;
    assert!(matches!(
        replies(&db).enqueue(again(&start)).await,
        Err(Error::Conflict)
    ));
    nothing_happened(&db).await;
    db.sql(r#"update bot_deliveries set answers = array['{"text":"보낼 답"}']"#)
        .await;

    // 수신자 매핑이 바뀐 활성 집합이면 이 초안은 보내지 않는다(다시 읽어도 마찬가지다)
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        replies(&db).enqueue(again(&start)).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        replies(&db).draft(attempt.clone(), None).await,
        Err(Error::Conflict)
    ));
    nothing_happened(&db).await;
    db.sql("update bot_active set revision = 'set-1'").await;
    assert_eq!(
        calls(&db)
            .approval(approval.nonce.clone())
            .await
            .unwrap()
            .state,
        ApprovalState::Approved
    );
    assert_eq!(state_of_run(&db, &attempt.run).await, "leased");

    // 바로잡으면 같은 승인으로 한 번 보낸다
    replies(&db).enqueue(start).await.unwrap();
    assert_eq!(outbox_rows(&db, "D9").await, 1);
}

#[tokio::test]
async fn the_delivery_call_and_its_approval_only_open_against_the_frozen_draft_and_the_owner() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
    // 초안이 없으면 호출을 기록하지 못한다. 호출 ID와 도구도 서로만 쓴다
    assert!(is_conflict(
        calls(&db)
            .begin(at.clone(), call(DELIVER_CALL), tool(DELIVER_TOOL), [3; 32])
            .await
    ));
    assert!(matches!(
        calls(&db)
            .begin(at.clone(), call("c9"), tool(DELIVER_TOOL), [3; 32])
            .await,
        Err(Error::Invalid)
    ));
    assert!(matches!(
        calls(&db)
            .begin(at.clone(), call(DELIVER_CALL), tool("web.search"), [3; 32])
            .await,
        Err(Error::Invalid)
    ));
    let draft = frozen(&db, &run).await;
    let hash = draft.sha256().unwrap();
    // 초안과 다른 내용의 호출은 기록하지 못한다
    assert!(is_conflict(
        calls(&db)
            .begin(at.clone(), call(DELIVER_CALL), tool(DELIVER_TOOL), [3; 32])
            .await
    ));
    calls(&db)
        .begin(at.clone(), call(DELIVER_CALL), tool(DELIVER_TOOL), hash)
        .await
        .unwrap();
    let document = draft.approval_json().unwrap();
    let approval = delivery_approval(&at, "n1", hash);
    // 카드가 초안을 그대로 보이지 않거나(다른 문서·조각 누락) 승인 지문이 다르면 열지 못한다
    for card in [
        card_for("n1", &document.replace("아침 요약", "다른 요약")),
        card_for("n1", &document[..document.len() / 2]),
        JsonPayload(
            r#"{"approval":{"nonce":"n1","title":"승인","lines":["도구: schedule.deliver"]}}"#
                .into(),
        ),
    ] {
        let open = OpenApproval {
            card,
            ..open_delivery(&approval, &document)
        };
        assert!(is_conflict(calls(&db).open_approval(open).await));
    }
    let other_hash = delivery_approval(&at, "n1", [4; 32]);
    assert!(is_conflict(
        calls(&db)
            .open_approval(open_delivery(&other_hash, &document))
            .await
    ));
    // 승인자는 예약을 만든 소유자 본인이다(다른 사람·요청자와 다른 승인자는 거절)
    for (requester, approver) in [(OWNER, "local:other"), ("local:other", OWNER)] {
        let mut forged = delivery_approval(&at, "n1", hash);
        forged.requester = PrincipalId(requester.into());
        forged.approver = PrincipalId(approver.into());
        assert!(matches!(
            calls(&db)
                .open_approval(open_delivery(&forged, &document))
                .await,
            Err(Error::Denied)
        ));
    }
    let mut risky = delivery_approval(&at, "n1", hash);
    risky.risk = Risk::Write;
    assert!(matches!(
        calls(&db)
            .open_approval(open_delivery(&risky, &document))
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    calls(&db)
        .open_approval(open_delivery(&approval, &document))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_earlier_occurrences_approval_cannot_deliver_a_later_occurrence() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = destination_run(&db, ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &first, answers(&["첫 발화"])).await;
    let (_, first_start) = approved_start(&db, &approval).await;
    // 같은 예약의 다음 발화: 같은 호출 ID(`schedule-deliver`)지만 다른 run이다
    runs(&db)
        .accept(accept_as(
            "personal",
            "r2",
            OWNER,
            ConversationKind::Direct,
            "c1",
            "다음",
        ))
        .await
        .unwrap();
    let second = runs(&db).claim().await.unwrap().unwrap();
    db.sql(&format!(
        "insert into bot_deliveries (profile, run, dest_binding, dest_kind, dest_id, dest_thread)
         values ('personal', '{}', 'slack', 'direct', 'D9', 't1')",
        second.attempt.run.0
    ))
    .await;
    let (draft, second_approval) = {
        let draft = replies(&db)
            .draft(second.attempt.clone(), Some(answers(&["둘째 발화"])))
            .await
            .unwrap()
            .unwrap();
        calls(&db)
            .begin(
                second.attempt.clone(),
                call(DELIVER_CALL),
                tool(DELIVER_TOOL),
                draft.sha256().unwrap(),
            )
            .await
            .unwrap();
        let approval = delivery_approval(&second.attempt, "n2", draft.sha256().unwrap());
        calls(&db)
            .open_approval(open_delivery(&approval, &draft.approval_json().unwrap()))
            .await
            .unwrap();
        (draft, approval)
    };
    let _ = (draft, second_approval);
    // 첫 발화의 승인으로는 둘째 run을 보내지 못한다
    let stolen = StartCall {
        approval: first_start.approval,
        ..start_call(&second.attempt, DELIVER_CALL, None)
    };
    assert!(replies(&db).enqueue(stolen).await.is_err());
    assert_eq!(outbox_rows(&db, "D9").await, 0);
}
