//! Scheduled delivery approval cards open under the host active-set lock (PG).
//!
//! Drafts frozen under an old set fail with `Conflict`, pinned stale handles fail with `Stale`,
//! and in-flight set replacements serialize with card opening; non-delivery approvals remain unaffected.

use crate::support::{delivery::*, *};
use bot_core::{
    id::*,
    ledger::*,
    port::*,
    schedule::{DELIVER_CALL, DELIVER_TOOL},
    state::*,
};
use sqlx::{Connection, Executor};
use std::time::Duration;

async fn frozen(db: &TestDb) -> (RunContext, OpenApproval) {
    let run = destination_run(db, ConversationKind::Direct).await;
    let draft = replies(db)
        .draft(run.attempt.clone(), Some(answers(&["reply to send"])))
        .await
        .unwrap()
        .expect("draft");
    assert_eq!(draft.activation.0, "set-1");
    let hash = draft.sha256().unwrap();
    calls(db)
        .begin(
            run.attempt.clone(),
            call(DELIVER_CALL),
            tool(DELIVER_TOOL),
            hash,
        )
        .await
        .unwrap();
    let approval = delivery_approval(&run.attempt, "n1", hash);
    let open = open_delivery(&approval, &draft.approval_json().unwrap());
    (run, open)
}

async fn approvals(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_approvals").await
}

#[tokio::test]
async fn a_draft_frozen_under_another_set_is_never_shown_to_the_owner() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, open) = frozen(&db).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        calls(&db).open_approval(open).await,
        Err(Error::Conflict)
    ));
    assert_eq!(approvals(&db).await, 0);
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call(DELIVER_CALL))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
}

#[tokio::test]
async fn a_card_opens_when_the_set_is_the_one_the_draft_was_frozen_under() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, open) = frozen(&db).await;
    calls(&db).open_approval(open).await.unwrap();
    assert_eq!(approvals(&db).await, 1);
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
}

#[tokio::test]
async fn a_handle_pinned_to_an_older_set_cannot_open_a_delivery_card_but_other_approvals_still_open()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let (run, open) = frozen(&db).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        CallLedger::open_approval(&pinned, open).await,
        Err(Error::Stale)
    ));
    assert_eq!(approvals(&db).await, 0);

    CallLedger::begin(
        &pinned,
        run.attempt.clone(),
        call("c2"),
        tool("web.search"),
        [1; 32],
    )
    .await
    .unwrap();
    let other = binding(&run.attempt, "c2", "n2");
    CallLedger::open_approval(&pinned, open_approval(other))
        .await
        .unwrap();
    assert_eq!(approvals(&db).await, 1);
}

async fn open_during_a_replacement(
    pinned: bool,
    replace: bool,
) -> (Result<(), Error>, i64, String) {
    let Some(db) = TestDb::new().await else {
        return (Ok(()), -1, String::new());
    };
    let (run, open) = frozen(&db).await;
    let handle = if pinned {
        db.store.for_activation(Revision("set-1".into()))
    } else {
        db.store.clone()
    };
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&handle, open).await });
    let started = std::time::Instant::now();
    loop {
        let waiting: i64 = db
            .scalar(&format!(
                "select count(*) from pg_stat_activity where application_name = '{}' and wait_event_type = 'Lock' and query like '%bot_active%'",
                db.schema
            ))
            .await;
        if waiting == 1 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "open_approval did not wait for the active-set lock"
        );
        assert!(!task.is_finished(), "finished without waiting for the lock");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    if replace {
        tx.execute("update bot_active set revision = 'set-2'")
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let result = task.await.unwrap();
    (
        result,
        approvals(&db).await,
        state_of_run(&db, &run.attempt.run).await,
    )
}

#[tokio::test]
async fn opening_a_card_waits_for_a_set_replacement_in_flight_and_judges_after_it() {
    let (result, cards, state) = open_during_a_replacement(true, true).await;
    if cards < 0 {
        return;
    }
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!(cards, 0);
    assert_eq!(state, "leased");
    let (result, cards, state) = open_during_a_replacement(true, false).await;
    result.unwrap();
    assert_eq!(cards, 1);
    assert_eq!(state, "waiting");
}

#[tokio::test]
async fn a_handle_that_pins_no_set_also_waits_for_the_replacement_and_never_opens_an_old_sets_card()
{
    let (result, cards, state) = open_during_a_replacement(false, true).await;
    if cards < 0 {
        return;
    }
    assert!(matches!(result, Err(Error::Conflict)), "{result:?}");
    assert_eq!(cards, 0);
    assert_eq!(state, "leased");
    let (result, cards, state) = open_during_a_replacement(false, false).await;
    result.unwrap();
    assert_eq!((cards, state.as_str()), (1, "waiting"));
}
