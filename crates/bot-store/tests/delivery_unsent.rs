//! 보내지 않고 끝내는 전송 초안(PG): 수신자가 현재 등록부에서 철회됐거나(Unrouted) 소유자가 거절·철회했을 때(Rejected) 초안·승인·run을 한 커밋으로 마무리한다.
//! 안내는 Unrouted에만, 원래 대화에 고정 문장 한 건이다. 이미 등록된 답은 되돌리지 않고, 같은 종결의 재전달은 같은 영수증이다.

mod support;

use bot_core::{
    id::*,
    ledger::{ApprovalState, Decision},
    port::{AuditLog, Error},
    schedule::{DELIVER_CALL, DeliveryFence, DeliveryTarget, UNROUTED_NOTICE, UnsentReason},
    state::{Conversation, ConversationKind},
};
use support::{delivery::*, *};

const UNROUTED: &str = r#"{"status":"delivery_unrouted"}"#;
const REJECTED: &str = r#"{"status":"delivery_rejected"}"#;

fn target(id: &str) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: id.into(),
            thread: Some("t1".into()),
        },
    }
}

fn claim(at: &Attempt) -> DeliveryFence {
    DeliveryFence::Claim(at.clone())
}

fn card(nonce: &str, actor: &str) -> DeliveryFence {
    DeliveryFence::Approval {
        nonce: ApprovalId(nonce.into()),
        actor: PrincipalId(actor.into()),
    }
}

async fn finish(
    db: &TestDb,
    fence: DeliveryFence,
    to: DeliveryTarget,
    reason: UnsentReason,
) -> Result<String, Error> {
    replies(db)
        .finish_unsent(fence, to, reason)
        .await
        .map(|receipt| receipt.0)
}

/// 원래 대화(c1)로 간 회신 본문들(순서대로).
async fn bodies_to_original(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar("select body from bot_outbox where conv_id = 'c1' order by seq")
        .fetch_all(db.store.pool())
        .await
        .unwrap()
}

async fn delivery_row(db: &TestDb) -> (String, Option<String>, bool, bool) {
    sqlx::query_as("select state, unsent, result is null, answers is null from bot_deliveries")
        .fetch_one(db.store.pool())
        .await
        .unwrap()
}

