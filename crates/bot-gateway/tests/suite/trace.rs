//! 계측 시험: 접수·회신·승인이 `core::trace` 스팬으로 남고, 본문·인자·nonce·영수증은 어디에도 실리지 않는다.
use bot_core::{
    id::{PrincipalId, ProfileId, RequestId, RunId},
    port::Error,
    profile::Adapter,
    state::{Conversation, ConversationKind},
    trace::{SpanRecord, Val, key},
};
use bot_gateway::{
    delivery::Deliverer,
    entrance::Real,
    host::HostConfig,
    inbound::{Event, Inbound},
    ingress::Ingress,
    observe,
    slack::{SlackGateway, api::SlackApi},
    testing::{
        ports::{MemoryBroker, MemoryLedger, MemoryOutbox, outgoing},
        slack::FakeSlack,
        *,
    },
};
use serde_json::json;
use std::sync::Arc;

const MARKER: &str = "합성-민감-표식-c41f";

fn host() -> HostConfig {
    HostConfig::from_json(
        r#"{"cli": {"accounts": {"owner": {"uid": 501}}},
            "slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token",
                "app_token": "app-token", "channels": {"planning": "C1"}}}},
            "http": {"endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "k"}}},
            "limits": {"per_principal_per_min": 50}}"#,
    )
    .unwrap()
}

fn fixture() -> Fixture {
    let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
    secure.private = true;
    secure.input.insert("text".into(), "body.text".into());
    let mut notify = http_trigger(
        "notify",
        "inbox",
        "POST",
        "/notify",
        tool_target("notify.send", "{}"),
    );
    notify.order = 1;
    profile("personal")
        .binding(
            "desk",
            Adapter::Cli {
                account: "owner".into(),
            },
            "main",
        )
        .binding(
            "direct",
            Adapter::SlackDm {
                workspace: "home".into(),
            },
            "main",
        )
        .binding(
            "inbox",
            Adapter::Http {
                endpoint: "inbox".into(),
            },
            "main",
        )
        .member("local:owner", &["owner"])
        .member("slack:home/U1", &["owner"])
        .member("http:inbox/svc", &["svc"])
        .role("owner", &["run"])
        .role("svc", &["calendar.delete"])
        .policy_scopes(&["run", "calendar.delete"])
        .trigger("main", secure)
        .trigger("main", notify)
}

struct Rig {
    ingress: Ingress,
    store: Arc<MemoryStore>,
    tracer: Arc<RecordingTracer>,
}

fn rig() -> Rig {
    let store = MemoryStore::new();
    let tracer = RecordingTracer::new();
    let ingress = Ingress::new(
        TestCatalog::new([fixture().build()]),
        Arc::new(host()),
        store.clone(),
        MemoryPrivate::new(),
    )
    .with_uid_resolver(|account| Ok(account.uid.unwrap()))
    .with_tracer(tracer.clone());
    Rig {
        ingress,
        store,
        tracer,
    }
}

fn cli(request: &str, text: &str) -> Inbound {
    Inbound {
        real: Real::Cli {
            uid: 501,
            profile: ProfileId("personal".into()),
        },
        principal: PrincipalId("local:owner".into()),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Cli,
            id: "owner".into(),
            thread: Some(request.into()),
        },
        attachments: vec![],
        body: json!({"text": text}),
        event: Event::Message,
    }
}

fn dm(user: &str, request: &str, text: &str) -> Inbound {
    Inbound {
        real: Real::SlackDm { team: "T1".into() },
        principal: PrincipalId(format!("slack:home/{user}")),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Direct,
            id: "D1".into(),
            thread: Some("1.1".into()),
        },
        attachments: vec![],
        body: json!({"text": text}),
        event: Event::Message,
    }
}

fn http(request: &str) -> Inbound {
    Inbound {
        real: Real::Http {
            endpoint: "inbox".into(),
        },
        principal: PrincipalId("http:inbox/svc".into()),
        request: RequestId(request.into()),
        conversation: Conversation {
            kind: ConversationKind::Http,
            id: "inbox".into(),
            thread: Some(request.into()),
        },
        attachments: vec![],
        body: json!({"to": "me"}),
        event: Event::Http {
            method: "POST".into(),
            path: "/notify".into(),
        },
    }
}

fn text_of(span: &SpanRecord) -> String {
    let mut all = vec![span.name.clone()];
    for (k, v) in &span.attrs {
        all.push((*k).to_owned());
        if let Val::Str(text) = v {
            all.push(text.clone());
        }
    }
    all.join("\n")
}

fn assert_vocabulary(span: &SpanRecord) {
    for (name, _) in &span.attrs {
        assert!(
            key::ALLOWED.contains(name) || *name == key::BOT_PRIVATE,
            "허용 어휘 밖의 키 {name}"
        );
    }
}

