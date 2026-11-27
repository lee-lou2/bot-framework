//! Owner-DM approval card delivery failure tests (PG): `Unavailable`, `Held`, stale `sending` sweep, retry exhaustion, and attempt fencing.

use crate::support::{cards::*, delivery::*, schedule::*, *};
use bot_core::{
    api::ApprovalBlock,
    cards::{CardKey, CardOutbox, CardOutcome, DM_UNAVAILABLE_NOTICE, DM_UNCONFIRMED_NOTICE},
    ledger::ApprovalState,
};
use bot_core::{id::*, port::*, state::*};
use std::time::Duration;

const UNSENT: &str = r#"{"status":"approval_dm_unavailable"}"#;

/// Asserts single-closure invariants: system-denied approval, closed card job, one audit entry, one fixed channel notice, and late clicks/settles rejected.
async fn closed_once(
    db: &TestDb,
    run: &RunContext,
    approval: &Approval,
    key: &CardKey,
    reason: &str,
) {
    let notice = if reason == "unavailable" {
        DM_UNAVAILABLE_NOTICE
    } else {
        DM_UNCONFIRMED_NOTICE
    };
    assert_eq!(
        public_lines(db, run).await,
        vec![serde_json::json!({ "text": notice }).to_string()]
    );
    assert_eq!(
        db.scalar::<String>("select state || '|' || decided_by from bot_approvals")
            .await,
        "denied|system"
    );
    assert_eq!(
        db.scalar::<String>("select state || '|' || closed_reason from bot_card_jobs")
            .await,
        format!("closed|{reason}")
    );
    assert_eq!(audits(db, "approval.dm_unavailable").await, 1);
    assert_eq!(
        db.scalar::<String>(
            "select target || '|' || verdict from bot_audit where action = 'approval.dm_unavailable'"
        )
        .await,
        format!("{}|approval_dm_unavailable", approval.nonce.0)
    );
    assert!(decide_as_owner(db, approval).await.is_err());
    assert!(matches!(
        settle(&db.store, key, posted()).await,
        Err(Error::Conflict)
    ));
    assert_eq!(receipt(db, &approval.nonce).await, None);
}

#[tokio::test]
async fn an_unavailable_dm_ends_the_send_as_unsent_once_and_keeps_the_recurring_schedule() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let origin = run_in(&db, "r0", OWNER, ConversationKind::Direct).await;
    let schedule = make(&db, &origin, "c0", spec_cron("0 9 * * *", "Asia/Seoul"))
        .await
        .unwrap();
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    settle(&db.store, &job.key, CardOutcome::Unavailable)
        .await
        .unwrap();

    closed_once(&db, &run, &approval, &job.key, "unavailable").await;
    // Occurrence finishes unsent with identical receipt across run, call, and delivery; draft answers are cleared.
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
    let result = format!(
        "select result from bot_runs where id = '{}'",
        run.attempt.run.0
    );
    assert_eq!(db.scalar::<String>(&result).await, UNSENT);
    assert_eq!(
        db.scalar::<String>(
            "select state || '|' || result from bot_calls where call = 'schedule-deliver'"
        )
        .await,
        format!("done|{UNSENT}")
    );
    assert_eq!(
        db.scalar::<String>(
            "select state || '|' || unsent || '|' || unsent_receipt from bot_deliveries"
        )
        .await,
        format!("dropped|dm_unavailable|{UNSENT}")
    );
    assert!(
        db.scalar::<bool>("select answers is null from bot_deliveries")
            .await
    );
    let receipt = replies(&db).receipt(run.attempt.clone()).await.unwrap();
    assert_eq!(receipt.map(|r| r.0), Some(UNSENT.to_owned()));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox where conv_id = 'D9'")
            .await,
        0
    );
    assert_eq!(column::<String>(&db, &schedule, "state").await, "active");
    // Redelivering settlement is idempotent and must not duplicate audit or channel notice.
    for again in [CardOutcome::Unavailable, CardOutcome::Held] {
        settle(&db.store, &job.key, again).await.unwrap();
    }
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 1);
    assert_eq!(
        public_lines(&db, &run).await.len(),
        1,
        "notice must be posted once even if redelivered with another failure outcome"
    );
    assert_eq!(db.scalar::<String>(&result).await, UNSENT);
}

#[tokio::test]
async fn an_unavailable_dm_closes_a_create_card_without_a_schedule_and_wakes_the_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_create(&db, "r1", "n1").await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let job = claim(&pinned, 8).await.unwrap().remove(0);
    begin(&pinned, &job.key, rev()).await.unwrap();
    settle(&pinned, &job.key, CardOutcome::Held).await.unwrap();

    closed_once(&db, &run, &approval, &job.key, "held").await;
    // No schedule is created; the waiting run wakes on the same generation.
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
}

#[tokio::test]
async fn a_post_with_an_unknown_outcome_is_closed_after_two_minutes_and_never_reposted() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    // Under 2 minutes: remains sending.
    db.sql("update bot_card_jobs set sending_ms = bot_now_ms() - 100000")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "sending");
    // Over 2 minutes: outcome is uncertain, so sweep closes it without reposting.
    db.sql("update bot_card_jobs set sending_ms = bot_now_ms() - 130000")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    closed_once(&db, &run, &approval, &job.key, "held").await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(receipt(&db, &job.key.nonce).await, None);
}

