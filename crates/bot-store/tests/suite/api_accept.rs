//! Atomic accept-or-replay for the keyed OpenAI-compatible HTTP endpoint (PG).
//!
//! Covers key reservation, per-endpoint active capacity limits, concurrency, scoped lookups,
//! activation fences, pre-commit `Unavailable`, and ambiguous-commit `Unknown`.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiRuns, ApiState},
    command::JsonPayload,
    id::*,
    port::Error,
    state::*,
};
use std::{sync::Arc, time::Duration};
use tokio::sync::Barrier;

#[tokio::test]
async fn a_fresh_key_commits_the_request_the_run_and_the_audit_together_with_the_fixed_values() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    assert_eq!(
        (
            run.model.as_str(),
            run.binding.0.as_str(),
            run.agent.0.as_str(),
            run.revision.0.as_str()
        ),
        ("assistant", "b-assistant", "main", "r1")
    );
    assert_eq!(run.digest, [1; 32]);
    assert_eq!(run.state, ApiState::Active);
    let created: i64 = db
        .scalar(&format!(
            "select created_ms / 1000 from bot_runs where id = '{}'",
            run.run.0
        ))
        .await;
    assert_eq!(run.created as i64, created);
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(
        db.scalar::<String>(&format!(
            "select run_origin from bot_runs where id = '{}'",
            run.run.0
        ))
        .await,
        "http"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );
    let seen = lookup(&db, "k1", "build").await.expect("lookup");
    assert_eq!((seen.run.clone(), seen.created), (run.run, run.created));
}

#[tokio::test]
async fn the_same_key_and_digest_replays_the_run_without_a_new_run_or_slot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = accepted(admit(&db, request("k1", "build", "assistant", 1, 1)).await);
    // Replays must attach to the existing run even when capacity (1) is full.
    for _ in 0..3 {
        match admit(&db, request("k1", "build", "assistant", 1, 1)).await {
            ApiAdmission::Replayed(run) => assert_eq!(run.run, first.run),
            other => panic!("expected replay: {other:?}"),
        }
    }
    assert!(matches!(
        admit(&db, request("k2", "build", "assistant", 1, 1)).await,
        ApiAdmission::Capacity
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1,
        "replays must not add audit rows"
    );
}

#[tokio::test]
async fn another_digest_conflicts_even_for_another_model_or_binding_and_leaves_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    for (model, digest) in [("assistant", 2u8), ("other", 1), ("other", 3)] {
        assert!(
            matches!(
                admit(&db, request("k1", "build", model, digest, 4)).await,
                ApiAdmission::Conflict
            ),
            "{model} {digest}"
        );
    }
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
    assert_eq!(lookup(&db, "k1", "build").await.unwrap().run, first.run);
}

