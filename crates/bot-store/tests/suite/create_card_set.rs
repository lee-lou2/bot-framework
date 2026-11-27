//! Routed `schedule.create` approval cards open under the host active-set lock (PG).
//!
//! Destination routing is detected from the top-level `destination` key in the card's `Args` row
//! and requires a pinned (`for_activation`) handle; unreadable `Args` rows are rejected as `Invalid`.

use crate::support::*;
use bot_core::{
    cards::{LABEL_APPROVER, LABEL_ARGS, LABEL_REASON, LABEL_REQUESTER, LABEL_RISK, LABEL_TOOL},
    id::*,
    ledger::*,
    port::*,
    state::*,
};
use serde_json::json;
use sqlx::{Connection, Executor};
use std::time::Duration;

const TOOL: &str = "schedule.create";
const PLAIN: &str = r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","quiet":false}"#;
const ROUTED: &str = r#"{"at":"2030-01-01T12:00:00Z","destination":{"binding":"slack","conversation":{"id":"T1/C9","kind":"channel","thread":null}},"instruction":"x","quiet":false}"#;

fn card(nonce: &str, args: &str) -> bot_core::command::JsonPayload {
    let mut lines = vec![
        format!("{LABEL_TOOL}: schedule.create"),
        format!("{LABEL_RISK}: irreversible"),
        format!("{LABEL_REASON}: irreversible"),
        format!("{LABEL_REQUESTER}: local:owner"),
        format!("{LABEL_APPROVER}: local:owner"),
    ];
    let mut rest = args;
    let mut first = true;
    while !rest.is_empty() {
        let prefix = if first {
            format!("{LABEL_ARGS}: ")
        } else {
            "  ".to_owned()
        };
        first = false;
        let mut cut = (200 - prefix.len()).min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        lines.push(format!("{prefix}{head}"));
        rest = tail;
    }
    bot_core::command::JsonPayload(
        json!({
            "approval": {
                "nonce": nonce,
                "title": "Approval request: schedule.create",
                "lines": lines,
            }
        })
        .to_string(),
    )
}

async fn create_card(db: &TestDb, run: &RunContext, call_id: &str, args: &str) -> OpenApproval {
    let hash = bot_core::canon::sha256(args);
    calls(db)
        .begin(run.attempt.clone(), call(call_id), tool(TOOL), hash)
        .await
        .unwrap();
    let approval = Approval {
        tool: tool(TOOL),
        risk: Risk::Irreversible,
        args_sha256: hash,
        ..binding(&run.attempt, call_id, &format!("n-{call_id}"))
    };
    OpenApproval {
        reasons: vec!["irreversible".into()],
        card: card(&approval.nonce.0, args),
        approval,
        delivery: None,
    }
}

async fn approvals(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_approvals").await
}

async fn schedules(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_user_schedules")
        .await
}

#[tokio::test]
async fn a_replacement_committed_first_leaves_no_card_and_no_schedule_for_a_routed_create() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let open = create_card(&db, &run, "c1", ROUTED).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    assert!(matches!(
        CallLedger::open_approval(&pinned, open).await,
        Err(Error::Stale)
    ));
    assert_eq!(approvals(&db).await, 0);
    assert_eq!(schedules(&db).await, 0);
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");
}

#[tokio::test]
async fn a_plain_create_and_other_approvals_still_open_on_an_older_pinned_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let plain = create_card(&db, &run, "c1", PLAIN).await;
    db.sql("update bot_active set revision = 'set-2'").await;
    CallLedger::open_approval(&pinned, plain).await.unwrap();
    assert_eq!(approvals(&db).await, 1);

    // Mentioning `"destination"` inside `instruction` is not a top-level destination key.
    let run2 = run_in(&db, "r2", OWNER, ConversationKind::Direct).await;
    let tricky = create_card(
        &db,
        &run2,
        "c2",
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"\"destination\":{\"binding\":\"x\"}","quiet":false}"#,
    )
    .await;
    CallLedger::open_approval(&pinned, tricky).await.unwrap();
    assert_eq!(approvals(&db).await, 2);

    let run3 = run_in(&db, "r3", OWNER, ConversationKind::Direct).await;
    let plain_again = create_card(&db, &run3, "c3", PLAIN).await;
    CallLedger::open_approval(&db.store, plain_again)
        .await
        .unwrap();
    assert_eq!(approvals(&db).await, 3);
}

