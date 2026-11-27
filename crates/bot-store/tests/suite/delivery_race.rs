//! Regression test for delivery SHA race: `enqueue` must recompute the draft SHA-256 after taking the `FOR UPDATE` row lock.

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

/// Holds the `bot_deliveries` row lock until `enqueue` blocks on its second read, then applies `change` and commits.
async fn race(db: &TestDb, start: StartCall, change: &[&str]) -> Result<(), Error> {
    let mut locker = db.raw().await;
    let mut tx = locker.begin().await.unwrap();
    tx.execute("select 1 from bot_deliveries for update")
        .await
        .unwrap();
    let store = db.store.clone();
    let worker = tokio::spawn(async move { ScheduledReplies::enqueue(&store, start).await });
    // Wait deterministically until `enqueue` finishes its unlocked read and blocks on `FOR UPDATE`.
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
            "enqueue is not waiting on the delivery row lock"
        );
        assert!(
            !worker.is_finished(),
            "enqueue finished without blocking on lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for statement in change {
        tx.execute(*statement).await.unwrap();
    }
    tx.commit().await.unwrap();
    worker.await.unwrap()
}

async fn nothing_happened(db: &TestDb, nonce: &str, run: &RunId) {
    assert_eq!(
        outbox_rows(db, "D9").await,
        0,
        "destination outbox must be empty"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'delivery.enqueue'")
            .await,
        0,
        "enqueue audit must be 0"
    );
    assert_eq!(
        calls(db)
            .approval(ApprovalId(nonce.into()))
            .await
            .unwrap()
            .state,
        ApprovalState::Approved,
        "approval must remain unconsumed"
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

async fn approved(db: &TestDb) -> (DeliveryDraft, Approval, StartCall, RunId) {
    let run = destination_run(db, ConversationKind::Direct).await;
    let (draft, approval) = request_delivery(db, &run, answers(&["reply to send"])).await;
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
    // Mutate only answers; recomputed hash after lock no longer matches stored sha256.
    let result = race(
        &db,
        start,
        &[r#"update bot_deliveries set answers = array['{"text":"unapproved reply"}']"#],
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
    // Mutate both answers and sha256 coherently; must still fail against the hash sealed on the call.
    let other = DeliveryDraft {
        answers: vec![JsonPayload(r#"{"text":"unapproved reply"}"#.into())],
        ..replies(&db)
            .draft(approved_attempt(&db).await, None)
            .await
            .unwrap()
            .expect("frozen draft")
    };
    let hash = other.sha256().unwrap();
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    // Single UPDATE avoids PG rechecking the FK with FOR KEY SHARE on the parent run row, which would deadlock with enqueue's run lock.
    let change = format!(
        r#"update bot_deliveries set answers = array['{{"text":"unapproved reply"}}'], sha256 = decode('{hex}', 'hex')"#
    );
    let result = race(&db, start, &[&change]).await;
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    nothing_happened(&db, &approval.nonce.0, &run).await;
}

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
    // Flip one bit in stored sha256 while leaving answers intact to verify corruption detection.
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
