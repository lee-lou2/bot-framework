//! 예약 회신(PG)의 조회 쪽: 원래 대화에 남는 것·소유자만 읽는 초안·마지막 claim만 읽는 등록 영수증.

mod support;

use bot_core::{admin::AdminRead, id::*, port::*, schedule::ScheduledReplies, state::*};
use support::{admin::*, delivery::*, *};

/// 승인 요청 뒤 원래 대화로 간 것과 조회자별로 보이는 것. 공개 채널(`public`)은 카드도 안내도 올리지 않는다: 카드는 소유자 DM으로 전달하는 작업으로 쌓이고,
/// 공개 안내는 DM 게시를 확인한 커밋의 한 줄뿐이다(`card_jobs.rs`).
async fn card_case(kind: ConversationKind, public: bool) {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = destination_run(&db, kind.clone()).await;
    let long = "가".repeat(400);
    let (draft, approval) = request_delivery(&db, &run, answers(&[&long, "둘째"])).await;
    let bodies: Vec<String> =
        sqlx::query_scalar("select body from bot_outbox where conv_id = 'c1' order by seq")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    if public {
        assert!(
            bodies.is_empty(),
            "{kind:?}: 공개 채널에는 아무것도 올리지 않는다: {bodies:?}"
        );
        assert_eq!(
            db.scalar::<i64>("select count(*) from bot_card_jobs").await,
            1,
            "{kind:?}: 카드는 소유자 DM 전달 작업으로 쌓인다"
        );
    } else {
        assert_eq!(bodies.len(), 1, "{kind:?}");
        assert!(
            bodies[0].contains("\"approval\""),
            "{kind:?}: 카드(버튼)는 소유자 대화에 간다"
        );
        assert!(bodies[0].contains(&long[..30]), "{kind:?}");
    }
    // 소유자는 긴 초안을 원문 한 행으로 받고, 운영자는 원문을 받지 못한다
    let seen = AdminRead::approval(&db.store, &owner(), approval.nonce.clone())
        .await
        .unwrap();
    assert_eq!(
        seen.render,
        vec![
            ("제목".to_owned(), "예약 회신 승인".to_owned()),
            ("전송 초안".to_owned(), draft.approval_json().unwrap())
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
    let (_, approval) = request_delivery(&db, &run, answers(&["하나", "둘"])).await;
    // 등록 전: 영수증이 없다(일반 원장 펜스가 판정한다)
    assert_eq!(
        replies(&db).receipt(run.attempt.clone()).await.unwrap(),
        None
    );
    let (attempt, start) = approved_start(&db, &approval).await;
    assert_eq!(replies(&db).receipt(attempt.clone()).await.unwrap(), None);
    replies(&db).enqueue(start).await.unwrap();
    // run이 끝나 리스를 놓은 뒤에도 마지막 claim은 등록 영수증만 읽는다(초안 본문·모델 결과는 아니다)
    let receipt = replies(&db)
        .receipt(attempt.clone())
        .await
        .unwrap()
        .expect("영수증");
    assert_eq!(receipt.0, r#"{"queued":2}"#);
    // 옛 claim·claim 없는 시도·옛 세대는 읽지 못한다
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
    // 이 run이 아닌 시도(회신이 없는 run)는 None이다
    let other = run_in(&db, "r2", OWNER, ConversationKind::Direct).await;
    assert_eq!(replies(&db).receipt(other.attempt).await.unwrap(), None);
    // 활성 집합이 바뀌면 옛 핸들·새 핸들 모두 옛 영수증을 읽지 못한다
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
