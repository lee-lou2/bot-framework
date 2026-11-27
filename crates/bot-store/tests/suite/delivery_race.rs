//! 전송 SHA 경합 회귀: `enqueue`는 동결한 초안을 **두 번** 읽는다 — 처음에는 잠그지 않고 읽어 호출에 봉인한 지문과 겨룰 값을 만들고, 호출·승인을 잠근 뒤
//! 행 잠금(`for update`)으로 다시 읽는다. 두 번째 읽기를 기다리는 동안 별도 트랜잭션이 답을 바꿔 커밋하면, 잠금 뒤에 **저장된 바이트로 지문을 다시 계산해
//! 처음 값과 겨뤄야** 승인하지 않은 내용을 보내지 않는다(잠금 뒤 재계산을 빼면 바뀐 답이 승인된 호출로 나간다).
//!
//! 기대: `Conflict`, 목적지 outbox 0, 등록 감사 0, 승인 미소비, 호출·초안·run은 그대로(재시도가 새 판단을 받는다).
//! 경합은 시간 추정 없이 만든다: 별도 연결이 초안 행 잠금을 쥔 채로 `enqueue`를 띄우고, 그 `enqueue`가 `pg_stat_activity`에서 잠금 대기로 보일 때 답을 바꿔 커밋한다.

use crate::support::{delivery::*, *};
use bot_core::{
    command::*,
    id::*,
    ledger::*,
    port::*,
    schedule::{DeliveryDraft, ScheduledReplies},
    state::*,
};
use sqlx::{Connection, Executor};
use std::time::Duration;

/// 별도 연결이 초안 행 잠금을 쥐고, 그 사이 `enqueue`가 두 번째 읽기에서 그 잠금을 기다리게 한다. `change`는 잠금을 쥔 트랜잭션 안에서 초안을 바꾸는 문장들이다.
async fn race(db: &TestDb, start: StartCall, change: &[&str]) -> Result<(), Error> {
    let mut locker = db.raw().await;
    let mut tx = locker.begin().await.unwrap();
    tx.execute("select 1 from bot_deliveries for update")
        .await
        .unwrap();
    let store = db.store.clone();
    let worker = tokio::spawn(async move { ScheduledReplies::enqueue(&store, start).await });
    // `enqueue`가 첫 읽기를 마치고 행 잠금을 기다리는 순간까지 기다린다(다른 이유의 잠금 대기를 세지 않도록 스키마와 질의 문장으로 가른다)
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = db
            .scalar(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_deliveries%for update%'",
                db.schema
            ))
            .await;
        if waiting == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "enqueue가 초안 행 잠금을 기다리지 않는다"
        );
        assert!(!worker.is_finished(), "잠금을 기다리지 않고 끝났다");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for statement in change {
        tx.execute(*statement).await.unwrap();
    }
    tx.commit().await.unwrap();
    worker.await.unwrap()
}

/// 아무것도 보내지도 소비하지도 기록하지도 않았다.
async fn nothing_happened(db: &TestDb, nonce: &str, run: &RunId) {
    assert_eq!(outbox_rows(db, "D9").await, 0, "목적지 outbox 0");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.enqueue'")
            .await,
        0,
        "등록 감사 0"
    );
    assert_eq!(
        calls(db)
            .approval(ApprovalId(nonce.into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Approved,
        "승인 미소비"
    );
    assert_eq!(
        db.scalar::<String>("select state from bot_deliveries")
            .await,
        "drafted"
    );
    assert_ne!(
        db.scalar::<String>("select state from bot_calls where call = 'schedule-deliver'")
            .await,
        "done"
    );
    assert_eq!(state_of_run(db, run).await, "leased");
}

/// 승인까지 끝낸 초안 하나와 그 `enqueue` 인자.
async fn approved(db: &TestDb) -> (DeliveryDraft, Approval, StartCall, RunId) {
    let run = destination_run(db, ConversationKind::Direct).await;
    let (draft, approval) = request_delivery(db, &run, answers(&["보낼 답"])).await;
    let (attempt, start) = approved_start(db, &approval).await;
    (draft, approval, start, attempt.run)
}

#[tokio::test]
async fn an_answer_changed_while_enqueue_waits_for_the_row_lock_is_refused_when_the_stored_hash_no_longer_matches()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, approval, start, run) = approved(&db).await;
    // 답만 바뀌었다(저장된 지문 열은 그대로): 잠금 뒤 저장된 바이트로 다시 계산한 지문이 저장된 지문과 어긋난다
    let result = race(
        &db,
        start,
        &[r#"update bot_deliveries set answers = array['{"text":"승인하지 않은 답"}']"#],
    )
    .await;
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    nothing_happened(&db, &approval.nonce.0, &run).await;
}

