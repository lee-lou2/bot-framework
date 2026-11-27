//! Host active-set fencing for workers, schedulers, delegation, and ledger reads.

use crate::support::{schedule::*, *};
use bot_core::{
    command::*,
    id::*,
    ledger::CallLedger,
    port::{DelegationStore, Error, RunStore},
    profile::ActiveRevisions,
    schedule::*,
    state::ConversationKind,
};
use sqlx::Connection;
use std::{collections::BTreeMap, time::Duration};

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

/// Advances the active set revision while keeping profile revisions unchanged.
async fn remap(db: &TestDb, from: &str, to: &str) {
    let profiles = BTreeMap::from([
        (profile_id(), rev("r1")),
        (ProfileId("work".into()), rev("r1")),
    ]);
    db.store
        .replace(Some(rev(from)), rev(to), profiles, vec![], BTreeMap::new())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_worker_bound_to_an_old_set_cannot_claim_or_start_and_the_new_one_can() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r1").await;
    begin_call(&db, &run.attempt, "c1").await;
    runs(&db)
        .accept(accept("personal", "r2", "another"))
        .await
        .unwrap();
    let old = db.peer("old-worker").await.for_activation(rev("set-1"));
    remap(&db, "set-1", "set-2").await;

    assert!(matches!(old.claim().await, Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where state = 'ready'")
            .await,
        1,
        "stale handle must not claim ready runs"
    );
    let started =
        bot_core::ledger::CallLedger::start(&old, start_call(&run.attempt, "c1", None)).await;
    assert!(matches!(started, Err(Error::Stale)));
    assert_eq!(
        db.scalar::<String>("select state from bot_calls where call = 'c1'")
            .await,
        "open",
        "stale handle must not start calls"
    );

    let current = db.peer("new-worker").await.for_activation(rev("set-2"));
    let claimed = current
        .claim()
        .await
        .unwrap()
        .expect("new handle should claim");
    assert_eq!(claimed.principal.0, "local:owner");
    bot_core::ledger::CallLedger::start(&current, start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_claim_waits_for_a_replacement_in_flight_and_then_sees_the_committed_set() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    runs(&db)
        .accept(accept("personal", "r1", "waiting run"))
        .await
        .unwrap();
    let worker = db.peer("worker").await.for_activation(rev("set-1"));
    let mut connection = db.raw().await;
    let mut tx = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = 'set-2' where singleton returning pg_backend_pid()",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    let task = tokio::spawn(async move { worker.claim().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            )
            .bind(writer)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            if blocked {
                break;
            }
            assert!(
                !task.is_finished(),
                "claim finished before replacement committed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("claim must wait behind replacement");
    tx.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Stale)));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where state = 'ready'")
            .await,
        1
    );
}

#[tokio::test]
async fn an_old_scheduler_cannot_accept_a_static_occurrence_or_advance_a_user_schedule() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(ScheduleSet {
        profile: profile_id(),
        revision: rev("r1"),
        triggers: vec![TriggerSpec {
            approver: None,
            trigger: "morning".into(),
            cron: "* * * * *".into(),
            timezone: "UTC".into(),
            beat: false,
            principal: PrincipalId("service:morning".into()),
            scopes: scopes(&["calendar.read"]),
            target: Target::Tool {
                agent: AgentId("main".into()),
                tool: ToolId("agenda.list".into()),
                args: JsonPayload("{}".into()),
            },
        }],
    })
    .await
    .unwrap();
    let origin = run_in(&db, "r1", "local:owner", ConversationKind::Direct).await;
    let id = make(&db, &origin, "c1", spec_in(&db, 60_000).await)
        .await
        .unwrap();
    make_due(&db, &id, 5_000).await;
    let at_ms = column::<i64>(&db, &id, "at_ms").await;
    let cursor = column::<i64>(&db, &id, "cursor_ms").await;
    let current = db.store.current().await.unwrap().unwrap();
    let old = db
        .peer("old-scheduler")
        .await
        .for_activation(current.clone());
    remap(&db, &current.0, "set-next").await;

    let advance = AdvanceSchedule {
        profile: profile_id(),
        id: id.clone(),
        expected_cursor: at(cursor),
        checked_revision: Some(rev("r1")),
        next: NextOccurrence::Fire {
            at: at(at_ms),
            next: None,
            scopes: scopes(&["run"]),
        },
    };
    assert!(matches!(
        bot_core::schedule::UserSchedules::advance(&old, advance).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        bot_core::schedule::UserSchedules::due(&old, None, 10).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        0
    );

    let cursor_before = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    let occurrence = Occurrence {
        profile: profile_id(),
        agent: AgentId("main".into()),
        trigger: "morning".into(),
        scheduled_at: at(cursor_before + 60_000).min(at(db.now_ms().await - 1)),
    };
    let accepted = bot_core::port::ScheduleStore::accept_occurrence(
        &old,
        occurrence,
        occurrence_accept(
            "personal",
            "morning",
            ms(at(cursor_before + 60_000)),
            Target::Tool {
                agent: AgentId("main".into()),
                tool: ToolId("agenda.list".into()),
                args: JsonPayload("{}".into()),
            },
            &["calendar.read"],
        ),
    )
    .await;
    assert!(matches!(accepted, Err(Error::Stale)), "{accepted:?}");
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_occurrences")
            .await,
        0
    );
}

#[tokio::test]
async fn a_delegation_judged_under_a_replaced_revision_creates_no_child_and_keeps_the_parent_running()
 {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = leased(&db, "p1").await;
    let command = |revision: &str| Delegate {
        checked_revision: rev(revision),
        parent: parent.attempt.clone(),
        step: "s1".into(),
        child: AgentId("main/research".into()),
        instruction: "investigate".into(),
        scopes: scopes(&["run"]),
    };
    assert!(matches!(
        DelegationStore::delegate(&db.store, command("r0")).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        0
    );
    assert_eq!(state_of_run(&db, &parent.attempt.run).await, "leased");
    let old = db.peer("old").await.for_activation(rev("set-1"));
    remap(&db, "set-1", "set-2").await;
    assert!(matches!(
        DelegationStore::delegate(&old, command("r1")).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_runs where parent is not null")
            .await,
        0
    );
    let child = DelegationStore::delegate(&db.store, command("r1"))
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &parent.attempt.run).await, "waiting");
    assert_eq!(
        db.scalar::<String>(&format!(
            "select parent from bot_runs where id = '{}'",
            child.0
        ))
        .await,
        parent.attempt.run.0
    );
}

#[tokio::test]
async fn a_handle_pinned_to_an_old_set_reads_neither_the_context_nor_a_call() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "pinned").await;
    begin_call(&db, &run.attempt, "c1").await;
    let current = db.store.for_activation(rev("set-1"));
    assert!(current.context(run.attempt.clone()).await.is_ok());
    assert!(
        current
            .lookup(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .is_some()
    );
    remap(&db, "set-1", "set-2").await;
    assert!(matches!(
        current.context(run.attempt.clone()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        current.lookup(run.attempt.clone(), call("c1")).await,
        Err(Error::Stale)
    ));
    let next = db.store.for_activation(rev("set-2"));
    assert!(next.context(run.attempt.clone()).await.is_ok());
    assert!(
        next.lookup(run.attempt, call("c1"))
            .await
            .unwrap()
            .is_some()
    );
}
