//! Scheduled delivery drafts: freeze once, verify stored-byte SHA-256 on approval/enqueue, and prevent replay.

use crate::support::{delivery::*, *};
use bot_core::{
    command::*,
    id::*,
    ledger::*,
    port::*,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
    state::*,
};

async fn frozen(db: &TestDb, run: &RunContext) -> bot_core::schedule::DeliveryDraft {
    replies(db)
        .draft(
            run.attempt.clone(),
            Some(answers(&["morning summary", "second reply"])),
        )
        .await
        .unwrap()
        .expect("draft")
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
    // Once frozen, later attempts cannot replace the approved draft.
    let again = replies(&db)
        .draft(run.attempt.clone(), Some(answers(&["other reply"])))
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
            .draft(plain.attempt.clone(), Some(answers(&["reply"])))
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
        .finish(plain.attempt.clone(), answers(&["reply"]))
        .await
        .unwrap();
    let quiet = destination_run_as(&db, "q1", ConversationKind::Direct).await;
    db.sql(&format!(
        "update bot_runs set quiet = true where id = '{}'",
        quiet.attempt.run.0
    ))
    .await;
    assert!(
        replies(&db)
            .draft(quiet.attempt.clone(), Some(answers(&["<quiet>"])))
            .await
            .unwrap()
            .is_none()
    );
    runs(&db)
        .finish(quiet.attempt.clone(), answers(&["<quiet>"]))
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
    assert!(matches!(
        runs(&db).finish(run.attempt.clone(), done("reply")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
    // Reject duplicate keys, disallowed MIME attachments, invalid base64, and approval payloads.
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
        "empty answers cannot be frozen"
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
    // Origin reply transport and explicit destination transport may differ.
    sqlx::query("update bot_runs set reply_transport = 'test-push' where profile = $1 and id = $2")
        .bind(&run.attempt.profile.0)
        .bind(&run.attempt.run.0)
        .execute(db.store.pool())
        .await
        .unwrap();
    let (draft, approval) =
        request_delivery(&db, &run, answers(&["morning summary", "second reply"])).await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
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
        r#"{"text":"morning summary/second reply"}"#
    );
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
    assert_eq!(sent[0].4, r#"{"text":"morning summary"}"#);
    assert_eq!(sent[1].4, r#"{"text":"second reply"}"#);
    assert_eq!(
        db.scalar::<String>("select principal from bot_outbox where conv_id = 'D9' limit 1")
            .await,
        OWNER
    );
    // Origin conversation holds only the approval card.
    assert_eq!(outbox_rows(&db, "c1").await, 1);
    assert_eq!(
        db.scalar::<String>("select transport from bot_outbox where conv_id = 'c1'")
            .await,
        "test-push"
    );
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_outbox where conv_id = 'D9' and transport = 'slack'"
        )
        .await,
        2
    );
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
    let (_, approval) = request_delivery(&db, &run, answers(&["first"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
    let again = StartCall {
        approval: start.approval.clone(),
        ..start_call(&attempt, DELIVER_CALL, None)
    };
    replies(&db).enqueue(start).await.unwrap();
    assert_eq!(outbox_rows(&db, "D9").await, 1);
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
    let (_, approval) = request_delivery(&db, &run, answers(&["draft reply"])).await;
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

    let mut missing = again(&start);
    missing.approval = None;
    assert!(matches!(
        replies(&db).enqueue(missing).await,
        Err(Error::Denied)
    ));
    nothing_happened(&db).await;

    // Recomputed digest over stored answers detects tampering.
    db.sql(r#"update bot_deliveries set answers = array['{"text":"tampered reply"}']"#)
        .await;
    assert!(matches!(
        replies(&db).enqueue(again(&start)).await,
        Err(Error::Conflict)
    ));
    nothing_happened(&db).await;
    db.sql(r#"update bot_deliveries set answers = array['{"text":"draft reply"}']"#)
        .await;

    // Changing the active set invalidates the draft pinned to the old mapping.
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
    for card in [
        card_for("n1", &document.replace("morning summary", "other summary")),
        card_for("n1", &document[..document.len() / 2]),
        JsonPayload(
            r#"{"approval":{"nonce":"n1","title":"Approval","lines":["Tool: schedule.deliver"]}}"#
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
    let (_, approval) = request_delivery(&db, &first, answers(&["first firing"])).await;
    let (_, first_start) = approved_start(&db, &approval).await;
    runs(&db)
        .accept(accept_as(
            "personal",
            "r2",
            OWNER,
            ConversationKind::Direct,
            "c1",
            "next",
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
            .draft(second.attempt.clone(), Some(answers(&["second firing"])))
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
    let stolen = StartCall {
        approval: first_start.approval,
        ..start_call(&second.attempt, DELIVER_CALL, None)
    };
    assert!(replies(&db).enqueue(stolen).await.is_err());
    assert_eq!(outbox_rows(&db, "D9").await, 0);
}
