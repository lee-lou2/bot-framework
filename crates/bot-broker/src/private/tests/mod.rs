use super::*;

mod recovery;
use crate::{keyed, testing::*};
use bot_core::{
    id::ClaimEpoch,
    intake::RequestKey,
    state::{Conversation, ConversationKind, Risk},
    trace::{Val, key as span_key},
};
use std::sync::Arc;

pub(super) const SENTINEL: &str = "TOP-SECRET-DM-TEXT-7731";
pub(super) const KEY: &[u8] = b"profile-intake-key-bytes";

pub(super) struct Rig {
    pub(super) gate: PrivateGate,
    pub(super) runner: Arc<FakeRunner>,
    pub(super) ledger: Arc<MemPrivateLedger>,
    pub(super) audit: Arc<MemAudit>,
    pub(super) tracer: Arc<RecordingTracer>,
    pub(super) catalog: Arc<FakeCatalog>,
}

pub(super) fn world(revision: &str) -> Arc<bot_core::profile::Snapshot> {
    let mut stepped = tool(Risk::Write, &["inbox.write"]);
    stepped.effects = true;
    snapshot("personal", revision)
        .role("owner", &["run", "inbox.write"])
        .role("reader", &["run"])
        .member("local:owner", &["owner"])
        .member("local:reader", &["reader"])
        .member("slack:home/U1234ABCD", &["owner"])
        .policy_scopes(&["run", "inbox.write"])
        .tool("main", "inbox.receive", tool(Risk::Write, &["inbox.write"]))
        .tool(
            "main",
            "inbox.delete",
            tool(Risk::Irreversible, &["inbox.write"]),
        )
        .tool("main", "inbox.other", tool(Risk::Write, &["inbox.write"]))
        .tool("main", "inbox.steps", stepped)
        .slack_dm_binding("direct", "main")
        .slack_channel_binding("channel", "main")
        .private_trigger("main", "secure", "direct", "inbox.receive")
        .private_trigger("main", "wrong", "channel", "inbox.receive")
        .private_trigger("main", "danger", "direct", "inbox.delete")
        .private_trigger("main", "steps", "direct", "inbox.steps")
        .build()
}

