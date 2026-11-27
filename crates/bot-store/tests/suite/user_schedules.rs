//! User schedules (PG): identity restoration from the run, atomic create/cancel with call completion and audit, quotas, listing, and wake depth.

use crate::support::{schedule::*, *};
use bot_core::{command::*, id::*, ledger::*, port::*, schedule::*, state::*};

const OWNER: &str = "local:owner";

async fn audits(db: &TestDb, action: &str) -> i64 {
    db.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

#[tokio::test]
async fn creating_restores_identity_from_the_run_commits_the_call_and_replays_idempotently() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 3_600_000).await;
    let id = make(&db, &run, "c1", spec.clone()).await.unwrap();

    assert_eq!(column::<String>(&db, &id, "owner").await, OWNER);
    assert_eq!(column::<String>(&db, &id, "agent").await, "main");
    assert_eq!(column::<String>(&db, &id, "origin_binding").await, "desk");
    assert_eq!(column::<String>(&db, &id, "origin_kind").await, "direct");
    assert_eq!(column::<String>(&db, &id, "origin_id").await, "c1");
    assert_eq!(
        column::<Vec<String>>(&db, &id, "ceiling").await,
        run_scopes().into_iter().collect::<Vec<_>>()
    );
    assert_eq!(column::<String>(&db, &id, "state").await, "active");
    assert_eq!(column::<i16>(&db, &id, "wake_depth").await, 0);
    let result = db
        .scalar::<String>("select result from bot_calls where call = 'c1'")
        .await;
    assert_eq!(result, format!(r#"{{"schedule":"{}"}}"#, id.0));
    assert_eq!(audits(&db, "schedule.create").await, 1);
    assert_eq!(audits(&db, "call.done").await, 1);

    let again = users(&db)
        .create(start_call(&run.attempt, "c1", None), spec)
        .await
        .unwrap();
    assert_eq!(again, id);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        1
    );
}

#[tokio::test]
async fn bad_input_unapproved_destinations_and_http_callers_leave_no_trace() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let now = db.now_ms().await;
    let past = plain(Timing::At(at(now - 1000)), "past task");
    assert!(matches!(
        make(&db, &run, "c1", past).await,
        Err(Error::Invalid)
    ));
    let far = plain(
        Timing::At(at(now + 10 * 366 * 24 * 3_600_000)),
        "far future task",
    );
    assert!(matches!(
        make(&db, &run, "c2", far).await,
        Err(Error::Invalid)
    ));
    let zone = spec_cron("0 9 * * *", "Mars/Olympus");
    assert!(matches!(
        make(&db, &run, "c3", zone).await,
        Err(Error::Invalid)
    ));
    for text in ["", "  \n", &"€".repeat(MAX_INSTRUCTION_BYTES), "a\0b"] {
        let mut spec = spec_in(&db, 60_000).await;
        spec.instruction = text.into();
        assert!(
            matches!(make(&db, &run, "c4", spec).await, Err(Error::Invalid)),
            "{text:?}"
        );
    }
    let mut spec = spec_in(&db, 60_000).await;
    spec.destination = Some(DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "D9".into(),
            thread: None,
        },
    });
    assert!(matches!(
        make(&db, &run, "c5", spec).await,
        Err(Error::Denied)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
    assert_eq!(audits(&db, "schedule.create").await, 0);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_calls where state = 'done'")
            .await,
        0,
        "denied schedule creation must not complete the call"
    );
    let http = run_in(&db, "r2", "http:caller", ConversationKind::Http).await;
    let spec = spec_in(&db, 60_000).await;
    assert!(matches!(
        make(&db, &http, "h1", spec).await,
        Err(Error::Denied)
    ));
}

#[tokio::test]
async fn the_active_limit_counts_one_owner_across_calls_and_frees_on_cancel() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let mut ids = Vec::new();
    for n in 0..MAX_ACTIVE_PER_OWNER {
        let spec = spec_in(&db, 60_000 + n as i64).await;
        ids.push(make(&db, &run, &format!("c{n}"), spec).await.unwrap());
    }
    let spec = spec_in(&db, 90_000).await;
    assert!(matches!(
        make(&db, &run, "over", spec).await,
        Err(Error::Denied)
    ));
    cancel_with(&db, &run, "cancel-1", &ids[0]).await.unwrap();
    let spec = spec_in(&db, 90_000).await;
    assert!(make(&db, &run, "again", spec).await.is_ok());
    let other = run_in(&db, "r2", "local:guest", ConversationKind::Direct).await;
    let spec = spec_in(&db, 90_000).await;
    assert!(make(&db, &other, "g1", spec).await.is_ok());
}