#[tokio::test]
async fn the_limit_counts_unfinished_work_and_a_finished_run_frees_its_slot() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let one = accepted(admit(&db, request("k1", "build", "assistant", 1, 2)).await);
    let _two = accepted(admit(&db, request("k2", "build", "assistant", 2, 2)).await);
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 2)).await,
        ApiAdmission::Capacity
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (2, 2));
    // Waiting runs (approval or hold) still occupy capacity slots.
    db.sql(&format!(
        "update bot_runs set state = 'waiting', wait_kind = 'hold' where id = '{}'",
        one.run.0
    ))
    .await;
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 2)).await,
        ApiAdmission::Capacity
    ));
    db.sql(&format!(
        "update bot_runs set state = 'succeeded', wait_kind = null where id = '{}'",
        one.run.0
    ))
    .await;
    let three = accepted(admit(&db, request("k3", "build", "assistant", 3, 2)).await);
    assert_ne!(three.run, one.run);
    // Capacity is tracked per endpoint.
    let mut elsewhere = request("k4", "build", "assistant", 4, 1);
    elsewhere.endpoint = "other".into();
    elsewhere.accept.event.principal = PrincipalId("http:other/build".into());
    assert!(matches!(
        admit(&db, elsewhere).await,
        ApiAdmission::Accepted(_)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_identical_requests_make_exactly_one_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(8));
    let mut tasks = Vec::new();
    for n in 0..8 {
        let store = db.peer(&format!("w{n}")).await;
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ApiRuns::accept(&store, request("same", "build", "assistant", 7, 4)).await
        }));
    }
    let mut accepted = 0;
    let mut replayed = 0;
    let mut runs = std::collections::BTreeSet::new();
    for task in tasks {
        match task.await.unwrap().unwrap() {
            ApiAdmission::Accepted(run) => {
                accepted += 1;
                runs.insert(run.run.0);
            }
            ApiAdmission::Replayed(run) => {
                replayed += 1;
                runs.insert(run.run.0);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted, replayed, runs.len()), (1, 7, 1));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_distinct_keys_never_exceed_the_limit() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let barrier = Arc::new(Barrier::new(12));
    let mut tasks = Vec::new();
    for n in 0..12u8 {
        let store = db.peer(&format!("w{n}")).await;
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            ApiRuns::accept(
                &store,
                request(&format!("k{n}"), "build", "assistant", n, 3),
            )
            .await
        }));
    }
    let (mut accepted, mut full) = (0, 0);
    for task in tasks {
        match task.await.unwrap().unwrap() {
            ApiAdmission::Accepted(_) => accepted += 1,
            ApiAdmission::Capacity => full += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((accepted, full), (3, 9));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (3, 3));
}

#[tokio::test]
async fn a_lookup_sees_only_its_own_profile_endpoint_principal_and_key() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 4)).await);
    assert_eq!(lookup(&db, "k1", "build").await.unwrap().run, run.run);
    assert!(lookup(&db, "k1", "other").await.is_none());
    assert!(lookup(&db, "nope", "build").await.is_none());
    let mut elsewhere = lookup_key("k1", "build");
    elsewhere.endpoint = "other".into();
    assert!(
        ApiRuns::lookup(&db.store, elsewhere)
            .await
            .unwrap()
            .is_none()
    );
    let mut other_profile = lookup_key("k1", "build");
    other_profile.profile = ProfileId("work".into());
    assert!(
        ApiRuns::lookup(&db.store, other_profile)
            .await
            .unwrap()
            .is_none()
    );
    // Legacy webhook requests sharing the principal and key must not appear in API lookups.
    let mut legacy = accept_as(
        "personal",
        "legacy",
        &principal("build").0,
        ConversationKind::Http,
        "c-legacy",
        "hi",
    );
    legacy.event.binding = BindingId("hook".into());
    runs(&db).accept(legacy).await.unwrap();
    assert!(lookup(&db, "legacy", "build").await.is_none());
}

#[tokio::test]
async fn the_fixed_values_and_the_activation_fence_survive_a_later_revision_and_set_change() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 8)).await);
    let pinned = db.store.for_activation(Revision("set-1".into()));
    assert!(
        ApiRuns::lookup(&pinned, lookup_key("k1", "build"))
            .await
            .unwrap()
            .is_some()
    );
    let set = std::collections::BTreeMap::from([
        (ProfileId("personal".into()), Revision("r2".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    bot_core::profile::ActiveRevisions::replace(
        &db.store,
        Some(Revision("set-1".into())),
        Revision("set-2".into()),
        set,
        Vec::new(),
        Default::default(),
    )
    .await
    .unwrap();
    let seen = lookup(&db, "k1", "build").await.unwrap();
    assert_eq!(
        (
            seen.run,
            seen.binding.0.as_str(),
            seen.agent.0.as_str(),
            seen.revision.0.as_str(),
            seen.model.as_str()
        ),
        (run.run, "b-assistant", "main", "r1", "assistant")
    );
    // A handle pinned to the old active set must reject both lookup and accept as stale.
    let got = ApiRuns::lookup(&pinned, lookup_key("k1", "build")).await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert!(matches!(
        ApiRuns::accept(&pinned, request("k2", "build", "assistant", 2, 8))
            .await
            .unwrap(),
        ApiAdmission::Stale
    ));
    // New requests must match the current active revision (r2).
    assert!(matches!(
        admit(&db, request("k3", "build", "assistant", 3, 8)).await,
        ApiAdmission::Stale
    ));
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (1, 1));
}

#[tokio::test]
async fn a_failure_before_the_commit_leaves_neither_a_request_row_nor_a_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.sql(
        "create function bot_fail_audit() returns trigger as $$ begin raise exception 'audit blocked'; end $$ language plpgsql",
    )
    .await;
    db.sql("create trigger bot_fail_audit before insert on bot_audit for each row execute function bot_fail_audit()")
        .await;
    let got = ApiRuns::accept(&db.store, request("k1", "build", "assistant", 1, 4)).await;
    assert!(matches!(got, Err(Error::Unavailable)), "{got:?}");
    assert_eq!((requests(&db).await, runs_of_endpoint(&db).await), (0, 0));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 0);
    db.sql("drop trigger bot_fail_audit on bot_audit").await;
    assert!(matches!(
        admit(&db, request("k1", "build", "assistant", 1, 4)).await,
        ApiAdmission::Accepted(_)
    ));
}