fn flag(span: &SpanRecord, name: &str) -> Option<bool> {
    span.attrs.iter().find_map(|(k, v)| match v {
        Val::Bool(b) if *k == name => Some(*b),
        _ => None,
    })
}

#[tokio::test]
async fn an_accepted_chat_leaves_one_ingress_span_with_run_and_outcome() {
    let rig = rig();
    rig.ingress.admit(cli("r1", MARKER)).await;
    let spans = rig.tracer.named("ingress cli");
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    assert_vocabulary(span);
    assert_eq!(span.str(key::BOT_PROFILE), Some("personal"));
    assert_eq!(span.str(key::BOT_BINDING), Some("desk"));
    assert_eq!(span.str(key::BOT_OUTCOME), Some("accepted"));
    assert_eq!(span.str(key::BOT_RUN_ID), Some("run-1"));
    assert_eq!(span.str(key::BOT_CONVERSATION_KIND), Some("cli"));
    assert_eq!(span.error, None);
    assert_eq!(flag(span, key::BOT_PRIVATE), None);
    // 같은 (프로필, run)의 스팬은 한 추적에 모인다
    let ids = observe::ids(&ProfileId("personal".into()), &RunId("run-1".into()));
    assert_eq!(span.ids.trace, ids.trace);
    // 본문은 어디에도 없다
    assert!(!text_of(span).contains(MARKER));
}

#[tokio::test]
async fn a_private_dm_is_marked_private_and_carries_no_content() {
    let rig = rig();
    rig.ingress.admit(dm("U1", "ev1", MARKER)).await;
    let span = rig.tracer.named("ingress slack").remove(0);
    assert_vocabulary(&span);
    assert_eq!(flag(&span, key::BOT_PRIVATE), Some(true));
    assert_eq!(span.str(key::BOT_OUTCOME), Some("private"));
    assert_eq!(span.str(key::BOT_TRIGGER), Some("secure"));
    assert_eq!(
        span.str(key::BOT_RUN_ID),
        None,
        "큐에 들어가지 않았으니 run이 없다"
    );
    assert!(!text_of(&span).contains(MARKER));
    assert!(rig.store.accepted().is_empty());
}

#[tokio::test]
async fn rejections_are_traced_with_their_classification() {
    let rig = rig();
    rig.ingress.admit(dm("U9", "ev1", "hi")).await;
    rig.store.fail_next(Error::Unavailable);
    rig.ingress.admit(cli("r1", "hi")).await;
    let slack = rig.tracer.named("ingress slack").remove(0);
    assert_eq!(slack.str(key::BOT_OUTCOME), Some("unregistered"));
    assert_eq!(slack.error, None, "정상 거절은 오류 상태가 아니다");
    let cli_span = rig.tracer.named("ingress cli").remove(0);
    assert_eq!(cli_span.str(key::BOT_OUTCOME), Some("unavailable"));
    assert_eq!(cli_span.error, Some("unavailable"));
    // 등록되지 않은 입구는 프로필을 알 수 없어 스팬이 없다
    let mut stranger = cli("r2", "hi");
    stranger.real = Real::Cli {
        uid: 999,
        profile: ProfileId("personal".into()),
    };
    let before = rig.tracer.spans().len();
    rig.ingress.admit(stranger).await;
    assert_eq!(rig.tracer.spans().len(), before);
}

#[tokio::test]
async fn http_requests_record_the_method_and_route_but_not_the_body() {
    let rig = rig();
    rig.ingress.admit(http("h1")).await;
    let span = rig.tracer.named("ingress http").remove(0);
    assert_vocabulary(&span);
    assert_eq!(span.str(key::HTTP_REQUEST_METHOD), Some("POST"));
    assert_eq!(span.str(key::HTTP_ROUTE), Some("/notify"));
    assert_eq!(span.str(key::BOT_TRIGGER), Some("notify"));
    assert!(!text_of(&span).contains("\"to\""));
}

/// 요청이 보낸 method·경로로 만든 HTTP 요청(등록된 트리거 정의와 같은지는 시험이 정한다).
fn http_at(request: &str, method: &str, path: &str) -> Inbound {
    let mut inbound = http(request);
    inbound.event = Event::Http {
        method: method.into(),
        path: path.into(),
    };
    inbound
}

