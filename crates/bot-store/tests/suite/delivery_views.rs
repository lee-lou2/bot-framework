//! Scheduled delivery views: channel vs DM card routing, owner-only draft visibility, and final-claim receipt fencing.

use crate::support::{admin::*, delivery::*, *};
use bot_core::{
    admin::AdminRead,
    cards::LABEL_TITLE,
    id::*,
    port::*,
    schedule::{DELIVERY_CARD_LABEL, ScheduledReplies},
    state::*,
};

/// Public channels stage a DM card job without posting publicly; DMs enqueue the card directly.
async fn card_case(kind: ConversationKind, public: bool) {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, kind.clone()).await;
    let long = "€".repeat(400);
    let (draft, approval) = request_delivery(&db, &run, answers(&[&long, "second"])).await;
    let bodies: Vec<String> =
        sqlx::query_scalar("select body from bot_outbox where conv_id = 'c1' order by seq")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    if public {
        assert!(
            bodies.is_empty(),
            "{kind:?}: public channel must receive nothing directly: {bodies:?}"
        );
        assert_eq!(
            db.scalar::<i64>("select count(*) from bot_card_jobs").await,
            1,
            "{kind:?}: card must be staged for owner DM"
        );
    } else {
        assert_eq!(bodies.len(), 1, "{kind:?}");
        assert!(
            bodies[0].contains("\"approval\""),
            "{kind:?}: card must be posted to owner DM"
        );
        assert!(bodies[0].contains(&long[..30]), "{kind:?}");
    }
    let seen = AdminRead::approval(&db.store, &owner(), approval.nonce.clone())
        .await
        .unwrap();
    assert_eq!(
        seen.render,
        vec![
            (
                LABEL_TITLE.to_owned(),
                "Scheduled delivery approval".to_owned(),
            ),
            (
                DELIVERY_CARD_LABEL.to_owned(),
                draft.approval_json().unwrap(),
            ),
        ],
        "{kind:?}"
    );
    let operator_view = AdminRead::approval(&db.store, &operator(), approval.nonce.clone())
        .await
        .unwrap();
    assert!(operator_view.render.is_empty(), "{kind:?}");
    assert_eq!(operator_view.args_sha256, draft.sha256().unwrap());
}

#[tokio::test]
async fn a_public_channel_gets_neither_card_nor_notice_and_only_the_owner_reads_the_draft() {
    card_case(ConversationKind::Channel, true).await;
    card_case(ConversationKind::Direct, false).await;
}

#[tokio::test]
async fn the_receipt_is_readable_only_by_the_final_claim_under_the_registered_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, ConversationKind::Direct).await;
    let (_, approval) = request_delivery(&db, &run, answers(&["one", "two"])).await;
    assert_eq!(
        replies(&db).receipt(run.attempt.clone()).await.unwrap(),
        None
    );
    let (attempt, start) = approved_start(&db, &approval).await;
    assert_eq!(replies(&db).receipt(attempt.clone()).await.unwrap(), None);
    replies(&db).enqueue(start).await.unwrap();
    let receipt = replies(&db)
        .receipt(attempt.clone())
        .await
        .unwrap()
        .expect("receipt");
    assert_eq!(receipt.0, r#"{"queued":2}"#);
    for stale in [
        run.attempt.clone(),
        {
            let mut a = attempt.clone();
            a.claim = None;
            a
        },
        {
            let mut a = attempt.clone();
            a.generation = Generation(a.generation.0 + 1);
            a
        },
    ] {
        assert!(matches!(
            replies(&db).receipt(stale).await,
            Err(Error::Stale)
        ));
    }
    let other = run_in(&db, "r2", OWNER, ConversationKind::Direct).await;
    assert_eq!(replies(&db).receipt(other.attempt).await.unwrap(), None);
    let pinned = db.store.for_activation(Revision("set-1".into()));
    assert!(
        ScheduledReplies::receipt(&pinned, attempt.clone())
            .await
            .is_ok()
    );
    db.sql("update bot_active set revision = 'set-2'").await;
    for handle in [
        db.store.clone(),
        pinned,
        db.store.for_activation(Revision("set-2".into())),
    ] {
        assert!(matches!(
            ScheduledReplies::receipt(&handle, attempt.clone()).await,
            Err(Error::Stale)
        ));
    }
}