pub(super) fn rig(secret: bool) -> Rig {
    let clock = Arc::new(ManualClock::new(1_000));
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(world("r1"));
    let ledger = Arc::new(MemPrivateLedger::new());
    ledger.set_active_revision("r1");
    ledger.set_attempt(Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("private-run".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    });
    let runner = Arc::new(FakeRunner::new());
    runner.set_default_result(r#"{"content":[{"text":"ok","type":"text"}],"isError":false}"#);
    let audit = Arc::new(MemAudit::new());
    let tracer = Arc::new(RecordingTracer::new());
    let entries: Vec<(&str, &str, &str)> = if secret {
        vec![("personal", FINGERPRINT_SECRET, "profile-intake-key-bytes")]
    } else {
        vec![]
    };
    let gate = PrivateGate::new(
        PrivateParts {
            catalog: catalog.clone(),
            ledger: ledger.clone(),
            secrets: Arc::new(FakeSecrets::new(&entries)),
            sealer: Arc::new(FakeSealer),
            runner: runner.clone(),
            tracer: tracer.clone(),
            clock,
        },
        crate::rate::RateConfig::default(),
    );
    Rig {
        gate,
        runner,
        ledger,
        audit,
        tracer,
        catalog,
    }
}

pub(super) fn delivery(
    request: &str,
    principal: &str,
    binding: &str,
    tool: &str,
    text: &str,
) -> PrivateCall {
    PrivateCall {
        key: RequestKey {
            profile: ProfileId("personal".into()),
            binding: BindingId(binding.into()),
            principal: PrincipalId(principal.into()),
            request: RequestId(request.into()),
        },
        revision: Revision("r1".into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "dm-1".into(),
            thread: None,
        },
        agent: AgentId("main".into()),
        trigger: match (binding, tool) {
            ("channel", _) => "wrong",
            (_, "inbox.delete") => "danger",
            (_, "inbox.steps") => "steps",
            _ => "secure",
        }
        .into(),
        envelope_sha256: [1; 32],
        tool: ToolId(tool.into()),
        args: bot_core::command::JsonPayload(text.into()),
    }
}

pub(super) async fn receive(
    r: &Rig,
    call: PrivateCall,
) -> Result<bot_core::intake::PrivateDecision, Error> {
    r.gate.receive(call).await
}

pub(super) fn standard(request: &str) -> PrivateCall {
    delivery(request, "local:owner", "direct", "inbox.receive", SENTINEL)
}

#[tokio::test]
async fn the_selected_declaration_owns_its_pass_policy_even_when_targets_match() {
    for allows in [false, true] {
        let r = rig(true);
        let mut snap = world("r1");
        let agent = Arc::get_mut(&mut snap)
            .unwrap()
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap();
        let secure = agent
            .triggers
            .iter_mut()
            .find(|t| t.id == "secure")
            .unwrap();
        secure.order = 1;
        secure.private_pass = allows;
        let mut other = secure.clone();
        other.id = "other".into();
        other.order = 2;
        other.private_pass = !allows;
        agent.triggers.insert(0, other);
        r.catalog.publish(snap);
        r.runner.script(Ran::Done(bot_core::command::JsonPayload(
            r#"{"content":[],"isError":false,"_meta":{"bot/pass":true}}"#.into(),
        )));
        let decision = receive(&r, standard("req-1")).await.unwrap();
        assert_eq!(decision == PrivateDecision::Pass, allows);
        assert_eq!(r.runner.jobs().len(), 1);
    }
}

#[tokio::test]
async fn a_private_dm_is_started_once_runs_only_the_designated_tool_and_leaves_no_plaintext() {
    let r = rig(true);
    receive(&r, standard("req-1")).await.unwrap();
    let jobs = r.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (jobs[0].tool.id.0.as_str(), jobs[0].args.as_str()),
        ("inbox.receive", SENTINEL),
        "지정 도구만 원문을 받는다"
    );
    assert!(
        jobs[0].tool.tool.revision.0.ends_with("@img1"),
        "설치 판본에 묶인다"
    );
    assert_eq!(
        jobs[0].owner.split('.').take(2).collect::<Vec<_>>(),
        ["private", "private-run"]
    );
    // 전용 원장: 전송 직전 한 번 시작, 지문·도구·판본이 같이 간다
    let starts = r.ledger.starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(
        (starts[0].0.0.as_str(), starts[0].2.0.as_str()),
        ("inbox.receive", "r1")
    );
    assert_eq!(r.ledger.stored()[0].fingerprint, starts[0].1);
    assert_eq!(r.ledger.outcomes(), ["completed"]);
    // 저장소·감사·추적 어디에도 원문이 없고 호출 ID에도 원문이 들어가지 않는다
    let dumps = [
        format!("{:?}", r.ledger.stored()),
        format!("{:?}", r.audit.entries()),
        format!("{:?}", r.tracer.spans()),
        format!("{:?}", r.ledger.phases()),
    ];
    assert!(dumps.iter().all(|d| !d.contains(SENTINEL)), "{dumps:?}");
    // 지문은 평문 해시가 아니라 프로필 키와 접수 키를 묶은 HMAC다
    assert_ne!(
        r.ledger.stored()[0].fingerprint,
        keyed::hmac(KEY, &[SENTINEL.as_bytes()])
    );
    assert_ne!(
        r.ledger.stored()[0].fingerprint,
        crate::canon::sha256(SENTINEL)
    );
}

