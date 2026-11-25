//! F4 지정한 사람의 승인 시험 도구: Slack 채널 바인딩 `team`의 반응 트리거 `confirm`이 승인 도구를 고정 인자로 직접 부른다.
#![allow(dead_code)]

use super::*;
use bot_broker::{
    seal::JcsSealer,
    service::{BrokerService, Config as BrokerConfig, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, RecordingTracer, snapshot, tool as fake_tool},
};
use bot_core::{
    grant::HumanApproval,
    pass::{Candidate, PassChain},
    profile::{InputValidator, Snapshot, Trigger, TriggerSource, TriggerTarget},
    slack::{SlackSource, SourceKind},
};
use std::{collections::BTreeMap, sync::Arc};

pub const APPROVER: &str = "slack:home/U1";
pub const OTHER: &str = "slack:home/U2";
pub const TOOL: &str = "reports.notify";
pub const ARGS: &str = r#"{"report":"daily"}"#;

struct AnyArgs;

impl InputValidator for AnyArgs {
    fn validate(&self, _: &JsonPayload, _: &JsonPayload) -> Result<(), bot_core::port::Error> {
        Ok(())
    }
}

pub fn confirm_trigger(approvers: &[&str]) -> Trigger {
    Trigger {
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
            args: JsonPayload(ARGS.into()),
        },
        preapproved: None,
        human_approval: Some(HumanApproval {
            approvers: approvers.iter().map(|a| PrincipalId((*a).into())).collect(),
        }),
    }
}

/// 승인이 필요한 도구(되돌릴 수 없음)를 가진 프로필. `members`에 없는 사람은 멤버가 아니다.
pub fn world(approvers: &[&str], members: &[&str]) -> Arc<Snapshot> {
    let mut notify = fake_tool(Risk::Irreversible, &["reports.post"]);
    notify.server = "reports".into();
    let mut builder = snapshot("personal", "r1")
        .role("svc", &["run", "reports.post"])
        .policy_scopes(&["run", "reports.post"])
        .slack_channel_binding("team", "main");
    for member in members {
        builder = builder.member(member, &["svc"]);
    }
    let mut snap = builder.agent("main").tool("main", TOOL, notify).build();
    Arc::make_mut(&mut snap)
        .agents
        .get_mut(&AgentId("main".into()))
        .unwrap()
        .triggers
        .push(confirm_trigger(approvers));
    snap
}

pub struct Rig {
    pub db: TestDb,
    pub broker: BrokerService,
    pub runner: Arc<FakeRunner>,
    pub catalog: Arc<FakeCatalog>,
}

pub async fn rig(snap: Arc<Snapshot>) -> Option<Rig> {
    let db = TestDb::new().await?;
    db.publish().await;
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(snap);
    let runner = Arc::new(FakeRunner::new());
    runner.set_image("reports", Some("reports-img"));
    let store = Arc::new(db.store.clone());
    let broker = BrokerService::new(
        Parts {
            catalog: catalog.clone(),
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
    )
    .with_schedules(store);
    Some(Rig {
        db,
        broker,
        runner,
        catalog,
    })
}

pub fn target() -> Target {
    Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId(TOOL.into()),
        args: JsonPayload(ARGS.into()),
    }
}

/// 입구가 인증한 이번 반응 이벤트의 원천.
pub fn source(event: &str, ts: &str) -> SlackSource {
    SlackSource {
        kind: SourceKind::Reaction,
        team: "T1".into(),
        channel: "C1".into(),
        ts: ts.into(),
        thread_ts: None,
        event_id: event.into(),
        reaction: Some("white_check_mark".into()),
    }
}

pub fn chain(trigger: &str) -> PassChain {
    PassChain {
        candidates: vec![Candidate {
            trigger: trigger.into(),
            target: target(),
        }],
        default: None,
    }
}

pub fn event_accept(request: &str, actor: &str) -> Accept {
    let mut a = accept_as(
        "personal",
        request,
        actor,
        ConversationKind::Channel,
        "C1",
        "react",
    );
    a.event.binding = BindingId("team".into());
    a.target = target();
    a.scopes = scopes(&["run", "reports.post"]);
    a
}

impl Rig {
    /// 인증된 이벤트로 접수하고 claim한다.
    pub async fn accept(
        &self,
        request: &str,
        actor: &str,
        extras: AcceptExtras,
    ) -> (RunId, Option<RunContext>) {
        let run = runs(&self.db)
            .accept_with(event_accept(request, actor), extras)
            .await
            .unwrap();
        (run, runs(&self.db).claim().await.unwrap())
    }

    pub async fn react(&self, request: &str, actor: &str, event: &str) -> RunContext {
        let extras = AcceptExtras {
            source: Some(source(event, "1.1")),
            chain: Some(chain("confirm")),
        };
        self.accept(request, actor, extras).await.1.unwrap()
    }

    pub fn call_of(ctx: &RunContext) -> ToolCall {
        ToolCall {
            attempt: ctx.attempt.clone(),
            call: CallId(format!("{}:tool", ctx.attempt.run.0)),
            tool: ToolId(TOOL.into()),
            args: JsonPayload(ARGS.into()),
        }
    }

    pub async fn count(&self, sql: &str) -> i64 {
        self.db.scalar::<i64>(sql).await
    }

    pub async fn verdicts(&self, action: &str) -> Vec<String> {
        sqlx::query_scalar(&format!(
            "select verdict from bot_audit where action = '{action}' order by seq"
        ))
        .fetch_all(self.db.store.pool())
        .await
        .unwrap()
    }
}
