//! Unsent scheduled delivery completion (PG): `Unrouted` posts one fixed origin notice; `Rejected` requires a stored owner decision and posts no notice.

use crate::support::{delivery::*, *};
use bot_core::{
    id::*,
    ledger::{ApprovalState, Decision},
    port::{AuditLog, Error},
    schedule::{DELIVER_CALL, DeliveryFence, DeliveryTarget, UNROUTED_NOTICE, UnsentReason},
    state::{Conversation, ConversationKind},
};

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
        .draft(at.clone(), Some(answers(&["morning summary", "second"])))
        .await
        .unwrap()
        .expect("draft");
    assert_eq!(draft.target.conversation.id, "D9");
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
    assert_eq!(
        delivery_row(&db).await,
        ("dropped".into(), Some("unrouted".into()), true, true)
    );
    assert_eq!(outbox_rows(&db, "D9").await, 0);
    assert_eq!(
        bodies_to_original(&db).await,
        [format!(r#"{{"text":"{UNROUTED_NOTICE}"}}"#)]
    );
    assert_eq!(
        replies(&db).receipt(at.clone()).await.unwrap().unwrap().0,
        UNROUTED
    );
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
    let (draft, approval) = request_delivery(&db, &run, answers(&["morning summary"])).await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    let before = bodies_to_original(&db).await;
    assert_eq!(before.len(), 1);

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
        "open card must be superseded"
    );
    assert_eq!(bodies_to_original(&db).await.len(), 2, "1 card + 1 notice");
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
    let (draft, approval) = request_delivery(&db, &run, answers(&["morning summary"])).await;
    let (attempt, start) = approved_start(&db, &approval).await;
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
        "delivery call finishes with the unsent receipt"
    );
}

#[tokio::test]
async fn nothing_is_closed_for_another_recipient_a_stale_claim_a_missing_draft_or_a_sent_draft() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let at = run.attempt.clone();
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
        .draft(at.clone(), Some(answers(&["morning summary"])))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        finish(&db, claim(&at), target("D1"), UnsentReason::Unrouted).await,
        Err(Error::Conflict)
    ));
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
    let (draft, approval) = request_delivery(&db, &run, answers(&["morning summary"])).await;
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
        .draft(at.clone(), Some(answers(&["morning summary"])))
        .await
        .unwrap()
        .unwrap();
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
    let resumed = runs(&db).claim().await.unwrap().expect("woken run");
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
        "rejection posts no notice"
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
        .draft(at.clone(), Some(answers(&["morning summary"])))
        .await
        .unwrap()
        .unwrap();
    // After active set changes, draft() refuses stale drafts (Conflict) while target() stays readable for revocation checks.
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
    for (what, sql, actor, want_denied) in [
        (
            "actor is owner but card approver is someone else",
            "update bot_approvals set approver = 'local:other'",
            OWNER,
            true,
        ),
        (
            "card approver matches actor but actor is not schedule owner",
            "update bot_approvals set approver = 'local:other'",
            "local:other",
            true,
        ),
        (
            "card is no longer pending",
            "update bot_approvals set state = 'denied'",
            OWNER,
            false,
        ),
        (
            "card is expired",
            "update bot_approvals set expires_ms = bot_now_ms() - 1",
            OWNER,
            false,
        ),
    ] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let run = destination_run(&db, ConversationKind::Direct).await;
        let (draft, _) = request_delivery(&db, &run, answers(&["morning summary"])).await;
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

#[tokio::test]
async fn cancellation_before_unsent_completion_creates_no_notice() {
    use bot_core::port::RunAdmin;
    let Some(db) = TestDb::new().await else {
        return;
    };
    let at = destination_run(&db, ConversationKind::Direct).await.attempt;
    let draft = replies(&db)
        .draft(at.clone(), Some(answers(&["reply"])))
        .await
        .unwrap()
        .unwrap();
    db.store
        .cancel(
            at.profile.clone(),
            at.run.clone(),
            PrincipalId("operator".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    assert!(matches!(
        finish(&db, claim(&at), draft.target, UnsentReason::Unrouted).await,
        Err(Error::Stale)
    ));
    assert!(bodies_to_original(&db).await.is_empty());
    assert_eq!(delivery_row(&db).await.0, "drafted");
    assert_eq!(
        runs(&db).finish(at, done("late reply")).await.unwrap(),
        bot_core::command::Completion::Cancelled
    );
}
