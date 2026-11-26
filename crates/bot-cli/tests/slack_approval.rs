//! 지정한 사람의 승인 교차 시험: 인증된 Slack 반응 → 실제 게이트웨이 `Ingress` → 실제 PG 저장소(증거 접수) → 실제 브로커(카드 없이 한 번 승인).
//! 게이트웨이(④)가 만드는 `AcceptExtras{source, chain}`가 저장소(③)의 증거 조건과 실제로 맞물리는지 확인한다. 시험 DB가 없으면 건너뛴다.

mod process_support;

use bot_broker::{
    seal::JcsSealer,
    service::{BrokerService, Config as BrokerConfig, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, RecordingTracer, snapshot, tool},
};
use bot_core::{
    command::{CallResult, JsonPayload, ToolCall},
    grant::HumanApproval,
    id::*,
    port::{Broker, RunStore},
    profile::{Adapter, Binding, InputValidator, Trigger, TriggerSource, TriggerTarget},
    slack::{SlackEvent, SlackSource, SourceKind},
    state::{Conversation, ConversationKind, Risk},
};
use bot_gateway::{
    entrance::Real,
    host::HostConfig,
    inbound::{Event, Inbound},
    ingress::{Admission, Ingress},
    testing::MemoryPrivate,
};
use bot_store::{Config, Store};
use process_support::Schema;
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc};

const HOST: &str = r#"{
    "slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token",
        "channels": {"planning": "C1"}}}}
}"#;
const TOOL: &str = "reports.notify";

struct AnyArgs;

impl InputValidator for AnyArgs {
    fn validate(&self, _: &JsonPayload, _: &JsonPayload) -> Result<(), bot_core::port::Error> {
        Ok(())
    }
}

struct Rig {
    schema: Schema,
    store: Arc<Store>,
    ingress: Ingress,
    broker: BrokerService,
    runner: Arc<FakeRunner>,
}

