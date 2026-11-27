use crate::support::*;
use bot_core::{command::*, id::*, intake::RequestKey, ledger::*, port::*, state::*};

fn is(result: Result<RunId, Error>, want: fn(&Error) -> bool) -> bool {
    matches!(result, Err(ref e) if want(e))
}

#[tokio::test]
async fn accepts_duplicate_request_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = db
        .store
        .accept(accept("personal", "req-1", "Show today's agenda"))
        .await
        .unwrap();
    let again = db
        .store
        .accept(accept("personal", "req-1", "Show today's agenda"))
        .await
        .unwrap();
    assert_eq!(first, again, "same key and body should return existing run");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );

    let changed = db
        .store
        .accept(accept("personal", "req-1", "Different body"))
        .await;
    assert!(is(changed, |e| matches!(e, Error::Conflict)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    // Any difference in the key tuple (principal or profile) creates a distinct run.
    let other_principal = accept_as(
        "personal",
        "req-1",
        "local:guest",
        ConversationKind::Cli,
        "owner",
        "Show today's agenda",
    );
    let other_profile = accept("work", "req-1", "Show today's agenda");
    assert_ne!(db.store.accept(other_principal).await.unwrap(), first);
    assert_ne!(db.store.accept(other_profile).await.unwrap(), first);
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_accepts_converge_on_one_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut tasks = Vec::new();
    for n in 0..12 {
        let store = if n % 2 == 0 {
            db.store.clone()
        } else {
            db.peer("peer").await
        };
        tasks.push(tokio::spawn(async move {
            store
                .accept(accept("personal", "same", "hi"))
                .await
                .unwrap()
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.dedup();
    assert_eq!(ids.len(), 1, "all accepts should converge on one run");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );
}

#[tokio::test]
async fn same_body_in_another_json_layout_is_the_same_request() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut a = accept("personal", "req-json", "x");
    a.event.body = JsonPayload(r#"{"b":1,  "a":[1,2]}"#.into());
    let mut b = accept("personal", "req-json", "x");
    b.event.body = JsonPayload(r#"{"a":[1,2],"b":1}"#.into());
    let first = db.store.accept(a).await.unwrap();
    assert_eq!(db.store.accept(b).await.unwrap(), first);
}

#[tokio::test]
async fn status_reads_the_same_request_without_new_effects() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let key = RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("desk".into()),
        principal: PrincipalId("local:owner".into()),
        request: RequestId("req-s".into()),
    };
    assert!(runs(&db).status(key.clone()).await.unwrap().is_none());
    let run = db
        .store
        .accept(accept("personal", "req-s", "hi"))
        .await
        .unwrap();
    let state = runs(&db).status(key.clone()).await.unwrap().unwrap();
    assert_eq!((state.run, state.state), (run.clone(), RunState::Ready));
    let before = db.scalar::<i64>("select count(*) from bot_audit").await;
    runs(&db).status(key.clone()).await.unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit").await,
        before
    );

    let foreign = RequestKey {
        profile: ProfileId("work".into()),
        ..key
    };
    assert!(runs(&db).status(foreign).await.unwrap().is_none());
}

#[tokio::test]
async fn rejects_malformed_and_oversized_input() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut empty_request = accept("personal", "", "hi");
    empty_request.event.request = RequestId(String::new());
    let mut bad_body = accept("personal", "r1", "hi");
    bad_body.event.body = JsonPayload("{not json".into());
    let mut control = accept("personal", "r2", "hi");
    control.event.principal = PrincipalId("local:ow\u{0}ner".into());
    let mut huge = accept("personal", "r3", "hi");
    huge.event.body = JsonPayload(format!("\"{}\"", "x".repeat(2 << 20)));
    let bad_args = tool_accept("personal", "r4", "agenda.list", "[1,2]");
    for bad in [empty_request, bad_body, control, huge, bad_args] {
        let result = db.store.accept(bad).await;
        assert!(is(result, |e| matches!(e, Error::Invalid)));
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 0);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_requests").await,
        0
    );
}

#[tokio::test]
async fn a_routing_change_after_the_first_accept_returns_the_original_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = runs(&db)
        .accept(accept("personal", "req-route", "Show agenda"))
        .await
        .unwrap();
    // Simulate a profile revision change altering the routed target for the same external request.
    let mut routed = accept("personal", "req-route", "Show agenda");
    routed.target = Target::Agent {
        agent: AgentId("main/research".into()),
        instruction: "Different instruction".into(),
    };
    routed.revision = Revision("r2".into());
    assert_eq!(
        runs(&db).accept(routed).await.unwrap(),
        first,
        "should converge on the original plan rather than conflict"
    );
    assert_eq!(
        db.scalar::<String>("select agent || ':' || revision from bot_runs")
            .await,
        "main:r1"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    let changed = runs(&db)
        .accept(accept("personal", "req-route", "Different request"))
        .await;
    assert!(matches!(changed, Err(Error::Conflict)));
}