#[tokio::test]
async fn a_coherent_replacement_committed_while_enqueue_waits_is_refused_because_it_is_not_what_the_call_sealed()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, approval, start, run) = approved(&db).await;
    // 답과 저장된 지문 열이 함께 바뀌어 안쪽으로는 일관된 다른 초안이다: 호출에 봉인한(첫 읽기로 만든) 지문과 겨뤄야만 막을 수 있다
    let other = DeliveryDraft {
        answers: vec![JsonPayload(r#"{"text":"승인하지 않은 답"}"#.into())],
        ..replies(&db)
            .draft(approved_attempt(&db).await, None)
            .await
            .unwrap()
            .expect("동결한 초안")
    };
    let hash = other.sha256().unwrap();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    // 한 문장으로 바꾼다: 같은 트랜잭션에서 같은 행을 두 번 고치면 두 번째 문장이 부모 run 행에 키 공유 잠금을 요구해(자기 트랜잭션이 만든 행 버전이라 외래 키를 다시 확인한다)
    // `enqueue`가 쥔 run 잠금과 서로 기다리는 교착이 된다. 시험의 장치일 뿐 저장소 동작이 아니다.
    let change = format!(
        r#"update bot_deliveries set answers = array['{{"text":"승인하지 않은 답"}}'], sha256 = decode('{hex}', 'hex')"#
    );
    let result = race(&db, start, &[&change]).await;
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    nothing_happened(&db, &approval.nonce.0, &run).await;
}

/// 재개한 시도(동결한 초안을 읽는 데 쓴다).
async fn approved_attempt(db: &TestDb) -> Attempt {
    let run: String = db.scalar("select run from bot_deliveries").await;
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.clone()),
        generation: Generation(
            db.scalar::<i64>(&format!(
                "select generation from bot_runs where id = '{run}'"
            ))
            .await as u64,
        ),
        claim: Some(ClaimEpoch(
            db.scalar::<i64>(&format!(
                "select claim_epoch from bot_runs where id = '{run}'"
            ))
            .await as u64,
        )),
    }
}

#[tokio::test]
async fn a_stored_hash_corrupted_by_one_bit_while_enqueue_waits_is_refused_though_the_approved_text_is_unchanged()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, approval, start, run) = approved(&db).await;
    // 승인한 본문(답)은 그대로 두고 저장된 지문 열만 한 비트 뒤집는다: 보낼 바이트는 승인한 것과 같아도 저장된 지문이 손상됐으면 보내지 않는다(손상 감지).
    // 저장된 지문과 저장된 바이트에서 다시 계산한 지문을 겨루는 검사를 빼면 이 시험이 실패한다.
    let result = race(
        &db,
        start,
        &["update bot_deliveries set sha256 = set_bit(sha256, 0, 1 - get_bit(sha256, 0))"],
    )
    .await;
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    nothing_happened(&db, &approval.nonce.0, &run).await;
}

#[tokio::test]
async fn without_a_concurrent_change_the_same_enqueue_sends_exactly_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, approval, start, _) = approved(&db).await;
    // 같은 경합 장치에서 아무것도 바꾸지 않으면(잠금만 쥐었다 놓는다) 정상으로 보낸다: 위 시험이 잡는 것은 «바뀐 초안»이지 잠금 대기가 아니다
    race(&db, start, &[]).await.unwrap();
    assert_eq!(outbox_rows(&db, "D9").await, 1);
    assert_eq!(
        calls(&db)
            .approval(approval.nonce.clone())
            .await
            .unwrap()
            .state,
        ApprovalState::Consumed
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.enqueue'")
            .await,
        1
    );
}
