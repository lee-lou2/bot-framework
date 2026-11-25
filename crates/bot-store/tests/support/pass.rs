//! F3 넘김 시험 도구(pass_chain·pass_recovery가 함께 쓴다): 실제 BrokerService·PG 원장·가짜 도구 실행기와 사슬 접수 도우미.
#![allow(dead_code)]

use super::*;
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot, tool},
};
use bot_core::{
    command::*,
    id::*,
    pass::{Candidate, PassChain, direct_call_id},
    port::{Broker, Error},
    state::{Risk, RunContext},
};
use std::sync::Arc;

pub const PASS: &str = r#"{"_meta":{"bot/pass":true},"content":[],"isError":false}"#;
pub const PLAIN: &str = r#"{"content":[{"text":"handled","type":"text"}],"isError":false}"#;

pub struct Rig {
    pub broker: BrokerService,
    pub runner: Arc<FakeRunner>,
    /// 시험이 새 판본을 게시해 «접수 뒤에 권한이 바뀐» 상황을 만든다
    pub catalog: Arc<FakeCatalog>,
}

/// 프로필 `personal`의 한 판본: `owner` 역할이 `owner_scopes`를 갖는다(정책 상한은 `run`·`calendar.read`로 같다).
pub fn world(revision: &str, owner_scopes: &[&str]) -> Arc<bot_core::profile::Snapshot> {
    let mut post = tool(Risk::Write, &["calendar.read"]);
    post.effects = true;
    snapshot("personal", revision)
        .role("owner", owner_scopes)
        .member("local:owner", &["owner"])
        .policy_scopes(&["run", "calendar.read"])
        .tool("main", "agenda.list", tool(Risk::Read, &["calendar.read"]))
        .tool("main", "agenda.find", tool(Risk::Read, &["calendar.read"]))
        .tool("main", "agenda.post", post)
        .tool(
            "main",
            "agenda.write",
            tool(Risk::Write, &["calendar.read"]),
        )
        .build()
}

pub fn rig(db: &TestDb) -> Rig {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(world("r1", &["run", "calendar.read"]));
    let store = Arc::new(db.store.clone());
    let runner = Arc::new(FakeRunner::new());
    let broker = BrokerService::new(
        Parts {
            catalog: catalog.clone(),
            ledger: store.clone(),
            effects: store.clone(),
            memory: store.clone(),
            audit: store,
            runner: runner.clone(),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    );
    Rig {
        broker,
        runner,
        catalog,
    }
}

pub fn tool_target(name: &str) -> Target {
    Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId(name.into()),
        args: JsonPayload("{}".into()),
    }
}

pub fn default_target() -> Target {
    Target::Agent {
        agent: AgentId("main".into()),
        instruction: "hello".into(),
    }
}

/// 후보 이름들(도구)과 기본 대화로 사슬을 만든다. 후보 ID는 `t<위치>`다.
pub fn chain(tools: &[&str], default: Option<Target>) -> PassChain {
    PassChain {
        candidates: tools
            .iter()
            .enumerate()
            .map(|(i, name)| Candidate {
                trigger: format!("t{i}"),
                target: tool_target(name),
            })
            .collect(),
        default,
    }
}

pub async fn accept_chain(db: &TestDb, request: &str, chain: PassChain) -> RunId {
    accept_chain_scoped(db, request, chain, run_scopes()).await
}

/// 요청자의 스코프를 정해 사슬을 접수한다(`run`이 없는 요청자 등).
pub async fn accept_chain_scoped(
    db: &TestDb,
    request: &str,
    chain: PassChain,
    scopes: std::collections::BTreeSet<String>,
) -> RunId {
    accept_chain_at(db, request, chain, scopes, "r1").await
}

/// 활성 판본이 `revision`일 때 그 판본에 고정해 접수한다(접수는 활성 판본과 같아야 한다).
pub async fn accept_chain_at(
    db: &TestDb,
    request: &str,
    chain: PassChain,
    scopes: std::collections::BTreeSet<String>,
    revision: &str,
) -> RunId {
    let first = chain.candidates[0].target.clone();
    let mut command = tool_accept("personal", request, "agenda.list", "{}");
    command.target = first;
    command.scopes = scopes;
    command.revision = Revision(revision.into());
    runs(db)
        .accept_with(
            command,
            AcceptExtras {
                source: None,
                chain: Some(chain),
            },
        )
        .await
        .unwrap()
}

pub async fn call_at(rig: &Rig, ctx: &RunContext) -> Result<CallResult, Error> {
    let pos = ctx.work.pass_pos.unwrap_or(0);
    let Target::Tool { tool, args, .. } = ctx.work.target.clone() else {
        panic!("직접 호출 차례가 아니다");
    };
    rig.broker
        .call(ToolCall {
            attempt: ctx.attempt.clone(),
            call: direct_call_id(&ctx.attempt.run, pos),
            tool,
            args,
        })
        .await
}

pub async fn run_state(db: &TestDb, run: &RunId) -> (String, Option<i32>, String) {
    sqlx::query_as(&format!(
        "select state, pass_pos, target->>'tool' from bot_runs where id = '{}'",
        run.0
    ))
    .fetch_one(db.store.pool())
    .await
    .map(
        |(state, pos, tool): (String, Option<i32>, Option<String>)| {
            (state, pos, tool.unwrap_or_else(|| "(agent)".into()))
        },
    )
    .unwrap()
}

pub async fn audit_actions(db: &TestDb) -> Vec<String> {
    sqlx::query_scalar("select action from bot_audit order by seq")
        .fetch_all(db.store.pool())
        .await
        .unwrap()
}

/// 저장소의 활성 판본 집합에서 `personal`을 이 판본으로 바꾼다(브로커 카탈로그의 `publish`와 짝이다: 호출 시작은 둘이 같은 판본이어야 한다).
pub async fn activate(
    db: &TestDb,
    rig: &Rig,
    snapshot: std::sync::Arc<bot_core::profile::Snapshot>,
) {
    use bot_core::profile::ActiveRevisions;
    let revision = snapshot.revision.clone();
    rig.catalog.publish(snapshot);
    let expected = db.store.current().await.unwrap();
    let profiles = std::collections::BTreeMap::from([
        (ProfileId("personal".into()), revision.clone()),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    db.store
        .replace(
            expected,
            Revision(format!("set-{}", revision.0)),
            profiles,
            vec![],
            Default::default(),
        )
        .await
        .unwrap();
}