async fn rig() -> Option<Rig> {
    let schema = Schema::create()?;
    let store = Store::connect(&schema.url(), Config::default())
        .await
        .unwrap();
    store.migrate().await.unwrap();
    // 승인이 필요한 도구를 고정 인자로 부르는 반응 트리거: U1만 승인자다(U2도 멤버라 반응은 접수된다)
    let mut notify = tool(Risk::Irreversible, &["reports.post"]);
    notify.server = "reports".into();
    let mut snap = snapshot("personal", "r1")
        .role("svc", &["run", "reports.post"])
        .member("slack:home/U1", &["svc"])
        .member("slack:home/U2", &["svc"])
        .policy_scopes(&["run", "reports.post"])
        .agent("main")
        .tool("main", TOOL, notify)
        .build();
    {
        let inner = Arc::make_mut(&mut snap);
        inner.bindings.insert(
            BindingId("team".into()),
            Binding {
                agent: AgentId("main".into()),
                source: Adapter::SlackChannel {
                    workspace: "home".into(),
                    channel: "planning".into(),
                    events: [SlackEvent::Reaction].into(),
                },
            },
        );
        inner
            .agents
            .get_mut(&AgentId("main".into()))
            .unwrap()
            .triggers
            .push(Trigger {
                id: "confirm".into(),
                on: TriggerSource::Event {
                    binding: BindingId("team".into()),
                    event: "reaction".into(),
                },
                order: 0,
                conditions: BTreeMap::new(),
                input: BTreeMap::new(),
                private: false,
                private_pass: false,
                target: TriggerTarget::Tool {
                    tool: ToolId(TOOL.into()),
                    args: JsonPayload(r#"{"report":"daily"}"#.into()),
                },
                preapproved: None,
                human_approval: Some(HumanApproval {
                    approvers: vec![PrincipalId("slack:home/U1".into())],
                }),
            });
    }
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(snap);
    bot_core::profile::ActiveRevisions::replace(
        &store,
        None,
        Revision("set-1".into()),
        BTreeMap::from([(ProfileId("personal".into()), Revision("r1".into()))]),
        vec![],
        Default::default(),
    )
    .await
    .unwrap();
    let store = Arc::new(store);
    let host = Arc::new(HostConfig::from_json(HOST).unwrap());
    let ingress = Ingress::new(
        catalog.clone(),
        host,
        store.clone() as Arc<dyn RunStore>,
        MemoryPrivate::new(),
    );
    let runner = Arc::new(FakeRunner::new());
    runner.set_image("reports", Some("reports-img"));
    let broker = BrokerService::new(
        Parts {
            catalog,
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store.clone(),
            runner: runner.clone(),
            sealer: Arc::new(JcsSealer::new(Arc::new(AnyArgs))),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        BrokerConfig::default(),
    );
    Some(Rig {
        schema,
        store,
        ingress,
        broker,
        runner,
    })
}

/// 게이트웨이가 반응 프레임에서 만드는 접수 입력과 인증된 원천(`SlackGateway::on_reaction`과 같은 모양).
fn reaction(user: &str, event: &str) -> (Inbound, SlackSource) {
    let inbound = Inbound {
        real: Real::SlackChannel {
            team: "T1".into(),
            channel: "C1".into(),
        },
        principal: PrincipalId(format!("slack:home/{user}")),
        request: RequestId(event.into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: "C1".into(),
            thread: Some("1.1".into()),
        },
        attachments: vec![],
        body: json!({"reaction": "white_check_mark", "channel": "C1", "item_ts": "1.1", "item_user": "U9"}),
        event: Event::Reaction,
    };
    let source = SlackSource {
        kind: SourceKind::Reaction,
        team: "T1".into(),
        channel: "C1".into(),
        ts: "1.1".into(),
        thread_ts: None,
        event_id: event.into(),
        reaction: Some("white_check_mark".into()),
    };
    (inbound, source)
}

async fn admit(r: &Rig, user: &str, event: &str) -> RunId {
    let (inbound, source) = reaction(user, event);
    let routes = r.ingress.routes().unwrap();
    match r
        .ingress
        .admit_sourced(&routes, inbound, Some(source))
        .await
    {
        Admission::Accepted { run, .. } => run,
        other => panic!("접수돼야 한다: {other:?}"),
    }
}

fn call_of(ctx: &bot_core::state::RunContext) -> ToolCall {
    ToolCall {
        attempt: ctx.attempt.clone(),
        call: CallId(format!("{}:tool", ctx.attempt.run.0)),
        tool: ToolId(TOOL.into()),
        args: JsonPayload(r#"{"report":"daily"}"#.into()),
    }
}

#[tokio::test]
async fn an_authenticated_reaction_approves_once_without_a_card_and_others_get_the_card() {
    let Some(r) = rig().await else {
        return;
    };
    // 지정한 사람(U1)의 반응: 접수 커밋에 증거가 남고, 시작 커밋에서 한 번 소비되며 카드는 없다
    let run = admit(&r, "U1", "Ev1").await;
    assert_eq!(
        r.schema
            .query("select trigger || '/' || actor || '/' || event_id || '/' || kind from {s}.bot_run_evidence"),
        "confirm/slack:home/U1/Ev1/reaction"
    );
    // 같은 이벤트의 재전달은 같은 run으로 수렴한다(증거도 한 줄)
    assert_eq!(admit(&r, "U1", "Ev1").await, run);
    assert_eq!(r.schema.count("bot_run_evidence"), "1");
    let ctx = r.store.claim().await.unwrap().expect("run");
    assert_eq!(ctx.attempt.run, run);
    let done = r.broker.call(call_of(&ctx)).await.unwrap();
    assert!(matches!(done, CallResult::Done(_)));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.schema.count("bot_approvals"), "0", "카드 0");
    assert_eq!(
        r.schema
            .query("select team || '/' || event_id || '/' || actor || '/' || tool from {s}.bot_evidence_uses"),
        "T1/Ev1/slack:home/U1/reports.notify"
    );
    // 다른 이벤트(Ev2)는 새 증거다: 같은 사람이 다시 반응하면 다시 승인된다(사람이 아니라 이벤트가 증거다)
    let second = admit(&r, "U1", "Ev2").await;
    assert_ne!(second, run);
    let ctx = r.store.claim().await.unwrap().expect("run");
    assert!(matches!(
        r.broker.call(call_of(&ctx)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.schema.count("bot_evidence_uses"), "2");
    // 지정하지 않은 멤버(U2)의 반응은 접수되지만 증거로 승인되지 않는다 → 기존 카드 규칙(요청자 본인 승인)
    admit(&r, "U2", "Ev3").await;
    let ctx = r.store.claim().await.unwrap().expect("run");
    let waiting = r.broker.call(call_of(&ctx)).await.unwrap();
    assert!(matches!(waiting, CallResult::Awaiting(_)));
    assert_eq!(r.runner.jobs().len(), 2, "카드를 거치기 전에는 실행 0");
    assert_eq!(r.schema.count("bot_evidence_uses"), "2");
    assert_eq!(
        r.schema.query(
            "select count(*) from {s}.bot_audit where action = 'call.note' and verdict = 'human_approval_mismatch'"
        ),
        "1"
    );
}
