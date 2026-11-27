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
        .accept(accept("personal", "req-1", "오늘 일정 알려줘"))
        .await
        .unwrap();
    let again = db
        .store
        .accept(accept("personal", "req-1", "오늘 일정 알려줘"))
        .await
        .unwrap();
    assert_eq!(first, again, "같은 키·같은 내용은 기존 접수");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.accept'")
            .await,
        1
    );

    // 같은 키·다른 내용은 충돌이고 아무것도 늘지 않는다
    let changed = db
        .store
        .accept(accept("personal", "req-1", "다른 내용"))
        .await;
    assert!(is(changed, |e| matches!(e, Error::Conflict)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);

    // 키의 한 칸이라도 다르면 별개 접수다: 주체·프로필
    let other_principal = accept_as(
        "personal",
        "req-1",
        "local:guest",
        ConversationKind::Cli,
        "owner",
        "오늘 일정 알려줘",
    );
    let other_profile = accept("work", "req-1", "오늘 일정 알려줘");
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
    assert_eq!(ids.len(), 1, "모든 접수가 같은 run으로 수렴");
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

    // 다른 프로필의 같은 키는 조회되지 않는다
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
        .accept(accept("personal", "req-route", "일정 알려줘"))
        .await
        .unwrap();
    // 프로필 판본이 바뀌어 같은 외부 요청의 라우팅 결과(대상·판본)가 달라졌다
    let mut routed = accept("personal", "req-route", "일정 알려줘");
    routed.target = Target::Agent {
        agent: AgentId("main/research".into()),
        instruction: "다른 지침".into(),
    };
    routed.revision = Revision("r2".into());
    assert_eq!(
        runs(&db).accept(routed).await.unwrap(),
        first,
        "충돌이 아니라 최초 접수의 실행 계획으로 수렴한다"
    );
    assert_eq!(
        db.scalar::<String>("select agent || ':' || revision from bot_runs")
            .await,
        "main:r1"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // 외부 입력(본문·대화)이 달라지면 그때는 충돌이다
    let changed = runs(&db)
        .accept(accept("personal", "req-route", "다른 요청"))
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
        "같은 경로·메서드의 재전달은 같은 run"
    );
    for (method, path) in [("PUT", "/agenda"), ("POST", "/agenda/cancel")] {
        let other = runs(&db).accept(http(method, path)).await;
        assert!(matches!(other, Err(Error::Conflict)), "{method} {path}");
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // HTTP가 아닌 입구의 해시 바이트는 그대로다: http가 없는 요청은 같은 키로 http 요청과 충돌하지 않고 기존 접수와 같은 해시다
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

    // 실행 중인 도구(started)가 잡힌다
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

    // 승인 대기 중인 열린 호출(bot_calls.state = 'open' + bot_approvals.state = 'pending')은 started보다 우선하며 waiting_approval=true다
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

    // 승인이 거절되어 더는 pending이 아니면 started 도구로 돌아가고 waiting_approval=false다
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

    // 종료된 run은 활성 도구·자식·승인 대기를 비운다
    let claimed = runs(&db).claim().await.unwrap().unwrap();
    runs(&db).finish(claimed.attempt, done("끝")).await.unwrap();
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
    // 같은 최대 깊이(1)의 활성 자식 둘: 더 최근에 갱신된 자식을 골라야 한다
    let older = DelegationStore::delegate(
        &db.store,
        Delegate {
            parent: root.attempt.clone(),
            step: "step-1".into(),
            child: AgentId("main/older".into()),
            instruction: "먼저".into(),
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
        "같은 깊이에서는 updated_ms가 가장 최신인 활성 자식을 고른다"
    );

    // 깊이 2 활성 손자가 있으면 updated_ms가 더 오래돼도 깊이 2가 우선한다
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

    // 깊이 2 활성 손자가 둘이면 그중 가장 최근에 갱신된 손자를 고른다
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