#[tokio::test]
async fn the_tool_result_is_discarded_even_when_it_echoes_the_input() {
    let r = rig(true);
    let echoed = format!(
        "{{\"content\":[{{\"text\":\"{SENTINEL}\",\"type\":\"text\"}}],\"isError\":false}}"
    );
    r.runner
        .script(crate::ports::Ran::Done(bot_core::command::JsonPayload(
            echoed,
        )));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.ledger.outcomes(),
        ["completed"],
        "고정 종결 상태만 남는다"
    );
    let dumps = [
        format!("{:?}", r.ledger.stored()),
        format!("{:?}", r.audit.entries()),
        format!("{:?}", r.tracer.spans()),
    ];
    assert!(dumps.iter().all(|d| !d.contains(SENTINEL)));
}

/// 민감 DM 전용 도구가 넘김(«처리하지 않음») 표식을 돌려줘도 넘길 곳이 없다 — 실패(보류)로 닫고, 도구는 한 번만 돌고, 원문은 어디에도 남지 않는다.
#[tokio::test]
async fn a_pass_marker_on_the_private_path_closes_as_held_and_hands_nothing_on() {
    let marker = r#"{"_meta":{"bot/pass":true},"content":[],"isError":false}"#;
    let echoing = format!(
        "{{\"_meta\":{{\"bot/pass\":true}},\"content\":[{{\"text\":\"{SENTINEL}\",\"type\":\"text\"}}],\"isError\":false}}"
    );
    for (n, result) in [marker.to_owned(), echoing].into_iter().enumerate() {
        let r = rig(true);
        r.runner
            .script(crate::ports::Ran::Done(bot_core::command::JsonPayload(
                result,
            )));
        receive(&r, standard(&format!("req-{n}"))).await.unwrap();
        assert_eq!(r.ledger.outcomes(), ["held"], "표식은 실패로 닫는다 (#{n})");
        assert_eq!(r.runner.jobs().len(), 1, "다음 실행 0 (#{n})");
        let dumps = [
            format!("{:?}", r.ledger.stored()),
            format!("{:?}", r.audit.entries()),
            format!("{:?}", r.tracer.spans()),
            format!("{:?}", r.ledger.phases()),
        ];
        assert!(dumps.iter().all(|d| !d.contains(SENTINEL)), "{dumps:?}");
        // 재전달이 도구를 다시 돌리지 않는다
        let _ = receive(&r, standard(&format!("req-{n}"))).await;
        assert_eq!(r.runner.jobs().len(), 1, "재전달 뒤에도 한 번 (#{n})");
    }
}

#[tokio::test]
async fn a_redelivery_never_runs_the_tool_twice_and_a_changed_body_conflicts() {
    let r = rig(true);
    receive(&r, standard("req-1")).await.unwrap();
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.runner.jobs().len(),
        1,
        "같은 접수 키의 재전달은 호출을 늘리지 않는다"
    );
    let changed = receive(
        &r,
        delivery(
            "req-1",
            "local:owner",
            "direct",
            "inbox.receive",
            "다른 본문",
        ),
    )
    .await;
    assert!(matches!(changed, Err(Error::Conflict)));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.ledger.stored().len(), 1);
}