#[tokio::test]
async fn only_the_owner_cancels_and_cancelling_twice_changes_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_in(&db, 60_000).await;
    let id = make(&db, &run, "c1", spec).await.unwrap();
    let guest = run_in(&db, "r2", "local:guest", ConversationKind::Direct).await;
    begin_tool(&db, &guest, "x1", "schedule.cancel", cancel_hash(&id)).await;
    assert!(matches!(
        users(&db)
            .cancel(start_call(&guest.attempt, "x1", None), id.clone())
            .await,
        Err(Error::Missing)
    ));
    assert_eq!(column::<String>(&db, &id, "state").await, "active");

    cancel_with(&db, &run, "k1", &id).await.unwrap();
    assert_eq!(column::<String>(&db, &id, "state").await, "canceled");
    assert!(column::<Option<i64>>(&db, &id, "next_ms").await.is_none());
    users(&db)
        .cancel(start_call(&run.attempt, "k1", None), id.clone())
        .await
        .unwrap();
    cancel_with(&db, &run, "k2", &id).await.unwrap();
    assert_eq!(audits(&db, "schedule.cancel").await, 2);
    let other = make(&db, &run, "c2", spec_in(&db, 60_000).await)
        .await
        .unwrap();
    assert!(matches!(
        users(&db)
            .cancel(start_call(&run.attempt, "k1", None), other)
            .await,
        Err(Error::Conflict)
    ));
}

#[tokio::test]
async fn the_list_is_mine_only_in_id_order_and_needs_the_current_claim_and_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let mut ids = Vec::new();
    ids.push(
        make(&db, &run, "c1", spec_in(&db, 60_000).await)
            .await
            .unwrap(),
    );
    ids.push(
        make(&db, &run, "c2", spec_cron("0 9 * * *", "Asia/Seoul"))
            .await
            .unwrap(),
    );
    let guest = run_in(&db, "r2", "local:guest", ConversationKind::Direct).await;
    make(&db, &guest, "g1", spec_in(&db, 60_000).await)
        .await
        .unwrap();

    let list = users(&db)
        .list(run.attempt.clone(), Revision("r1".into()))
        .await
        .unwrap();
    ids.sort();
    assert_eq!(list.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), ids);
    assert!(list.iter().any(
        |s| matches!(s.timing, Timing::Cron { ref timezone, .. } if timezone == "Asia/Seoul")
    ));
    assert!(matches!(
        users(&db)
            .list(run.attempt.clone(), Revision("r0".into()))
            .await,
        Err(Error::Stale)
    ));
    let mut old = run.attempt.clone();
    old.generation = Generation(old.generation.0 + 1);
    assert!(matches!(
        users(&db).list(old, Revision("r1".into())).await,
        Err(Error::Stale)
    ));
    cancel_with(&db, &run, "k1", &ids[0]).await.unwrap();
    let list = users(&db)
        .list(run.attempt.clone(), Revision("r1".into()))
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test]
async fn a_wake_keeps_the_tool_result_with_one_follow_up_per_call_and_the_chain_stops_at_eight() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    begin_call(&db, &run.attempt, "t1").await;
    calls(&db)
        .start(start_call(&run.attempt, "t1", None))
        .await
        .unwrap();
    let wake = |depth_secs: i64, now: i64| Wake {
        at: at(now + depth_secs),
        instruction: "check the result".into(),
        quiet: true,
    };
    let now = db.now_ms().await;
    let result = JsonPayload(r#"{"started":true}"#.into());
    let id = users(&db)
        .finish_with_wake(
            run.attempt.clone(),
            call("t1"),
            Revision("r1".into()),
            result.clone(),
            wake(60_000, now),
        )
        .await
        .unwrap();
    assert_eq!(column::<i16>(&db, &id, "wake_depth").await, 1);
    assert!(column::<bool>(&db, &id, "quiet").await);
    assert_eq!(column::<String>(&db, &id, "origin_kind").await, "direct");
    assert!(matches!(
        calls(&db).begin(run.attempt.clone(), call("t1"), tool("web.search"), [1; 32]).await.unwrap().phase,
        CallPhase::Done(ref r) if r.0 == r#"{"started":true}"#
    ));
    let again = users(&db)
        .finish_with_wake(
            run.attempt.clone(),
            call("t1"),
            Revision("r1".into()),
            result,
            wake(60_000, now),
        )
        .await
        .unwrap();
    assert_eq!(again, id);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        1
    );
    begin_call(&db, &run.attempt, "t2").await;
    assert!(matches!(
        users(&db)
            .finish_with_wake(
                run.attempt.clone(),
                call("t2"),
                Revision("r1".into()),
                JsonPayload("{}".into()),
                wake(60_000, now)
            )
            .await,
        Err(Error::Conflict)
    ));
    calls(&db)
        .start(start_call(&run.attempt, "t2", None))
        .await
        .unwrap();
    assert!(matches!(
        users(&db)
            .finish_with_wake(
                run.attempt.clone(),
                call("t2"),
                Revision("r0".into()),
                JsonPayload("{}".into()),
                wake(60_000, now)
            )
            .await,
        Err(Error::Stale)
    ));
    db.sql("update bot_runs set wake_depth = 8").await;
    assert!(matches!(
        users(&db)
            .finish_with_wake(
                run.attempt.clone(),
                call("t2"),
                Revision("r1".into()),
                JsonPayload("{}".into()),
                wake(60_000, now)
            )
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        1
    );
}
