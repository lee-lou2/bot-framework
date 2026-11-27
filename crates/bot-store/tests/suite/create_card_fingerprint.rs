//! Schedule creation approval cards open only when SHA-256 of the card's `Args` line matches both approval and call fingerprints.

use crate::support::{schedule::*, *};
use bot_core::{
    id::*,
    ledger::*,
    port::*,
    schedule::{DeliveryTarget, ScheduleSpec, Timing},
    state::*,
};
use std::time::Duration;

const OWNER: &str = "local:owner";

fn approval_for(run: &RunContext, call_id: &str, hash: [u8; 32]) -> Approval {
    Approval {
        tool: tool("schedule.create"),
        args_sha256: hash,
        ..binding(&run.attempt, call_id, &format!("n-{call_id}"))
    }
}

async fn nothing_opened(db: &TestDb, run: &RunContext, call_id: &str) {
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        0
    );
    assert!(matches!(
        calls(db)
            .lookup(run.attempt.clone(), call(call_id))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(db, &run.attempt.run).await, "leased");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'approval.open'")
            .await,
        0
    );
}

fn a_day_later(spec: &ScheduleSpec) -> ScheduleSpec {
    let mut later = spec.clone();
    later.timing = match &spec.timing {
        Timing::At(at) => Timing::At(*at + Duration::from_secs(86_400)),
        other => other.clone(),
    };
    later
}

#[tokio::test]
async fn a_card_that_shows_other_arguments_than_the_sealed_ones_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 3_600_000).await;
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;

    let shown = a_day_later(&spec).seal_create().unwrap().canonical;
    assert_ne!(shown, spec.seal_create().unwrap().canonical);
    let got = calls(&db)
        .open_approval(open_create_text(approval_for(&run, "c1", hash), &shown))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;

    // Exact canonical bytes must match; whitespace or key changes produce a different fingerprint.
    let canonical = spec.seal_create().unwrap().canonical;
    let spaced = canonical.replacen(',', ", ", 1);
    assert_ne!(spaced, canonical);
    for shown in [
        spaced,
        format!("{canonical} "),
        canonical.replace("\"quiet\"", "\"quiet \""),
    ] {
        let got = calls(&db)
            .open_approval(open_create_text(approval_for(&run, "c1", hash), &shown))
            .await;
        assert!(matches!(got, Err(Error::Conflict)), "{shown}: {got:?}");
    }
    nothing_opened(&db, &run, "c1").await;

    calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
}

#[tokio::test]
async fn the_call_that_was_opened_must_carry_the_same_fingerprint_as_the_card_and_the_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 3_600_000).await;
    let other = a_day_later(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", sealed(&other)).await;
    let got = calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", sealed(&spec)), &spec))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
}

#[tokio::test]
async fn a_long_card_split_over_continuation_lines_is_judged_by_its_whole_text() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let mut spec = spec_in(&db, 3_600_000).await;
    spec.instruction = "€ note ".repeat(300); // exceeds the 480-byte line limit multiple times
    let canonical = spec.seal_create().unwrap().canonical;
    assert!(canonical.len() > 2000);
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let tampered = canonical.replacen(
        "€ note € note € note € note € note",
        "€ note € note € note € note diff",
        1,
    );
    assert_ne!(tampered, canonical);
    let got = calls(&db)
        .open_approval(open_create_text(approval_for(&run, "c1", hash), &tampered))
        .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
    calls(&db)
        .open_approval(open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
}

#[tokio::test]
async fn a_routed_create_card_naming_another_recipient_than_the_sealed_one_opens_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let target = |id: &str| DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: bot_core::state::Conversation {
            kind: ConversationKind::Channel,
            id: id.into(),
            thread: None,
        },
    };
    let mut spec = spec_in(&db, 3_600_000).await;
    spec.destination = Some(target("T1/C9"));
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let mut elsewhere = spec.clone();
    elsewhere.destination = Some(target("T1/C1"));
    let shown = elsewhere.seal_create().unwrap().canonical;
    let got = CallLedger::open_approval(
        &pinned,
        open_create_text(approval_for(&run, "c1", hash), &shown),
    )
    .await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    nothing_opened(&db, &run, "c1").await;
    CallLedger::open_approval(&pinned, open_create(approval_for(&run, "c1", hash), &spec))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_approvals").await,
        1
    );
}