#[tokio::test]
async fn the_same_key_cannot_be_reclaimed_for_another_agent_tool_or_conversation() {
    let r = rig(true);
    r.runner.script(crate::ports::Ran::NotSent);
    assert!(receive(&r, standard("req-1")).await.is_err()); // 반납된 접수
    let mut other_tool = standard("req-1");
    other_tool.tool = ToolId("inbox.other".into());
    assert!(
        matches!(receive(&r, other_tool).await, Err(Error::Denied)),
        "트리거에 없는 도구"
    );
    let mut other_conversation = standard("req-1");
    other_conversation.conversation.id = "dm-2".into();
    assert!(matches!(
        receive(&r, other_conversation).await,
        Err(Error::Conflict)
    ));
    assert_eq!(r.runner.jobs().len(), 1);
    receive(&r, standard("req-1")).await.unwrap(); // 같은 입력이면 이어 간다
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn a_stale_revision_from_the_gateway_is_never_stored() {
    let r = rig(true);
    r.catalog.publish(world("r2"));
    assert!(
        matches!(receive(&r, standard("req-1")).await, Err(Error::Stale)),
        "게이트웨이가 넘긴 판본이 낡았다"
    );
    assert!(r.ledger.stored().is_empty() && r.runner.jobs().is_empty());
}

#[tokio::test]
async fn the_gateways_revision_must_be_the_active_one_and_is_never_swapped() {
    let r = rig(true);
    let mut old = standard("req-1");
    old.revision = Revision("r0".into());
    assert!(matches!(receive(&r, old).await, Err(Error::Stale)));
    assert!(r.ledger.stored().is_empty() && r.runner.jobs().is_empty());
}

#[tokio::test]
async fn only_a_private_trigger_for_that_binding_agent_and_tool_is_accepted() {
    let r = rig(true);
    for (binding, tool) in [
        ("other", "inbox.receive"),
        ("direct", "inbox.other"),
        ("direct", "inbox.unknown"),
    ] {
        let result = receive(
            &r,
            delivery("req-x", "local:owner", binding, tool, SENTINEL),
        )
        .await;
        assert!(matches!(result, Err(Error::Denied)), "{binding}/{tool}");
    }
    let mut elsewhere = standard("req-y");
    elsewhere.agent = AgentId("main/gone".into());
    assert!(matches!(receive(&r, elsewhere).await, Err(Error::Denied)));
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
}

#[tokio::test]
async fn tools_that_need_an_approval_card_or_a_step_ledger_are_refused_without_storing_anything() {
    let r = rig(true);
    for tool in ["inbox.delete", "inbox.steps"] {
        let result = receive(
            &r,
            delivery("req-1", "local:owner", "direct", tool, SENTINEL),
        )
        .await;
        assert!(matches!(result, Err(Error::Denied)), "{tool}");
    }
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
    assert!(!format!("{:?}", r.audit.entries()).contains(SENTINEL));
}

#[tokio::test]
async fn the_sender_must_hold_the_scopes_now() {
    let r = rig(true);
    let reader = receive(
        &r,
        delivery("req-1", "local:reader", "direct", "inbox.receive", SENTINEL),
    )
    .await;
    assert!(matches!(reader, Err(Error::Denied)), "역할에 스코프가 없다");
    let stranger = receive(
        &r,
        delivery(
            "req-2",
            "local:stranger",
            "direct",
            "inbox.receive",
            SENTINEL,
        ),
    )
    .await;
    assert!(matches!(stranger, Err(Error::Denied)), "멤버가 아니다");
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
}

#[tokio::test]
async fn a_server_that_is_not_installed_or_a_secret_the_profile_does_not_list_is_refused() {
    let r = rig(true);
    r.runner.set_image("demo-agenda", None);
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Denied)
    ));
    let r = rig(true);
    let mut needy = tool(Risk::Write, &["inbox.write"]);
    needy
        .secrets
        .insert("TOKEN".into(), SecretName::parse("reader").unwrap());
    let snap = snapshot("personal", "r1")
        .role("owner", &["inbox.write"])
        .member("local:owner", &["owner"])
        .policy_scopes(&["inbox.write"])
        .tool("main", "inbox.receive", needy)
        .private_trigger("main", "secure", "direct", "inbox.receive")
        .build();
    r.catalog.publish(snap);
    assert!(
        matches!(receive(&r, standard("req-2")).await, Err(Error::Denied)),
        "프로필이 허용하지 않은 자격"
    );
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn without_the_profile_fingerprint_key_nothing_is_accepted() {
    let r = rig(false);
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Unavailable)
    ));
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
}

