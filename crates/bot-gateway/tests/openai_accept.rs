//! OpenAI 호환 입구의 접수 명령(실제 리스너): 모델마다 바인딩 에이전트, 승인자·전달 설치 기록, 구판 프로세스·입구 속도 한도.
use bot_gateway::testing::api::Fault;
use openai_support::*;
use std::sync::Arc;

mod openai_support;

#[tokio::test]
async fn an_unknown_commit_whose_ledger_row_is_someone_elses_request_is_a_conflict_not_a_join() {
    let rig = rig().await;
    rig.api.start_as(answer("먼저 접수된 요청의 결과"));
    assert_eq!(
        chat(
            &rig,
            BUILD_KEY,
            Some("dup1"),
            &ask("assistant", "원래 질문")
        )
        .await
        .status,
        200
    );
    // 같은 키로 다른 질문을 보냈는데 접수 결과를 알 수 없다: 조회한 작업은 다른 요청의 것이다
    rig.api.fault(Fault::UnknownNotCommitted);
    let reply = chat(
        &rig,
        BUILD_KEY,
        Some("dup1"),
        &ask("assistant", "다른 질문"),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.error().1, "idempotency_key_conflict");
    assert!(!reply.body.contains("먼저 접수된"));
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_stale_process_never_accepts_a_chat() {
    let rig = rig().await;
    rig.active.set(Some("set-2"));
    let reply = chat(&rig, BUILD_KEY, Some("st1"), &ask("assistant", "q")).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.header("retry-after"), Some("1"));
    assert!(rig.api.accepts().is_empty(), "구판은 저장소에 쓰지 않는다");
    rig.active.set(Some("set-1"));
    rig.api.start_as(answer("ok"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("st1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn the_entrance_rate_limit_applies_to_every_subject_of_the_entrance() {
    let rig = rig_with(Setup {
        tune: Box::new(|host| host.limits.per_entrance_per_min = Some(2)),
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("x"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("e1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("e2"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    let limited = chat(&rig, BUILD_KEY, Some("e3"), &ask("assistant", "q")).await;
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert!(limited.header("retry-after").is_some());
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn each_model_runs_its_own_agent_and_the_approver_and_team_are_recorded_only_with_a_route() {
    use bot_core::{
        command::Target,
        profile::{Adapter, ApprovalRoute},
    };
    let host = r#"{
        "slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token", "channels": {"planning": "C1"}}}},
        "http": {"endpoints": {"openai": {"kind": "openai", "wait_ms": 2000, "keys": {
            "build": {"secret": "build-key", "subject": "build"}, "ops": {"secret": "ops-key", "subject": "ops"}}}}}
    }"#;
    let route = |model: &str, approval: Option<ApprovalRoute>| Adapter::OpenAi {
        endpoint: "openai".into(),
        model: model.into(),
        approvers: approval
            .map(|route| (principal("build"), route))
            .into_iter()
            .collect(),
    };
    let slack = ApprovalRoute::Slack {
        person: bot_core::id::PrincipalId("slack:home/U1".into()),
        binding: bot_core::id::BindingId("team".into()),
    };
    let local = ApprovalRoute::Local {
        person: bot_core::id::PrincipalId("local:owner".into()),
    };
    let fixture = bot_gateway::testing::profile("personal")
        .agent("side", &["run"])
        .agent_scopes("main", &["run"])
        .binding("slacked", route("slacked", Some(slack)), "main")
        .binding("localed", route("localed", Some(local)), "side")
        .binding("plain", route("plain", None), "main")
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
            },
            "main",
        )
        .member("http:openai/build", &["runner"])
        .member("slack:home/U1", &["runner"])
        .member("local:owner", &["runner"])
        .role("runner", &["run"])
        .policy_scopes(&["run"]);
    let rig = rig_host(
        host,
        Setup {
            profiles: vec![fixture],
            ..Setup::default()
        },
    )
    .await;
    rig.api.start_as(answer("ok"));
    for (model, key) in [("slacked", "m1"), ("localed", "m2"), ("plain", "m3")] {
        assert_eq!(
            chat(&rig, BUILD_KEY, Some(key), &ask(model, "q"))
                .await
                .status,
            200,
            "{model}"
        );
    }
    let accepts = rig.api.accepts();
    let by = |model: &str| accepts.iter().find(|a| a.model == model).unwrap();
    // 승인 경로가 Slack이면 접수 순간 호스트가 확인한 설치(팀)와 승인자를 기록한다
    assert_eq!(by("slacked").team.as_deref(), Some("T1"));
    assert_eq!(
        by("slacked").accept.approver,
        Some(bot_core::id::PrincipalId("slack:home/U1".into()))
    );
    // 로컬 경로는 팀이 없다. 경로가 없으면 승인자도 팀도 없다(HTTP 키 자신은 승인자가 아니다)
    assert_eq!(
        (
            by("localed").team.clone(),
            by("localed").accept.approver.clone()
        ),
        (None, Some(bot_core::id::PrincipalId("local:owner".into())))
    );
    assert_eq!(
        (
            by("plain").team.clone(),
            by("plain").accept.approver.clone()
        ),
        (None, None)
    );
    // 모델마다 바인딩 에이전트를 직접 접수한다
    let agent = |model: &str| match &by(model).accept.target {
        Target::Agent { agent, .. } => agent.0.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (agent("slacked"), agent("localed"), agent("plain")),
        ("main".into(), "side".into(), "main".into())
    );
    assert_eq!(by("localed").accept.event.binding.0, "localed");
}

#[tokio::test]
async fn when_every_connection_slot_is_busy_the_next_request_gets_a_plain_openai_503_and_the_webhook_its_own()
 {
    // 같은 리스너의 동시 대기 연결 수는 유한하다(웹훅과 한 세마포어): 자리가 다 차면 /v1은 OpenAI 형식 503, 웹훅은 옛 형식 503이다
    let rig = rig_with(Setup {
        tune: Box::new(|host| {
            host.limits.per_principal_per_min = Some(10_000);
            host.limits.per_entrance_per_min = Some(10_000);
            let endpoint = host.http.openai.get_mut("openai").unwrap();
            endpoint.wait_ms = Some(6000);
            endpoint.max_active_runs = Some(256);
        }),
        ..Setup::default()
    })
    .await;
    let waiting: Vec<_> = (0..256)
        .map(|n| {
            let addr = rig.addr;
            tokio::spawn(async move {
                let auth = bearer(BUILD_KEY);
                send(
                    addr,
                    "POST",
                    "/v1/chat/completions",
                    &[
                        ("Authorization", &auth),
                        ("Content-Type", "application/json"),
                        ("Idempotency-Key", &format!("busy-{n}")),
                    ],
                    &ask("assistant", "q").to_string(),
                )
                .await
            })
        })
        .collect();
    // 256개가 모두 접수돼 기다리는 중이 될 때까지
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while rig.api.runs() < 256 && std::time::Instant::now() < until {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        rig.api.runs(),
        256,
        "자리를 채우는 요청이 모두 접수돼야 해요"
    );
    let busy = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(busy.status, 503, "{}", busy.body);
    assert_eq!(
        busy.error(),
        (
            "server_error".into(),
            "service_unavailable".into(),
            serde_json::Value::Null
        )
    );
    assert_eq!(busy.header("retry-after"), Some("1"));
    assert!(busy.header("x-request-id").is_some());
    let webhook = send(rig.addr, "GET", "/inbox/_requests/x", &[], "").await;
    assert_eq!(webhook.status, 503);
    assert!(
        webhook.json()["error"].is_string(),
        "웹훅은 옛 형식이다: {}",
        webhook.body
    );
    // 기다리던 요청들은 끝나면 «처리 중» 안내로 마친다(작업은 그대로)
    for handle in waiting {
        let reply = handle.await.unwrap();
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    assert_eq!(rig.api.runs(), 256);
}

#[tokio::test]
async fn the_connection_slot_stays_taken_while_the_body_still_waits_for_its_last_check() {
    // 한도 1: 짧은 답이라 생산자는 조각을 큐에 다 넣고 곧 끝난다. 그래도 본문이 넘기기 직전의 재확인을 기다리는 동안은 연결이 살아 있으므로 새 요청을 받으면 안 된다.
    const SLOTS: usize = 3;
    let rig = rig_with(Setup {
        connections: Some(1),
        ..tuned(3000, 5)
    })
    .await;
    rig.api.start_as(answer("짧은 답"));
    let (active, reads) = (
        rig.active.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    rig.secrets.on_read(move || {
        // 인증 한 바퀴·헤더 전 확인 한 바퀴가 지난 뒤(첫 조각을 꺼내는 확인부터) 저장소가 느려진다
        if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2 * SLOTS {
            active.delay(std::time::Duration::from_secs(2));
        }
    });
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("slot1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(sse.status, 200);
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    // 생산자는 이미 끝났다(조각 3개가 큐 용량 안): 그래도 자리는 없다
    let started = std::time::Instant::now();
    let busy = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(busy.status, 503, "{}", busy.body);
    assert_eq!(busy.error().1, "service_unavailable");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    // 본문이 끝나면(기한이든 확인이 끝났든) 자리를 돌려준다
    let _ = sse.rest(std::time::Duration::from_secs(10)).await;
    rig.active.delay(std::time::Duration::ZERO);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}
