//! Cross-crate test for human-approval reaction evidence: `Ingress` (`AcceptExtras`) -> PG store -> `BrokerService` one-time cardless approval.

use crate::process_support::Schema;
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
    // Only U1 is a designated approver; U2 is a member so reactions are admitted but not pre-approved.
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
        other => panic!("expected admission: {other:?}"),
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
    // Designated approver (U1): evidence is recorded on accept and consumed once with no card.
    let run = admit(&r, "U1", "Ev1").await;
    assert_eq!(
        r.schema
            .query("select trigger || '/' || actor || '/' || event_id || '/' || kind from {s}.bot_run_evidence"),
        "confirm/slack:home/U1/Ev1/reaction"
    );
    assert_eq!(admit(&r, "U1", "Ev1").await, run);
    assert_eq!(r.schema.count("bot_run_evidence"), "1");
    let ctx = r.store.claim().await.unwrap().expect("run");
    assert_eq!(ctx.attempt.run, run);
    let done = r.broker.call(call_of(&ctx)).await.unwrap();
    assert!(matches!(done, CallResult::Done(_)));
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.schema.count("bot_approvals"), "0", "expected zero cards");
    assert_eq!(
        r.schema
            .query("select team || '/' || event_id || '/' || actor || '/' || tool from {s}.bot_evidence_uses"),
        "T1/Ev1/slack:home/U1/reports.notify"
    );
    // Distinct event (Ev2) provides fresh evidence for the same approver.
    let second = admit(&r, "U1", "Ev2").await;
    assert_ne!(second, run);
    let ctx = r.store.claim().await.unwrap().expect("run");
    assert!(matches!(
        r.broker.call(call_of(&ctx)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.schema.count("bot_evidence_uses"), "2");
    // Non-approver member (U2) falls back to the standard approval card flow.
    admit(&r, "U2", "Ev3").await;
    let ctx = r.store.claim().await.unwrap().expect("run");
    let waiting = r.broker.call(call_of(&ctx)).await.unwrap();
    assert!(matches!(waiting, CallResult::Awaiting(_)));
    assert_eq!(
        r.runner.jobs().len(),
        2,
        "must not execute before card approval"
    );
    assert_eq!(r.schema.count("bot_evidence_uses"), "2");
    assert_eq!(
        r.schema.query(
            "select count(*) from {s}.bot_audit where action = 'call.note' and verdict = 'human_approval_mismatch'"
        ),
        "1"
    );
}
