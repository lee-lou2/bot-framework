//! 목적지 ID는 어느 단계에서도 바뀌거나 거절되지 않는다: 슬랙 `팀ID/채널ID`처럼 "/"가 든 ID와 상한 길이 ID가 outbox의 대화 ID까지 그대로 간다.

mod support;

use bot_core::{port::AuditLog, state::ConversationKind};
use support::{delivery::*, *};

#[tokio::test]
async fn a_destination_id_reaches_the_outbox_exactly_as_it_was_sealed() {
    let long = format!("T1/{}", "C".repeat(253));
    assert_eq!(long.len(), 256);
    for dest in ["T123/C456", "T0AB-1/C9_x.y", long.as_str()] {
        // 실행마다 새 저장소로 돌린다(같은 승인 nonce·호출 ID를 쓰는 별개의 발화다)
        let Some(db) = TestDb::new().await else {
            return;
        };
        let run = destination_run_to(&db, "r1", ConversationKind::Direct, dest).await;
        let (draft, approval) = request_delivery(&db, &run, answers(&["아침 요약"])).await;
        assert!(
            !draft.approval_json().unwrap().is_empty(),
            "승인 문서가 있다"
        );
        let (attempt, start) = approved_start(&db, &approval).await;
        replies(&db).enqueue(start).await.unwrap();
        assert_eq!(state_of_run(&db, &attempt.run).await, "succeeded");
        let sent: Vec<(String, String, Option<String>)> =
            sqlx::query_as("select binding, conv_kind, thread from bot_outbox where conv_id = $1")
                .bind(dest)
                .fetch_all(db.store.pool())
                .await
                .unwrap();
        assert_eq!(
            sent,
            [("slack".into(), "direct".into(), Some("t1".into()))],
            "{dest}"
        );
        assert_eq!(db.store.verify().await.unwrap().first_bad, None);
    }
}