fn unreadable_cards(nonce: &str) -> Vec<(&'static str, bot_core::command::JsonPayload)> {
    let with = |lines: Vec<String>| {
        bot_core::command::JsonPayload(
            json!({"approval": {"nonce": nonce, "title": "t", "lines": lines}}).to_string(),
        )
    };
    vec![
        (
            "missing row",
            with(vec![format!("{LABEL_TOOL}: schedule.create")]),
        ),
        ("not json", with(vec![format!("{LABEL_ARGS}: {{\"at\":")])),
        ("not an object", with(vec![format!("{LABEL_ARGS}: [1,2]")])),
        (
            "string",
            with(vec![format!("{LABEL_ARGS}: \"destination\"")]),
        ),
        (
            "duplicate rows",
            with(vec![
                format!("{LABEL_ARGS}: {PLAIN}"),
                format!("{LABEL_ARGS}: {ROUTED}"),
            ]),
        ),
    ]
}

#[tokio::test]
async fn a_create_card_whose_arguments_row_cannot_be_read_is_closed_as_invalid_not_treated_as_plain()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let pinned = db.store.for_activation(Revision("set-1".into()));
    for (n, handle) in [("pinned", Some(&pinned)), ("unpinned", None)]
        .into_iter()
        .enumerate()
    {
        let (what, handle) = handle;
        for (m, (label, card)) in unreadable_cards("n").into_iter().enumerate() {
            let run = leased(&db, &format!("r{n}{m}")).await;
            let mut open = create_card(&db, &run, "c1", PLAIN).await;
            open.card = card;
            let result = match handle {
                Some(pinned) => CallLedger::open_approval(pinned, open).await,
                None => CallLedger::open_approval(&db.store, open).await,
            };
            assert!(
                matches!(result, Err(Error::Invalid)),
                "{what} / {label}: {result:?}"
            );
            assert_eq!(
                approvals(&db).await,
                0,
                "{what} / {label}: expected 0 cards"
            );
            assert!(
                matches!(
                    calls(&db)
                        .lookup(run.attempt.clone(), call("c1"))
                        .await
                        .unwrap()
                        .unwrap()
                        .phase,
                    CallPhase::Open { approval: None }
                ),
                "{what} / {label}"
            );
        }
    }
}

#[tokio::test]
async fn a_routed_create_cannot_be_opened_by_a_handle_that_does_not_pin_the_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    let open = create_card(&db, &run, "c1", ROUTED).await;
    let result = CallLedger::open_approval(&db.store, open).await;
    assert!(matches!(result, Err(Error::Denied)), "{result:?}");
    assert_eq!(approvals(&db).await, 0);
    assert_eq!(schedules(&db).await, 0);
    assert!(matches!(
        calls(&db)
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .unwrap()
            .phase,
        CallPhase::Open { approval: None }
    ));
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "leased");

    let pinned = db.store.for_activation(Revision("set-1".into()));
    let again = create_card(&db, &run, "c1", ROUTED).await;
    CallLedger::open_approval(&pinned, again).await.unwrap();
    assert_eq!(approvals(&db).await, 1);
}

async fn open_during_a_replacement(replace: bool) -> Option<(Result<(), Error>, i64, i64)> {
    let db = TestDb::new().await?;
    let run = leased(&db, "r").await;
    let open = create_card(&db, &run, "c1", ROUTED).await;
    let pinned = db.store.for_activation(Revision("set-1".into()));
    let mut replacer = db.raw().await;
    let mut tx = replacer.begin().await.unwrap();
    tx.execute("select 1 from bot_active for update")
        .await
        .unwrap();
    let task = tokio::spawn(async move { CallLedger::open_approval(&pinned, open).await });
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
    Some((result, approvals(&db).await, schedules(&db).await))
}

#[tokio::test]
async fn a_replacement_in_flight_is_waited_for_and_the_routed_card_is_judged_after_it() {
    let Some((result, cards, made)) = open_during_a_replacement(true).await else {
        return;
    };
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert_eq!((cards, made), (0, 0));
    let (result, cards, made) = open_during_a_replacement(false).await.unwrap();
    result.unwrap();
    assert_eq!((cards, made), (1, 0));
}
