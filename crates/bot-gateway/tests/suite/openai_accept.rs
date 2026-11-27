//! OpenAI-compatible ingress admission tests: model agent routing, approval routes, stale fencing, limits, and connection slots.
use crate::openai_support::*;
use bot_gateway::testing::api::Fault;
use std::sync::Arc;

#[tokio::test]
async fn an_unknown_commit_whose_ledger_row_is_someone_elses_request_is_a_conflict_not_a_join() {
    let rig = rig().await;
    rig.api.start_as(answer("result of earlier request"));
    assert_eq!(
        chat(
            &rig,
            BUILD_KEY,
            Some("dup1"),
            &ask("assistant", "original question")
        )
        .await
        .status,
        200
    );
    // Different payload under the same key with an unknown commit outcome must return conflict.
    rig.api.fault(Fault::UnknownNotCommitted);
    let reply = chat(
        &rig,
        BUILD_KEY,
        Some("dup1"),
        &ask("assistant", "different question"),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.error().1, "idempotency_key_conflict");
    assert!(!reply.body.contains("earlier request"));
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn a_stale_process_never_accepts_a_chat() {
    let rig = rig().await;
    rig.active.set(Some("set-2"));
    let reply = chat(&rig, BUILD_KEY, Some("st1"), &ask("assistant", "q")).await;
    assert_eq!(reply.status, 503, "{}", reply.body);
    assert_eq!(reply.header("retry-after"), Some("1"));
    assert!(
        rig.api.accepts().is_empty(),
        "stale process must not write to the store"
    );
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
                events: bot_core::slack::default_channel_events(),
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
    assert_eq!(by("slacked").team.as_deref(), Some("T1"));
    assert_eq!(
        by("slacked").accept.approver,
        Some(bot_core::id::PrincipalId("slack:home/U1".into()))
    );
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
    // OpenAI and webhook endpoints share the listener semaphore but render their own 503 formats.
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
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while rig.api.runs() < 256 && std::time::Instant::now() < until {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        rig.api.runs(),
        256,
        "all slot-filling requests must be admitted"
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
        "webhook keeps legacy error shape: {}",
        webhook.body
    );
    for handle in waiting {
        let reply = handle.await.unwrap();
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    assert_eq!(rig.api.runs(), 256);
}

#[tokio::test]
async fn the_connection_slot_stays_taken_while_the_body_still_waits_for_its_last_check() {
    // Even after the producer finishes queuing chunks, the connection permit stays held until body handoff completes.
    const SLOTS: usize = 3;
    let rig = rig_with(Setup {
        connections: Some(1),
        ..tuned(3000, 5)
    })
    .await;
    rig.api.start_as(answer("short reply"));
    let (active, reads) = (
        rig.active.clone(),
        Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    );
    rig.secrets.on_read(move || {
        // Delay active checks starting at the first chunk handoff (after auth and pre-header passes).
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
    let started = std::time::Instant::now();
    let busy = get(&rig, BUILD_KEY, "/v1/models").await;
    assert_eq!(busy.status, 503, "{}", busy.body);
    assert_eq!(busy.error().1, "service_unavailable");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    let _ = sse.rest(std::time::Duration::from_secs(10)).await;
    rig.active.delay(std::time::Duration::ZERO);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(get(&rig, BUILD_KEY, "/v1/models").await.status, 200);
}