#[tokio::test]
async fn a_commit_that_cannot_be_confirmed_is_unknown_and_never_half_written() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Terminate the backend during deferred constraint check at COMMIT to simulate a lost reply.
    db.sql(
        "create function bot_kill_at_commit() returns trigger as $$ begin perform pg_terminate_backend(pg_backend_pid()); return null; end $$ language plpgsql",
    )
    .await;
    db.sql("create constraint trigger bot_kill_at_commit after insert on bot_requests deferrable initially deferred for each row execute function bot_kill_at_commit()")
        .await;
    let got = admit(&db, request("k1", "build", "assistant", 1, 4)).await;
    assert!(matches!(got, ApiAdmission::Unknown), "{got:?}");
    db.sql("drop trigger bot_kill_at_commit on bot_requests")
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(lookup(&db, "k1", "build").await.is_none());
    assert_eq!(
        (
            requests(&db).await,
            db.scalar::<i64>("select count(*) from bot_runs").await
        ),
        (0, 0)
    );
    assert!(matches!(
        admit(&db, request("k1", "build", "assistant", 1, 4)).await,
        ApiAdmission::Accepted(_)
    ));
}

type Mutation = Box<dyn Fn(&mut ApiAccept)>;

#[tokio::test]
async fn requests_that_do_not_fit_this_entrance_are_invalid_and_record_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let cases: Vec<(&str, Mutation)> = vec![
        ("limit 0", Box::new(|r| r.max_active = 0)),
        ("limit 257", Box::new(|r| r.max_active = 257)),
        (
            "principal is not http",
            Box::new(|r| r.accept.event.principal = PrincipalId("local:owner".into())),
        ),
        (
            "endpoint differs from the principal's",
            Box::new(|r| r.endpoint = "other".into()),
        ),
        (
            "key longer than 128 bytes",
            Box::new(|r| r.accept.event.request = RequestId("k".repeat(129))),
        ),
        (
            "empty key",
            Box::new(|r| r.accept.event.request = RequestId(String::new())),
        ),
        ("empty model", Box::new(|r| r.model = String::new())),
        (
            "a direct tool target",
            Box::new(|r| {
                r.accept.target = bot_core::command::Target::Tool {
                    agent: AgentId("main".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                }
            }),
        ),
        (
            "not an http conversation",
            Box::new(|r| {
                r.accept.event.conversation.kind = ConversationKind::Cli;
                r.accept.event.http = None;
            }),
        ),
    ];
    for (name, change) in cases {
        let mut req = request("k1", "build", "assistant", 1, 4);
        change(&mut req);
        let got = ApiRuns::accept(&db.store, req).await;
        assert!(matches!(got, Err(Error::Invalid)), "{name}: {got:?}");
    }
    assert_eq!(
        (
            requests(&db).await,
            db.scalar::<i64>("select count(*) from bot_runs").await
        ),
        (0, 0)
    );
}