#[tokio::test]
async fn every_span_on_the_private_path_carries_the_private_mark_and_no_content() {
    let r = rig(true);
    receive(&r, standard("req-1")).await.unwrap();
    let _ = receive(
        &r,
        delivery("req-2", "local:owner", "other", "inbox.receive", SENTINEL),
    )
    .await;
    let _ = receive(
        &r,
        delivery("req-3", "local:owner", "direct", "inbox.delete", SENTINEL),
    )
    .await;
    let spans = r.tracer.spans();
    assert!(
        spans.iter().any(|s| s.name == "broker.private.exec"),
        "실행 스팬도 있다"
    );
    assert!(
        spans.len() >= 4,
        "성공·실패 모두 스팬이 있다: {}",
        spans.len()
    );
    assert!(spans.iter().all(|s| {
        s.attrs
            .iter()
            .any(|(k, v)| *k == span_key::BOT_PRIVATE && *v == Val::Bool(true))
    }));
    assert!(
        spans
            .iter()
            .all(|s| s.str(span_key::BOT_PRINCIPAL).is_none()
                && !format!("{s:?}").contains(SENTINEL))
    );
    assert!(
        spans.iter().any(|s| s.error.is_some()),
        "실패한 스팬에도 표지가 붙는다"
    );
}

#[tokio::test]
async fn sensitive_input_is_accepted_only_from_a_direct_conversation_on_a_slack_dm_binding() {
    let r = rig(true);
    // 대화 종류가 DM이 아니다
    for kind in [
        ConversationKind::Channel,
        ConversationKind::Cli,
        ConversationKind::Http,
        ConversationKind::Scheduled,
    ] {
        let mut call = standard("req-1");
        call.conversation.kind = kind;
        assert!(matches!(receive(&r, call).await, Err(Error::Denied)));
    }
    // 바인딩이 DM이 아니다(채널 바인딩에 private 트리거를 걸어도 받지 않는다)
    let on_channel = delivery("req-2", "local:owner", "channel", "inbox.receive", SENTINEL);
    assert!(matches!(receive(&r, on_channel).await, Err(Error::Denied)));
    // 바인딩이 다른 에이전트의 것이다 / 없는 바인딩이다
    let ghost = delivery("req-3", "local:owner", "nope", "inbox.receive", SENTINEL);
    assert!(matches!(receive(&r, ghost).await, Err(Error::Denied)));
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
}

#[tokio::test]
async fn the_private_jobs_progress_origin_is_the_authenticated_ingress_and_never_the_body() {
    use bot_core::state::RunOrigin;
    // 인증한 Slack 사람의 1:1 입력은 message다. 입력 본문이 출처를 정하지 못한다. Slack DM이 아닌 입구 조합(로컬 사용자가 Direct
    // 대화로 온 경우 등)은 message가 되지 못하고 알 수 없음으로 닫힌다. 멤버가 아닌 주체의 입력은 도구를 시작하지도 않는다.
    for (principal, request, expected) in [
        (
            "slack:home/U1234ABCD",
            "req-slack",
            Some(RunOrigin::Message),
        ),
        ("local:owner", "req-local", Some(RunOrigin::Unknown)),
        ("http:inbox/sub-1", "req-http", None),
        ("service:morning", "req-service", None),
    ] {
        let r = rig(true);
        let call = delivery(
            request,
            principal,
            "direct",
            "inbox.receive",
            r#"{"bot/trigger":"schedule","note":"claims to be a schedule"}"#,
        );
        let _ = receive(&r, call).await;
        let jobs = r.runner.jobs();
        match expected {
            Some(origin) => {
                assert_eq!(jobs.len(), 1, "{principal}");
                assert_eq!(jobs[0].origin, origin, "{principal}");
            }
            None => assert!(jobs.is_empty(), "{principal}"),
        }
    }
}

#[tokio::test]
async fn a_server_the_host_cannot_run_is_refused_before_a_private_input_is_stored() {
    let r = rig(true);
    r.runner.refuse_server("demo-agenda");
    let result = receive(&r, standard("req-state")).await;
    assert!(matches!(result, Err(Error::Denied)), "{result:?}");
    assert!(r.runner.jobs().is_empty() && r.ledger.stored().is_empty());
}