#[tokio::test]
async fn an_http_request_that_matches_no_trigger_leaves_fixed_values_not_what_the_caller_sent() {
    let rig = rig();
    // 호출자가 경로에 식별자·비밀을 실었다 / 알 수 없는 method / 등록된 경로에 다른 method
    let leaks = [
        ("u1", "POST", "/notify/user-4242-key-abcdef0123456789"),
        ("u2", "PURGE-ME-9917", "/notify"),
        ("u3", "GET", "/notify"),
        ("u4", "POST", "/"),
        ("u5", "POST", "/NOTIFY"),
    ];
    for (request, method, path) in leaks {
        rig.ingress.admit(http_at(request, method, path)).await;
    }
    let spans = rig.tracer.named("ingress http");
    assert_eq!(spans.len(), leaks.len());
    for span in &spans {
        assert_vocabulary(span);
        assert_eq!(span.str(key::BOT_OUTCOME), Some("no_route"));
        assert_eq!(span.str(key::HTTP_ROUTE), Some("unmatched"));
        assert_eq!(span.str(key::HTTP_REQUEST_METHOD), Some("_OTHER"));
        let all = text_of(span);
        for needle in ["4242", "abcdef0123456789", "PURGE-ME-9917", "NOTIFY"] {
            assert!(!all.contains(needle), "{needle}이(가) 스팬에 있다: {all}");
        }
        assert_eq!(span.str(key::BOT_TRIGGER), None, "맞은 트리거가 없다");
    }
}

#[tokio::test]
async fn a_matched_http_request_records_the_registered_definitions_values() {
    let rig = rig();
    rig.ingress.admit(http("h1")).await;
    let span = rig.tracer.named("ingress http").remove(0);
    assert_eq!(span.str(key::HTTP_ROUTE), Some("/notify"));
    assert_eq!(span.str(key::HTTP_REQUEST_METHOD), Some("POST"));
    assert_eq!(span.str(key::BOT_TRIGGER), Some("notify"));
}

#[tokio::test]
async fn a_reply_delivery_joins_the_trace_of_the_run_it_answers() {
    let slack = FakeSlack::start().await;
    let outbox = MemoryOutbox::new();
    let tracer = RecordingTracer::new();
    let store = MemoryStore::new();
    let ingress = Arc::new(
        Ingress::new(
            TestCatalog::new([fixture().build()]),
            Arc::new(host()),
            store.clone(),
            MemoryPrivate::new(),
        )
        .with_uid_resolver(|account| Ok(account.uid.unwrap()))
        .with_tracer(tracer.clone()),
    );
    // run-1을 만든 접수
    ingress.admit(cli("r1", "hi")).await;
    let secrets = MemorySecrets::new(&[("personal", "bot-token", b"test-bot-1")]);
    let gateway = SlackGateway::new(
        ingress.clone(),
        MemoryBroker::new(),
        MemoryLedger::new(),
        secrets,
        SlackApi::new(Some(&slack.base)),
    );
    let deliverer = Deliverer::new(outbox.clone(), ingress, Arc::new(gateway.sender()));
    let dm_destination = Conversation {
        kind: ConversationKind::Direct,
        id: "D1".into(),
        thread: Some("1700000001.000100".into()),
    };
    outbox.push(outgoing(
        "personal",
        1,
        "direct",
        "slack:home/U1",
        dm_destination.clone(),
        &json!({"text": MARKER}).to_string(),
    ));
    // 거절되는 회신(없는 주체)과 불명(Held)이 되는 회신도 함께
    outbox.push(outgoing(
        "personal",
        2,
        "direct",
        "slack:home/U9",
        dm_destination,
        r#"{"text":"x"}"#,
    ));
    deliverer.tick().await.unwrap();
    let spans = tracer.named("deliver reply");
    assert_eq!(spans.len(), 2);
    let sent = &spans[0];
    assert_vocabulary(sent);
    assert_eq!(sent.str(key::BOT_OUTCOME), Some("confirmed"));
    assert_eq!(sent.str(key::BOT_RUN_ID), Some("run-1"));
    assert_eq!(sent.str(key::BOT_CONVERSATION_KIND), Some("direct"));
    assert_eq!(sent.error, None);
    assert_eq!(spans[1].str(key::BOT_OUTCOME), Some("rejected"));
    assert_eq!(spans[1].error, Some("rejected"));
    // 접수 스팬과 같은 추적(같은 프로필·run)이고, 영수증·본문은 싣지 않는다
    let ingress_span = tracer.named("ingress cli").remove(0);
    assert_eq!(sent.ids.trace, ingress_span.ids.trace);
    assert_ne!(sent.ids.span, ingress_span.ids.span);
    for span in &spans {
        let text = text_of(span);
        assert!(
            !text.contains(MARKER) && !text.contains("1700000000."),
            "{text}"
        );
    }
    // 불명 결과는 오류 분류 held
    slack.script(
        "chat.postMessage",
        bot_gateway::testing::slack::Scripted::Raw {
            status: 500,
            retry_after: None,
            body: String::new(),
        },
    );
    outbox.push(outgoing(
        "personal",
        3,
        "direct",
        "slack:home/U1",
        Conversation {
            kind: ConversationKind::Direct,
            id: "D1".into(),
            thread: None,
        },
        r#"{"text":"again"}"#,
    ));
    deliverer.tick().await.unwrap();
    let held = tracer.named("deliver reply").remove(2);
    assert_eq!(
        (held.str(key::BOT_OUTCOME), held.error),
        (Some("held"), Some("held"))
    );
}
