//! Destination IDs (including Slack `team/channel` with `/` and 256-byte max length) reach the outbox verbatim.

use crate::support::{delivery::*, *};
use bot_core::{port::AuditLog, state::ConversationKind};

#[tokio::test]
async fn a_destination_id_reaches_the_outbox_exactly_as_it_was_sealed() {
    let long = format!("T1/{}", "C".repeat(253));
    assert_eq!(long.len(), 256);
    for dest in ["T123/C456", "T0AB-1/C9_x.y", long.as_str()] {
        // Use a fresh store per case because each reuses the same approval nonce and call ID.
        let Some(db) = TestDb::new().await else {
            return;
        };
        let run = destination_run_to(&db, "r1", ConversationKind::Direct, dest).await;
        let (draft, approval) = request_delivery(&db, &run, answers(&["morning summary"])).await;
        assert!(
            !draft.approval_json().unwrap().is_empty(),
            "approval document must be non-empty"
        );
        let (attempt, start) = approved_start(&db, &approval).await;
        replies(&db).enqueue(start).await.unwrap();
        assert_eq!(state_of_run(&db, &attempt.run).await, "succeeded");
        let sent: Vec<(String, String, Option<String>, String)> = sqlx::query_as(
            "select binding, conv_kind, thread, transport from bot_outbox where conv_id = $1",
        )
        .bind(dest)
        .fetch_all(db.store.pool())
        .await
        .unwrap();
        assert_eq!(
            sent,
            [(
                "slack".into(),
                "direct".into(),
                Some("t1".into()),
                "slack".into()
            )],
            "{dest}"
        );
        assert_eq!(db.store.verify().await.unwrap().first_bad, None);
    }
}
