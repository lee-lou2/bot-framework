//! 위임 슬롯과 프런트의 확정 거절 표식: 프런트가 위임 호출을 거절 단어로 닫았으면(자식을 만들기 전) 복원·끝남 확인·자리 마무리가 모두 모델이 처음 본 그 단어를
//! 그대로 되살리고 자식을 만들지 않는다. 권한을 잃었다가 다시 얻어도 같다. 기존 자식(완료 결과·저장소 대조 실패)이 있으면 표식이 아니라 그쪽이 우선이다.

use super::fakes::*;
use crate::{
    host::Relays,
    model::{
        ModelRequest, ModelRunner, ModelStep, RelayChannel, RelayGrant, RunnerKind, Session,
        SessionError, Slot, StepFuture, ToolOutput,
        session::{Pending, ToolUse},
    },
    worker::Outcome,
};
use bot_core::{cli::CliRefusal, command::*, id::*, port::Error, state::RunContext};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct Grab(Arc<Mutex<Option<Session>>>);

impl ModelRunner for Grab {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }
    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        *lock(&self.0) = request.session;
        Box::pin(async {
            Ok(ModelStep::Final {
                text: "답".into(),
                usage: Default::default(),
            })
        })
    }
}

struct Open;

impl Relays for Open {
    fn open(&self, _: &Attempt, _: &AgentId, _: &str) -> Option<RelayGrant> {
        Some(RelayGrant::new(
            RelayChannel {
                name: "relay".into(),
                base_url: "http://192.168.64.1:8787".into(),
                token: "tok".into(),
            },
            (),
        ))
    }
}

async fn session_of(ctx: RunContext) -> (Rig, Session) {
    let grabbed = Arc::new(Mutex::new(None));
    let model: Arc<dyn ModelRunner> = Arc::new(Grab(grabbed.clone()));
    let rig = rig_with_relays(model, Arc::new(Open));
    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    let session = lock(&grabbed)
        .take()
        .expect("릴레이가 열려 세션 포트가 있다");
    (rig, session)
}

fn delegate(input: serde_json::Value) -> Pending {
    Pending {
        turn: 7,
        pos: 0,
        tool_use: ToolUse {
            id: "toolu_7_0".into(),
            name: "mcp__relay__delegate".into(),
            input,
        },
    }
}

fn valid() -> serde_json::Value {
    json!({"agent": "main/research", "instruction": "조사해"})
}

fn mark(rig: &Rig, word: &str) {
    lock(&rig.ledger.steps).insert(
        CliRefusal::key(&CallId::cli(7, 0)),
        JsonPayload(json!(word).to_string()),
    );
}

fn text(out: &ToolOutput) -> (serde_json::Value, bool) {
    (out.content[0]["text"].clone(), out.is_error)
}

fn closed(slot: Slot) -> ToolOutput {
    match slot {
        Slot::Result(output) => output,
        Slot::Parked => panic!("결과가 있어야 한다"),
    }
}

/// 지금 core가 아는 거절 단어들(표식으로 읽히는 것만 걸러서 순회하므로 단어가 늘어도 같은 시험이 그대로 덮는다).
fn words() -> Vec<&'static str> {
    [
        "unknown_tool",
        "invalid",
        "denied",
        "conflict",
        "stale",
        "unavailable",
        "secret_missing",
        "destination_unsupported",
        "destination_unregistered",
        "scope_denied",
        "rate_limited",
    ]
    .into_iter()
    .filter(|w| CliRefusal::try_from((*w).to_owned()).is_ok())
    .collect()
}

#[tokio::test]
async fn a_front_refusal_on_a_delegate_is_replayed_without_creating_a_child_even_after_the_permission_returns()
 {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    assert!(words().len() >= 6, "{:?}", words());
    for word in words() {
        mark(&rig, word);
        // 자식 없음 + 표식: 끝난 호출이고, 되살릴 때도 마무리할 때도 처음 본 단어다
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        let recalled = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(text(&recalled), (json!(word), true), "{word}");
        let settled = closed(port.settle(&slot).await.unwrap());
        assert_eq!(text(&settled), (json!(word), true), "{word}");

        // 권한을 잃은 뒤에도 «denied»가 아니라 같은 단어다(저장소는 읽지 않는다)
        let reads = lock(&rig.delegation.reads).len();
        let mut changed = snapshot();
        changed.agents.remove(&AgentId("main/research".into()));
        *lock(&rig.catalog.current) = Arc::new(changed);
        let lost = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(text(&lost), (json!(word), true), "{word}: 권한 상실");
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        assert_eq!(lock(&rig.delegation.reads).len(), reads);

        // 권한을 다시 얻어도 같은 단어이고 자식은 만들지 않는다
        *lock(&rig.catalog.current) = rig.catalog.pinned.clone();
        let again = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(text(&again), (json!(word), true), "{word}: 권한 회복");
        let settled = closed(port.settle(&slot).await.unwrap());
        assert_eq!(text(&settled), (json!(word), true), "{word}: 권한 회복");

        // 입력이 틀린 위임도 같은 단어다
        let broken = delegate(json!({"agent": 5}));
        let out = port.recall(&broken).await.unwrap().unwrap();
        assert_eq!(text(&out), (json!(word), true), "{word}: 잘못된 입력");
    }
    assert!(
        lock(&rig.delegation.requests).is_empty(),
        "거절한 위임은 자식을 만들지 않는다"
    );
    assert!(rig.broker.calls().is_empty());
}

#[tokio::test]
async fn an_existing_child_or_a_store_mismatch_wins_over_the_mark() {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    mark(&rig, "conflict");
    // 끝난 자식이 있으면 표식이 아니라 그 결과다
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": "답"}).to_string()),
    );
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("답"), false));
    assert_eq!(port.ended(&slot).await, Ok(true));
    lock(&rig.delegation.results).clear();
    // 저장소가 기존 자식과 어긋난다고 하면 표식이 있어도 `denied`다(저장소가 더 구체적이다)
    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("denied"), true));
    // 소유권을 잃었으면 표식과 무관하게 물러난다
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.recall(&slot).await, Err(SessionError::Lost));
}

#[tokio::test]
async fn without_a_readable_mark_a_delegate_is_judged_and_created_as_before() {
    let (rig, session) = session_of(agent_run("r1")).await;
    let port = session.0;
    let slot = delegate(valid());
    // 표식이 없거나 읽을 수 없으면 없는 것이다: 끝난 것이 없고, 마무리하면 같은 호출 ID로 자식을 만든다
    for junk in [None, Some("\"nope\""), Some("{\"invalid\":null}")] {
        if let Some(junk) = junk {
            lock(&rig.ledger.steps).insert(
                CliRefusal::key(&CallId::cli(7, 0)),
                JsonPayload(junk.into()),
            );
        }
        assert_eq!(port.ended(&slot).await, Ok(false), "{junk:?}");
        assert_eq!(port.recall(&slot).await, Ok(None), "{junk:?}");
    }
    assert!(lock(&rig.delegation.requests).is_empty());
    assert!(matches!(port.settle(&slot).await, Ok(Slot::Parked)));
    assert_eq!(lock(&rig.delegation.requests).len(), 1);
    // 표식이 없을 때 지금 권한이 없으면 예전처럼 `denied`다
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    lock(&rig.ledger.steps).clear();
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(text(&out), (json!("denied"), true));
}