#[tokio::test]
async fn an_http_request_id_reused_on_another_route_conflicts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let http = |method: &str, path: &str| {
        let mut a = accept_as(
            "personal",
            "req-http",
            "http:inbox/sub",
            ConversationKind::Http,
            "inbox",
            "x",
        );
        a.event.http = Some(HttpRoute {
            method: method.into(),
            path: path.into(),
            response: bot_core::profile::ResponseMode::Accepted,
        });
        a
    };
    let first = runs(&db).accept(http("POST", "/agenda")).await.unwrap();
    assert_eq!(
        runs(&db).accept(http("POST", "/agenda")).await.unwrap(),
        first,
        "redelivery on the same method and path returns the same run"
    );
    for (method, path) in [("PUT", "/agenda"), ("POST", "/agenda/cancel")] {
        let other = runs(&db).accept(http(method, path)).await;
        assert!(matches!(other, Err(Error::Conflict)), "{method} {path}");
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // Non-HTTP ingress keeps its unchanged input hash bytes.
    let cli = accept("personal", "req-cli", "x");
    let again = accept("personal", "req-cli", "x");
    assert_eq!(
        runs(&db).accept(cli).await.unwrap(),
        runs(&db).accept(again).await.unwrap()
    );
}

#[tokio::test]
async fn progress_reports_started_tool_and_open_call_with_pending_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "req-progress-calls").await;
    let initial = runs(&db)
        .progress(profile_id(), run.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(initial.state, RunState::Leased);
    assert_eq!(initial.active_tool, None);
    assert!(!initial.waiting_approval);

    begin_call(&db, &run.attempt, "c-run").await;
    calls(&db)
        .start(start_call(&run.attempt, "c-run", None))
        .await
        .unwrap();
    let running = runs(&db)
        .progress(profile_id(), run.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.active_tool.as_deref(), Some("web.search"));
    assert!(!running.waiting_approval);

    // An open call with a pending approval takes precedence over a started call.
    calls(&db)
        .begin(
            run.attempt.clone(),
            call("c-approve"),
            tool("calendar.delete"),
            [2; 32],
        )
        .await
        .unwrap();
    let mut approval = binding(&run.attempt, "c-approve", "n-prog-1");
    approval.tool = tool("calendar.delete");
    approval.args_sha256 = [2; 32];
    calls(&db)
        .open_approval(open_approval(approval))
        .await
        .unwrap();
    let waiting = runs(&db)
        .progress(profile_id(), run.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(waiting.state, RunState::Waiting);
    assert_eq!(waiting.active_tool.as_deref(), Some("calendar.delete"));
    assert!(waiting.waiting_approval);

    // Once denied, progress falls back to the started tool with waiting_approval=false.
    calls(&db)
        .decide(Decision {
            nonce: ApprovalId("n-prog-1".into()),
            actor: PrincipalId("local:owner".into()),
            allow: false,
        })
        .await
        .unwrap();
    let resumed = runs(&db)
        .progress(profile_id(), run.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.state, RunState::Ready);
    assert_eq!(resumed.active_tool.as_deref(), Some("web.search"));
    assert!(!resumed.waiting_approval);

    let claimed = runs(&db).claim().await.unwrap().unwrap();
    runs(&db)
        .finish(claimed.attempt, done("done"))
        .await
        .unwrap();
    let ended = runs(&db)
        .progress(profile_id(), run.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ended.state, RunState::Succeeded);
    assert_eq!(ended.active_tool, None);
    assert_eq!(ended.active_agent, None);
    assert!(!ended.waiting_approval);
}

#[tokio::test]
async fn progress_picks_most_recently_updated_deepest_active_child() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let root = leased(&db, "req-progress-children").await;
    let older = DelegationStore::delegate(
        &db.store,
        Delegate {
            parent: root.attempt.clone(),
            step: "step-1".into(),
            child: AgentId("main/older".into()),
            instruction: "first".into(),
            scopes: run_scopes(),
            checked_revision: Revision("r1".into()),
        },
    )
    .await
    .unwrap();
    db.sql(&format!(
        "insert into bot_runs (profile, id, root, parent, parent_step, depth, agent, principal, revision, binding,
                               conv_kind, conv_id, target, input, state, updated_ms)
         select profile, 'child-newer', root, parent, 'step-2', 1, 'main/newer', principal, revision, binding,
                conv_kind, conv_id, target, input, 'ready', updated_ms + 5000
         from bot_runs where profile = 'personal' and id = '{}'",
        older.0
    ))
    .await;
    let at_depth_1 = runs(&db)
        .progress(profile_id(), root.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        at_depth_1.active_agent.as_deref(),
        Some("main/newer"),
        "at equal depth, pick the most recently updated active child"
    );

    // Deeper active descendants take precedence even with an older updated_ms.
    db.sql(&format!(
        "insert into bot_runs (profile, id, root, parent, parent_step, depth, agent, principal, revision, binding,
                               conv_kind, conv_id, target, input, state, updated_ms)
         select profile, 'grandchild-older', root, id, 'step-deep-1', 2, 'main/older/leaf_old', principal, revision, binding,
                conv_kind, conv_id, target, input, 'ready', updated_ms - 5000
         from bot_runs where profile = 'personal' and id = '{}'",
        older.0
    ))
    .await;
    let at_depth_2_one = runs(&db)
        .progress(profile_id(), root.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        at_depth_2_one.active_agent.as_deref(),
        Some("main/older/leaf_old")
    );

    db.sql(&format!(
        "insert into bot_runs (profile, id, root, parent, parent_step, depth, agent, principal, revision, binding,
                               conv_kind, conv_id, target, input, state, updated_ms)
         select profile, 'grandchild-newer', root, id, 'step-deep-2', 2, 'main/older/leaf_new', principal, revision, binding,
                conv_kind, conv_id, target, input, 'ready', updated_ms + 1000
         from bot_runs where profile = 'personal' and id = '{}'",
        older.0
    ))
    .await;
    let at_depth_2_two = runs(&db)
        .progress(profile_id(), root.attempt.run.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        at_depth_2_two.active_agent.as_deref(),
        Some("main/older/leaf_new")
    );
}