#[tokio::test]
async fn transient_failures_back_off_and_the_sixth_attempt_closes_the_card() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "r1", "n1").await;
    // Default backoff prevents immediate reclaim.
    let job = sending(&db).await;
    settle(&db.store, &job.key, not_sent(None)).await.unwrap();
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "staged");
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    db.sql("update bot_card_jobs set not_before_ms = 0").await;
    for attempt in 2..=5 {
        let job = sending(&db).await;
        assert_eq!(
            db.scalar::<i32>("select attempts from bot_card_jobs").await,
            attempt
        );
        settle(&db.store, &job.key, not_sent(Some(Duration::ZERO)))
            .await
            .unwrap();
        assert_eq!(state(&db, "bot_card_jobs", "n1").await, "staged");
    }
    // Sixth failed attempt closes the card as unavailable.
    let job = sending(&db).await;
    settle(&db.store, &job.key, not_sent(Some(Duration::ZERO)))
        .await
        .unwrap();
    closed_once(&db, &run, &approval, &job.key, "unavailable").await;
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "succeeded");
}

#[tokio::test]
async fn an_abandoned_lease_is_claimed_again_but_a_job_that_began_is_not() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (_, _) = channel_card(&db, "r1", "n1").await;
    let first = claim(&db.store, 8).await.unwrap();
    assert_eq!(first.len(), 1);
    // Lease expiry before begin allows another sender to reclaim.
    db.sql("update bot_card_jobs set lease_until_ms = bot_now_ms() - 1")
        .await;
    let second = claim(&db.store, 8).await.unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(
        db.scalar::<i32>("select attempts from bot_card_jobs").await,
        2
    );
    begin(&db.store, &second[0].key, rev()).await.unwrap();
    // Once begin commits, lease expiry never reclaims (delivery may already be in flight).
    db.sql("update bot_card_jobs set lease_until_ms = bot_now_ms() - 1")
        .await;
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "sending");
}

#[tokio::test]
async fn a_failure_report_for_a_card_that_is_no_longer_open_only_closes_the_job() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, _) = channel_card(&db, "r1", "n1").await;
    let job = sending(&db).await;
    // If the approval expired while sending, failure settlement only marks the job stale without touching approval or run.
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    settle(&db.store, &job.key, CardOutcome::Unavailable)
        .await
        .unwrap();
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "closed");
    assert_eq!(
        db.scalar::<String>("select closed_reason from bot_card_jobs")
            .await,
        "stale"
    );
    assert_eq!(state(&db, "bot_approvals", "n1").await, "pending");
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    assert_eq!(audits(&db, "approval.dm_unavailable").await, 0);
    assert!(public_lines(&db, &run).await.is_empty());
}

#[tokio::test]
async fn late_settlement_cannot_reset_a_new_card_send() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, approval) = channel_card(&db, "retry", "n1").await;
    let first = sending(&db).await;
    let not_sent = not_sent(Some(Duration::ZERO));
    settle(&db.store, &first.key, not_sent.clone())
        .await
        .unwrap();
    settle(&db.store, &first.key, not_sent.clone())
        .await
        .unwrap();
    let second = sending(&db).await;
    assert_eq!(first.key.nonce, second.key.nonce);
    assert_eq!((first.key.attempt, second.key.attempt), (1, 2));

    for late in [
        not_sent,
        posted(),
        CardOutcome::Held,
        CardOutcome::Unavailable,
    ] {
        assert!(matches!(
            settle(&db.store, &first.key, late).await,
            Err(Error::Stale)
        ));
    }
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "sending");
    assert!(claim(&db.store, 8).await.unwrap().is_empty());
    assert!(public_lines(&db, &run).await.is_empty());
    assert!(receipt(&db, &approval.nonce).await.is_none());

    for _ in 0..2 {
        settle(&db.store, &second.key, posted()).await.unwrap();
    }
    assert_eq!(public_lines(&db, &run).await.len(), 1);
    assert_eq!(
        decide_as_owner(&db, &approval).await.unwrap(),
        ApprovalState::Approved
    );
    assert_eq!(
        db.scalar::<i32>("select attempts from bot_card_jobs").await,
        2
    );
}

#[tokio::test]
async fn an_old_card_lease_cannot_begin_or_block_the_next_attempt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    channel_card(&db, "retake", "n1").await;
    let first = claim(&db.store, 8).await.unwrap().remove(0);
    db.sql("update bot_card_jobs set lease_until_ms = bot_now_ms() - 1")
        .await;
    assert!(matches!(
        begin(&db.store, &first.key, rev()).await,
        Err(Error::Stale)
    ));
    let second = claim(&db.store, 8).await.unwrap().remove(0);
    assert!(matches!(
        begin(&db.store, &first.key, rev()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        CardOutbox::block(&db.store, first.key, ApprovalBlock::RouteChanged).await,
        Err(Error::Stale)
    ));
    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "leased");
    begin(&db.store, &second.key, rev()).await.unwrap();
    settle(&db.store, &second.key, posted()).await.unwrap();
}

#[tokio::test]
async fn sweep_rechecks_the_state_after_waiting_for_a_concurrent_settlement() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, _) = channel_card(&db, "sweep", "n1").await;
    sending(&db).await;
    db.sql("update bot_card_jobs set sending_ms = bot_now_ms() - 130000")
        .await;
    let mut tx = db.store.pool().begin().await.unwrap();
    let pid: i32 =
        sqlx::query_scalar("select pg_backend_pid() from bot_runs where id = $1 for update")
            .bind(&run.attempt.run.0)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    let sweeper = db.peer("sweep").await;
    let sweep = tokio::spawn(async move { claim(&sweeper, 0).await });
    db.waiter_blocked_by(pid).await;
    sqlx::query("update bot_card_jobs set state = 'staged', not_before_ms = bot_now_ms() + 5000, sending_ms = null, activation = null, checked_revision = null")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(sweep.await.unwrap().unwrap().is_empty());

    assert_eq!(state(&db, "bot_card_jobs", "n1").await, "staged");
    assert_eq!(state(&db, "bot_approvals", "n1").await, "pending");
    assert!(public_lines(&db, &run).await.is_empty());
}