#[tokio::test]
async fn an_unrouted_draft_is_closed_from_the_live_claim_with_one_fixed_notice_to_the_original_conversation()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
    let draft = replies(&db)
        .draft(at.clone(), Some(answers(&["아침 요약", "둘째"])))
        .await
        .unwrap()
        .expect("초안");
    assert_eq!(draft.target.conversation.id, "D9");
    // 수신자만 읽는다(본문 없음): 현재 claim에서
    let seen = replies(&db).target(at.clone()).await.unwrap().unwrap();
    assert_eq!(
        (seen.binding.0.as_str(), seen.conversation.id.as_str()),
        ("slack", "D9")
    );

    let receipt = finish(
        &db,
        claim(&at),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await
    .unwrap();
    assert_eq!(receipt, UNROUTED);
    assert_eq!(state_of_run(&db, &at.run).await, "succeeded");
    assert_eq!(
        db.scalar::<String>("select result from bot_runs").await,
        UNROUTED
    );
    // 초안 본문은 지우고 종결 사유만 남는다. 목적지로는 아무것도 가지 않는다
    assert_eq!(
        delivery_row(&db).await,
        ("dropped".into(), Some("unrouted".into()), true, true)
    );
    assert_eq!(outbox_rows(&db, "D9").await, 0);
    // 원래 대화에는 고정 안내 한 건뿐이다(초안·수신자·지시문 없음)
    assert_eq!(
        bodies_to_original(&db).await,
        [format!(r#"{{"text":"{UNROUTED_NOTICE}"}}"#)]
    );
    // 미발송 영수증은 마지막 claim에 같은 바이트로 돌려준다
    assert_eq!(
        replies(&db).receipt(at.clone()).await.unwrap().unwrap().0,
        UNROUTED
    );
    // 같은 종결의 재전달은 같은 영수증이고 안내를 더 쌓지 않는다
    assert_eq!(
        finish(
            &db,
            claim(&at),
            draft.target.clone(),
            UnsentReason::Unrouted
        )
        .await
        .unwrap(),
        UNROUTED
    );
    assert_eq!(bodies_to_original(&db).await.len(), 1);
    // 마지막 claim이 아닌 시도는 재전달이어도 영수증을 받지 못한다
    let mut stale = at.clone();
    stale.claim = stale.claim.map(|c| ClaimEpoch(c.0 + 1));
    assert!(matches!(
        finish(
            &db,
            claim(&stale),
            draft.target.clone(),
            UnsentReason::Unrouted
        )
        .await,
        Err(Error::Stale)
    ));
    // 다른 사유로 다시 닫을 수는 없다
    assert!(matches!(
        finish(
            &db,
            claim(&at),
            draft.target.clone(),
            UnsentReason::Rejected
        )
        .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.unsent'")
            .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn a_waiting_card_is_closed_only_by_the_authenticated_owner_and_can_no_longer_be_approved() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (draft, approval) = request_delivery(&db, &run, answers(&["아침 요약"])).await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    let before = bodies_to_original(&db).await; // 승인 카드 한 건
    assert_eq!(before.len(), 1);

    // 소유자가 아니거나 다른 카드면 아무것도 바뀌지 않는다
    let other = finish(
        &db,
        card("n1", "local:other"),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await;
    assert!(matches!(other, Err(Error::Denied)), "{other:?}");
    let unknown = finish(
        &db,
        card("nope", OWNER),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await;
    assert!(matches!(unknown, Err(Error::Missing)), "{unknown:?}");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(delivery_row(&db).await.0, "drafted");

    let receipt = finish(
        &db,
        card("n1", OWNER),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await
    .unwrap();
    assert_eq!(receipt, UNROUTED);
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
    assert_eq!(
        calls(&db)
            .approval(approval.nonce.clone())
            .await
            .unwrap()
            .state,
        ApprovalState::Superseded,
        "열린 카드는 무효가 된다"
    );
    assert_eq!(
        bodies_to_original(&db).await.len(),
        2,
        "카드 한 건 + 안내 한 건"
    );
    // 무효가 된 카드로는 승인도 전송도 할 수 없다
    assert!(
        calls(&db)
            .decide(Decision {
                nonce: approval.nonce.clone(),
                actor: PrincipalId(OWNER.into()),
                allow: true,
            })
            .await
            .is_err()
    );
    assert_eq!(outbox_rows(&db, "D9").await, 0);
    // 같은 카드로 재전달하면 같은 영수증이다. 다른 카드로는 안 된다
    assert_eq!(
        finish(
            &db,
            card("n1", OWNER),
            draft.target.clone(),
            UnsentReason::Unrouted
        )
        .await
        .unwrap(),
        UNROUTED
    );
    assert_eq!(bodies_to_original(&db).await.len(), 2);
    // 소유자가 아닌 사람은 재전달로도 영수증을 받지 못한다
    let other = finish(
        &db,
        card("n1", "local:other"),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await;
    assert!(matches!(other, Err(Error::Denied)), "{other:?}");
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn an_approved_but_unconsumed_draft_is_closed_from_the_resumed_claim_and_never_sent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (draft, approval) = request_delivery(&db, &run, answers(&["아침 요약"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
    // 승인은 났지만 소비 직전에 수신자가 철회됐다
    finish(
        &db,
        claim(&attempt),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await
    .unwrap();
    assert_eq!(state_of_run(&db, &attempt.run).await, "succeeded");
    assert_eq!(
        calls(&db)
            .approval(approval.nonce.clone())
            .await
            .unwrap()
            .state,
        ApprovalState::Superseded
    );
    assert!(replies(&db).enqueue(start).await.is_err());
    assert_eq!(outbox_rows(&db, "D9").await, 0);
    assert_eq!(
        db.scalar::<String>(&format!(
            "select state from bot_calls where call = '{DELIVER_CALL}'"
        ))
        .await,
        "done",
        "전송 호출은 영수증으로 끝난다"
    );
}

#[tokio::test]
async fn nothing_is_closed_for_another_recipient_a_stale_claim_a_missing_draft_or_a_sent_draft() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
    // 초안이 없으면 닫을 것이 없다
    assert!(matches!(
        finish(&db, claim(&at), target("D9"), UnsentReason::Unrouted).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        replies(&db)
            .target(at.clone())
            .await
            .unwrap()
            .map(|t| t.conversation.id),
        None
    );
    let draft = replies(&db)
        .draft(at.clone(), Some(answers(&["아침 요약"])))
        .await
        .unwrap()
        .unwrap();
    // 보관한 수신자와 다르면 닫지 않는다
    assert!(matches!(
        finish(&db, claim(&at), target("D1"), UnsentReason::Unrouted).await,
        Err(Error::Conflict)
    ));
    // 옛 claim(다른 세대·claim)은 거절된다
    let mut stale = at.clone();
    stale.claim = stale.claim.map(|c| ClaimEpoch(c.0 + 1));
    assert!(matches!(
        finish(
            &db,
            claim(&stale),
            draft.target.clone(),
            UnsentReason::Unrouted
        )
        .await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        replies(&db).target(stale).await,
        Err(Error::Stale)
    ));
    assert_eq!(state_of_run(&db, &at.run).await, "leased");
    assert_eq!(delivery_row(&db).await.0, "drafted");
    assert_eq!(bodies_to_original(&db).await.len(), 0);
}

#[tokio::test]
async fn a_registered_answer_cannot_be_unsent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (draft, approval) = request_delivery(&db, &run, answers(&["아침 요약"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
    replies(&db).enqueue(start).await.unwrap();
    assert_eq!(outbox_rows(&db, "D9").await, 1);
    for fence in [claim(&attempt), card("n1", OWNER)] {
        let got = finish(&db, fence, draft.target.clone(), UnsentReason::Unrouted).await;
        assert!(
            matches!(got, Err(Error::Conflict | Error::Stale)),
            "{got:?}"
        );
    }
    assert_eq!(delivery_row(&db).await.0, "enqueued");
    assert_eq!(outbox_rows(&db, "D9").await, 1);
}

#[tokio::test]
async fn a_rejection_needs_the_owners_own_stored_decision_and_leaves_no_notice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
    let draft = replies(&db)
        .draft(at.clone(), Some(answers(&["아침 요약"])))
        .await
        .unwrap()
        .unwrap();
    // 저장된 소유자의 결정이 없으면 거절로 닫을 수 없다
    assert!(matches!(
        finish(
            &db,
            claim(&at),
            draft.target.clone(),
            UnsentReason::Rejected
        )
        .await,
        Err(Error::Denied)
    ));
    assert_eq!(delivery_row(&db).await.0, "drafted");
    // 카드를 열고 소유자가 거절한다
    let hash = draft.sha256().unwrap();
    calls(&db)
        .begin(
            at.clone(),
            call(DELIVER_CALL),
            tool(bot_core::schedule::DELIVER_TOOL),
            hash,
        )
        .await
        .unwrap();
    let approval = delivery_approval(&at, "n1", hash);
    calls(&db)
        .open_approval(open_delivery(&approval, &draft.approval_json().unwrap()))
        .await
        .unwrap();
    calls(&db)
        .decide(Decision {
            nonce: approval.nonce.clone(),
            actor: PrincipalId(OWNER.into()),
            allow: false,
        })
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().expect("깨어난 run");
    let before = bodies_to_original(&db).await.len();
    let receipt = finish(
        &db,
        claim(&resumed.attempt),
        draft.target.clone(),
        UnsentReason::Rejected,
    )
    .await
    .unwrap();
    assert_eq!(receipt, REJECTED);
    assert_eq!(state_of_run(&db, &at.run).await, "succeeded");
    assert_eq!(
        delivery_row(&db).await,
        ("dropped".into(), Some("rejected".into()), true, true)
    );
    assert_eq!(
        bodies_to_original(&db).await.len(),
        before,
        "거절에는 안내가 없다"
    );
    assert_eq!(outbox_rows(&db, "D9").await, 0);
    assert_eq!(
        replies(&db)
            .receipt(resumed.attempt.clone())
            .await
            .unwrap()
            .unwrap()
            .0,
        REJECTED
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn the_recipient_is_readable_after_the_host_set_changed_but_a_stale_draft_is_not_returned() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
    let draft = replies(&db)
        .draft(at.clone(), Some(answers(&["아침 요약"])))
        .await
        .unwrap()
        .unwrap();
    // 호스트 활성 집합이 바뀌었다: 옛 집합에서 동결한 초안은 더는 읽히지 않지만(Conflict) 수신자는 읽어 철회를 가릴 수 있다
    db.sql("update bot_active set revision = 'set-2' where singleton")
        .await;
    assert!(matches!(
        replies(&db).draft(at.clone(), None).await,
        Err(Error::Conflict)
    ));
    let seen = replies(&db).target(at.clone()).await.unwrap().unwrap();
    assert_eq!(seen.conversation.id, "D9");
    let receipt = finish(
        &db,
        claim(&at),
        draft.target.clone(),
        UnsentReason::Unrouted,
    )
    .await
    .unwrap();
    assert_eq!(receipt, UNROUTED);
    // 종결 당시 집합과 지금 집합이 다르면 영수증을 읽을 수 없다(Stale)
    assert_eq!(
        replies(&db).receipt(at.clone()).await.unwrap().unwrap().0,
        UNROUTED
    );
    db.sql("update bot_active set revision = 'set-3' where singleton")
        .await;
    assert!(matches!(replies(&db).receipt(at).await, Err(Error::Stale)));
}

#[tokio::test]
async fn a_card_that_is_not_the_owners_live_pending_card_is_never_closed() {
    // 저장된 카드가 소유자 본인의 살아 있는 대기 카드가 아니면(승인자·상태·만료 중 하나라도 어긋나면) 닫지 않는다. 검사마다 따로 어긋나게 만든다.
    for (what, sql, actor, want_denied) in [
        (
            "행위자는 소유자지만 카드의 승인자는 다른 사람이다",
            "update bot_approvals set approver = 'local:other'",
            OWNER,
            true,
        ),
        (
            "카드의 승인자와 행위자는 같지만 예약 소유자가 아니다",
            "update bot_approvals set approver = 'local:other'",
            "local:other",
            true,
        ),
        (
            "카드가 더는 대기 상태가 아니다",
            "update bot_approvals set state = 'denied'",
            OWNER,
            false,
        ),
        (
            "카드가 만료됐다",
            "update bot_approvals set expires_ms = bot_now_ms() - 1",
            OWNER,
            false,
        ),
    ] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let run = destination_run(&db, ConversationKind::Direct).await;
        let (draft, _) = request_delivery(&db, &run, answers(&["아침 요약"])).await;
        db.sql(sql).await;
        let got = finish(
            &db,
            card("n1", actor),
            draft.target.clone(),
            UnsentReason::Unrouted,
        )
        .await;
        if want_denied {
            assert!(matches!(got, Err(Error::Denied)), "{what}: {got:?}");
        } else {
            assert!(matches!(got, Err(Error::Stale)), "{what}: {got:?}");
        }
        assert_eq!(delivery_row(&db).await.0, "drafted", "{what}");
        assert_eq!(
            state_of_run(&db, &run.attempt.run).await,
            "waiting",
            "{what}"
        );
    }
}
