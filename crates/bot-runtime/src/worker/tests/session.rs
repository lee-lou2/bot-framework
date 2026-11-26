//! CLI 세션 포트(`RunSession`): 워커가 Loop 실행기에 주는 체크포인트 쓰기·프런트 확인 읽기·결과 진위·자리 마무리.
//! 실행기가 받은 포트를 그대로 잡아 두었다가 호출한다(실제 CLI 없이 호스트 쪽 규칙만 본다).

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
use bot_core::{
    cli::{CliResponse, CliResponseUse},
    command::*,
    id::*,
    ledger::{CallPhase, CallRecord},
    port::Error,
    state::RunContext,
};
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
                outcome: Default::default(),
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

/// 실행기가 받은 세션 포트를 잡는다(릴레이가 열려 있을 때만 포트가 있다).
async fn session_of(ctx: RunContext, relays: bool) -> (Rig, Option<Session>) {
    let grabbed = Arc::new(Mutex::new(None));
    let model: Arc<dyn ModelRunner> = Arc::new(Grab(grabbed.clone()));
    let rig = if relays {
        rig_with_relays(model, Arc::new(Open))
    } else {
        rig(model)
    };
    assert_eq!(rig.worker.execute(ctx).await, Outcome::Succeeded);
    let session = lock(&grabbed).take();
    (rig, session)
}

fn pending(turn: u64, pos: u32, name: &str, input: serde_json::Value) -> Pending {
    Pending {
        turn,
        pos,
        tool_use: ToolUse {
            id: format!("toolu_{turn}_{pos}"),
            name: name.into(),
            input,
        },
    }
}

fn output(slot: Slot) -> ToolOutput {
    match slot {
        Slot::Result(output) => output,
        Slot::Parked => panic!("결과가 있어야 한다"),
    }
}

#[tokio::test]
async fn a_loop_run_gets_a_session_port_only_when_a_relay_is_open() {
    let (_, session) = session_of(agent_run("r1"), true).await;
    assert!(session.is_some());
    let (_, session) = session_of(agent_run("r2"), false).await;
    assert!(
        session.is_none(),
        "채널이 없으면 CLI가 돌지 않으니 세션도 없다"
    );
}

#[tokio::test]
async fn the_port_sees_the_claim_steps_and_commits_a_batch_through_the_run_store() {
    let mut ctx = agent_run("r1");
    ctx.work.steps.push(Step {
        key: "cli-rec-0".into(),
        body: JsonPayload("\"{}\"".into()),
    });
    let (rig, session) = session_of(ctx, true).await;
    let port = session.unwrap().0;
    assert_eq!(port.steps().len(), 1);
    let batch = vec![
        Step {
            key: "cli-rec-1".into(),
            body: JsonPayload("\"a\"".into()),
        },
        Step {
            key: "cli-group-1".into(),
            body: JsonPayload("{}".into()),
        },
    ];
    port.commit(batch).await.unwrap();
    assert_eq!(
        rig.runs.events(),
        ["checkpoint:cli-rec-1", "checkpoint:cli-group-1"]
    );
    // 같은 키에 다른 내용은 묶음 전체를 되돌린다(영구 오류)
    let clash = vec![Step {
        key: "cli-rec-1".into(),
        body: JsonPayload("\"b\"".into()),
    }];
    assert_eq!(
        port.commit(clash).await,
        Err(SessionError::Permanent("checkpoint_conflict"))
    );
}

#[tokio::test]
async fn the_front_confirmed_response_is_read_and_validated() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    assert_eq!(port.response("msg_1").await, Ok(None));
    let good = CliResponse {
        id: "msg_1".into(),
        uses: vec![CliResponseUse {
            id: "toolu_1".into(),
            name: "mcp__relay__agenda_list".into(),
            input: json!({}),
        }],
    };
    lock(&rig.ledger.steps).insert(
        "cli-resp-msg_1".into(),
        JsonPayload(serde_json::to_string(&good).unwrap()),
    );
    assert_eq!(port.response("msg_1").await, Ok(Some(good)));
    // 모양이 틀린 기록은 믿지 않는다
    lock(&rig.ledger.steps).insert(
        "cli-resp-msg_2".into(),
        JsonPayload("{\"id\":\"msg_2\",\"uses\":[]}".into()),
    );
    assert_eq!(
        port.response("msg_2").await,
        Err(SessionError::Permanent("cli_response_invalid"))
    );
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.response("msg_1").await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn recalling_an_answered_tool_reads_only_finished_calls_through_the_broker_and_never_executes()
 {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"b": 1, "a": 2}));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    // 기록이 없거나 끝나지 않은 호출은 읽을 것이 없고 브로커도 부르지 않는다(새 실행 금지)
    assert_eq!(port.recall(&slot).await, Ok(None));
    lock(&rig.ledger.calls).insert("cli-7-0".into(), record(CallPhase::Open { approval: None }));
    assert_eq!(port.recall(&slot).await, Ok(None));
    assert!(rig.broker.calls().is_empty());
    // 끝난 호출은 같은 호출 ID·정규 인자로 브로커에서 다시 읽는다(재인가)
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    rig.broker.script(done(
        r#"{"content":[{"type":"text","text":"실제"}],"isError":true}"#,
    ));
    let out = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(
        (out.content[0]["text"].clone(), out.is_error),
        (json!("실제"), true)
    );
    let sent = rig.broker.calls();
    assert_eq!((sent.len(), sent[0].call.0.as_str()), (1, "cli-7-0"));
    assert_eq!(sent[0].args.0, r#"{"a":2,"b":1}"#);
    // 확정 거절(승인 거절·권한 거절)로 끝난 호출은 같은 도구·인자일 때 실행 없이 거절 단어로 복원한다 — 몇 번을 재개해도 같다
    let canonical_hash = bot_core::canon::sha256(r#"{"a":2,"b":1}"#);
    let denied_record = |hash: [u8; 32], tool_id: &str| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId(tool_id.into()),
        args_sha256: hash,
        phase: CallPhase::Denied,
    };
    let calls_before = rig.broker.calls().len();
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        denied_record(canonical_hash, "agenda.list"),
    );
    for _ in 0..2 {
        let again = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(
            (again.content[0]["text"].clone(), again.is_error),
            (json!("denied"), true)
        );
    }
    assert_eq!(
        rig.broker.calls().len(),
        calls_before,
        "거절은 브로커를 다시 부르지 않는다"
    );
    // 거절 기록의 지문은 승인 지문(기억 공유의 본문 지문·예약의 확정 시간대가 더해진 값)이라 모델 입력 해시와 달라도 같은 자리의 거절이다
    lock(&rig.ledger.calls).insert("cli-7-0".into(), denied_record([9; 32], "agenda.list"));
    for _ in 0..2 {
        let sealed = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(sealed.content[0]["text"].clone(), json!("denied"));
    }
    // 도구가 다르면 그 거절이 아니니 믿을 결과가 없다
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        denied_record(canonical_hash, "agenda.delete"),
    );
    assert_eq!(port.recall(&slot).await, Ok(None));
    // 끝난 호출이 지금은 거절되면 고정 단어다
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    rig.broker.script(Err(Error::Denied));
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(
        (denied.content[0]["text"].clone(), denied.is_error),
        (json!("denied"), true)
    );
    // VM 내장 도구는 호스트가 아는 결과가 없다
    let read = pending(7, 1, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    assert_eq!(port.recall(&read).await, Ok(None));
}

#[tokio::test]
async fn a_call_has_ended_only_when_the_ledger_shows_it_done_or_confirmed_denied() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"a": 1}));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    // 기록이 없거나 열려 있거나 보류인 호출은 끝나지 않았다(실행되지 못했다)
    assert_eq!(port.ended(&slot).await, Ok(false));
    for phase in [
        CallPhase::Open { approval: None },
        CallPhase::Held {
            reason: "timeout".into(),
            resolution: None,
        },
    ] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(false));
    }
    // 완료와 확정 거절은 끝난 것이다. 확인만 하고 브로커를 부르지 않는다(재읽기·실행 없음)
    for phase in [CallPhase::Done(JsonPayload("{}".into())), CallPhase::Denied] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(true));
    }
    assert!(rig.broker.calls().is_empty());
    // 실행할 것이 없는 자리(VM 내장·모르는 도구·잘못된 입력)는 기록이 없어도 끝났다
    let read = pending(7, 1, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    assert_eq!(port.ended(&read).await, Ok(true));
    // 프런트가 계획·원장 전에 거절하는 호출(풀 수 없는 이름·객체가 아닌 인자·호스트 전용 도구)도 기록이 없어도 끝났고, 모델이 본 단어로 되살린다
    for (name, input, want) in [
        ("mcp__relay__a_b_c", json!({}), "invalid"),
        ("mcp__relay__agenda_list", json!([1]), "invalid"),
        ("mcp__relay__schedule_deliver", json!({}), "unknown_tool"),
    ] {
        let refused = pending(7, 2, name, input);
        assert_eq!(port.ended(&refused).await, Ok(true), "{name}");
        let recalled = port.recall(&refused).await.unwrap().unwrap();
        assert_eq!(recalled.content[0]["text"].clone(), json!(want), "{name}");
        let Slot::Result(settled) = port.settle(&refused).await.unwrap() else {
            panic!("{name}")
        };
        assert_eq!(settled.content[0]["text"].clone(), json!(want), "{name}");
    }
    assert!(
        rig.broker.calls().is_empty(),
        "거절한 호출은 브로커를 부르지 않는다"
    );
    // 이름이 풀리고 인자가 객체여도 원장에 끝난 기록이 없으면 실행되지 못한 호출이다(모르는 도구·검증 실패는 프런트가 기록하지 않는 한 구분할 수 없다)
    let unrecorded = pending(7, 3, "mcp__relay__agenda_other", json!({}));
    assert_eq!(port.ended(&unrecorded).await, Ok(false));
    // 원장을 읽지 못하면 모른다
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.ended(&slot).await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn a_confirmed_refusal_mark_closes_the_slot_with_the_same_word_and_never_executes() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__agenda_list", json!({"a": 1}));
    let mark = |slot: &Pending, value: serde_json::Value| {
        lock(&rig.ledger.steps).insert(
            bot_core::cli::CliRefusal::key(&CallId::cli(slot.turn, slot.pos)),
            JsonPayload(value.to_string()),
        );
    };
    let text = |out: ToolOutput| (out.content[0]["text"].clone(), out.is_error);
    // 표식이 없으면 미실행이다
    assert_eq!(port.ended(&slot).await, Ok(false));
    assert_eq!(port.recall(&slot).await, Ok(None));
    // 한도 초과로 저장한 unavailable까지, 각 위치에서 모델이 본 오류를 그대로 복원한다.
    for (pos, word) in (1..).zip([
        "unknown_tool",
        "invalid",
        "denied",
        "conflict",
        "stale",
        "unavailable",
    ]) {
        let slot = pending(7, pos, "mcp__relay__agenda_list", json!({"a": 1}));
        mark(&slot, json!(word));
        assert_eq!(port.ended(&slot).await, Ok(true), "{word}");
        assert_eq!(
            text(port.recall(&slot).await.unwrap().unwrap()),
            (json!(word), true)
        );
        assert_eq!(
            text(output(port.settle(&slot).await.unwrap())),
            (json!(word), true)
        );
    }
    assert!(
        rig.broker.calls().is_empty(),
        "거절한 호출은 브로커를 부르지 않는다"
    );
    // 감사 사유 또는 JSON 객체는 종료된 호출을 증명하지 못한다.
    for junk in [json!("secret_missing"), json!({"invalid": null})] {
        mark(&slot, junk.clone());
        assert_eq!(port.ended(&slot).await, Ok(false), "{junk}");
        assert_eq!(port.recall(&slot).await, Ok(None), "{junk}");
    }
    // 원장에 완료·시작·보류가 있으면 표식이 아니라 원장을 따른다
    mark(&slot, json!("invalid"));
    let record = |phase| CallRecord {
        call: CallId::cli(7, 0),
        tool: ToolId("agenda.list".into()),
        args_sha256: [0; 32],
        phase,
    };
    for phase in [
        CallPhase::Started {
            generation: Generation(1),
            owner: "o".into(),
            confirmed_step: false,
        },
        CallPhase::Held {
            reason: "timeout".into(),
            resolution: None,
        },
    ] {
        lock(&rig.ledger.calls).insert("cli-7-0".into(), record(phase));
        assert_eq!(port.ended(&slot).await, Ok(false));
        assert_eq!(port.recall(&slot).await, Ok(None));
    }
    lock(&rig.ledger.calls).insert(
        "cli-7-0".into(),
        record(CallPhase::Done(JsonPayload("{}".into()))),
    );
    assert_eq!(port.ended(&slot).await, Ok(true));
    // 열려 있기만 한 호출은 표식이 닫는다
    lock(&rig.ledger.calls).insert("cli-7-0".into(), record(CallPhase::Open { approval: None }));
    assert_eq!(
        text(port.recall(&slot).await.unwrap().unwrap()),
        (json!("invalid"), true)
    );
    // 표식이 없는 위치에서 저장소를 읽지 못하면 종료와 복원 모두 재시도한다.
    let unmarked = pending(8, 0, "mcp__relay__agenda_list", json!({}));
    rig.ledger
        .down
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(port.ended(&unmarked).await, Err(SessionError::Unavailable));
    assert_eq!(port.recall(&unmarked).await, Err(SessionError::Unavailable));
}

#[tokio::test]
async fn recalling_a_delegate_reads_the_stores_finished_result_without_delegating_again() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(
        7,
        0,
        "mcp__relay__delegate",
        json!({"agent": "main/research", "instruction": "조사해"}),
    );
    // 끝난 결과가 없으면 읽을 것이 없고 새 위임도 만들지 않는다
    assert_eq!(port.recall(&slot).await, Ok(None));
    assert!(lock(&rig.delegation.requests).is_empty());
    // 저장소가 대조해 준 완료 결과: 성공한 문자열은 그대로, 취소·실패는 고정 단어
    for (body, text, is_error) in [
        (json!({"state": "succeeded", "result": "답"}), "답", false),
        (
            json!({"state": "succeeded", "result": {"a": 1}}),
            r#"{"a":1}"#,
            false,
        ),
        (
            json!({"state": "cancelled", "error": "cancelled"}),
            "child_cancelled",
            true,
        ),
        (json!({"state": "dead", "error": "x"}), "child_failed", true),
    ] {
        lock(&rig.delegation.results).insert("cli-7-0".into(), JsonPayload(body.to_string()));
        let out = port.recall(&slot).await.unwrap().unwrap();
        assert_eq!(
            (out.content[0]["text"].clone(), out.is_error),
            (json!(text), is_error)
        );
    }
    // 읽을 때 넘기는 명령은 현재 판본·직계 인가를 거친 같은 단계다
    let read = lock(&rig.delegation.reads).last().cloned().unwrap();
    assert_eq!(
        (
            read.step.as_str(),
            read.child.0.as_str(),
            read.checked_revision.0.as_str()
        ),
        ("cli-7-0", "main/research", "r1")
    );
    assert!(read.scopes.contains("run"));
    // 저장소가 거절하면 고정 단어, 소유권을 잃었으면 물러난다
    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(denied.content[0]["text"], "denied");
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.recall(&slot).await, Err(SessionError::Lost));
    // 지금 활성인 판본에서 직계 자식이 아니면 저장소를 읽기 전에 거절한다
    let reads = lock(&rig.delegation.reads).len();
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    let denied = port.recall(&slot).await.unwrap().unwrap();
    assert_eq!(denied.content[0]["text"], "denied");
    assert_eq!(lock(&rig.delegation.reads).len(), reads);
}

#[tokio::test]
async fn settling_a_slot_calls_the_same_call_id_with_canonical_args_and_maps_the_outcome() {
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let list = pending(7, 1, "mcp__relay__agenda_list", json!({"b": 1, "a": 2}));
    rig.broker.script(done(
        r#"{"content":[{"type":"text","text":"일정 없음"}],"isError":false}"#,
    ));
    let out = output(port.settle(&list).await.unwrap());
    assert_eq!(out.content, vec![json!({"type":"text","text":"일정 없음"})]);
    assert!(!out.is_error);
    let sent = rig.broker.calls().pop().unwrap();
    assert_eq!(sent.call.0, "cli-7-1", "라이브 경로와 같은 호출 ID");
    assert_eq!(sent.tool.0, "agenda.list");
    assert_eq!(sent.args.0, r#"{"a":2,"b":1}"#, "정규 JSON(키 정렬)");

    // 일반 JSON은 글 하나로, 거절은 고정 단어와 오류 표시로
    rig.broker.script(done(r#"{"events":[]}"#));
    let plain = output(port.settle(&list).await.unwrap());
    assert_eq!(plain.content[0]["text"], r#"{"events":[]}"#);
    rig.broker.script(Err(Error::Denied));
    let denied = output(port.settle(&list).await.unwrap());
    assert_eq!(
        (denied.content[0]["text"].clone(), denied.is_error),
        (json!("denied"), true)
    );
    // 승인 대기·보류: 저장소가 run을 멈췄다
    rig.broker
        .script(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(port.settle(&list).await, Ok(Slot::Parked));
    // 일시 오류는 다시 하고, 이름을 풀 수 없는 도구(프런트가 `invalid`로 거절하는 호출)·VM 내장 도구는 호스트가 실행하지 않는다
    rig.broker.script(Err(Error::Unavailable));
    assert_eq!(port.settle(&list).await, Err(SessionError::Unavailable));
    let calls_before = rig.broker.calls().len();
    let unknown = pending(7, 2, "mcp__relay__no_such_tool_x", json!({}));
    assert_eq!(
        output(port.settle(&unknown).await.unwrap()).content[0]["text"],
        "invalid"
    );
    let builtin = pending(7, 3, "Read", json!({"file_path": "/bot/AGENTS.md"}));
    let aborted = output(port.settle(&builtin).await.unwrap());
    assert_eq!(
        (aborted.content[0]["text"].clone(), aborted.is_error),
        (json!("aborted"), true)
    );
    assert_eq!(
        rig.broker.calls().len(),
        calls_before,
        "내장 도구는 브로커로 가지 않는다"
    );
}

#[tokio::test]
async fn a_delegate_slot_reuses_the_finished_child_or_delegates_again_under_the_call_id() {
    let input = json!({"agent": "main/research", "instruction": "조사해"});
    // 자식이 아직이면 같은 단계 키(호출 ID)로 위임하고 부모는 다시 기다린다
    let (rig, session) = session_of(agent_run("r1"), true).await;
    let port = session.unwrap().0;
    let slot = pending(7, 0, "mcp__relay__delegate", input.clone());
    assert_eq!(port.settle(&slot).await, Ok(Slot::Parked));
    let delegated = lock(&rig.delegation.requests).clone();
    assert_eq!(delegated.len(), 1);
    assert_eq!(delegated[0].step, "cli-7-0");
    assert_eq!(delegated[0].child.0, "main/research");
    assert_eq!(
        delegated[0].checked_revision.0, "r1",
        "권한을 판정한 지금 판본"
    );

    // 자식이 끝나 부모가 깨어났다: 저장소가 대조한 결과를 돌려주고 다시 위임하지 않는다
    let mut ctx = agent_run("r2");
    ctx.work.steps.push(Step {
        key: "cli-7-0".into(),
        body: JsonPayload(json!({"state": "succeeded", "result": {"summary": "끝"}}).to_string()),
    });
    let (rig, session) = session_of(ctx, true).await;
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": {"summary": "끝"}}).to_string()),
    );
    let port = session.unwrap().0;
    let out = output(port.settle(&slot).await.unwrap());
    assert_eq!(out.content[0]["text"], r#"{"summary":"끝"}"#);
    assert!(lock(&rig.delegation.requests).is_empty());
}

#[tokio::test]
async fn a_finished_child_is_returned_only_while_the_store_and_both_revisions_allow_it() {
    let mut ctx = agent_run("r1");
    ctx.work.steps.push(Step {
        key: "cli-7-0".into(),
        body: JsonPayload(json!({"state": "succeeded", "result": "답"}).to_string()),
    });
    let (rig, session) = session_of(ctx, true).await;
    lock(&rig.delegation.results).insert(
        "cli-7-0".into(),
        JsonPayload(json!({"state": "succeeded", "result": "답"}).to_string()),
    );
    let port = session.unwrap().0;
    let slot = pending(
        7,
        0,
        "mcp__relay__delegate",
        json!({"agent": "main/research", "instruction": "조사해"}),
    );
    let text = |slot: Slot| output(slot).content[0]["text"].clone();
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("답"));

    // 저장소가 자식의 대상·범위를 대조해 거절하면 결과를 주지 않는다. 소유권을 잃었으면 물러난다
    *lock(&rig.delegation.read_error) = Some(Error::Denied);
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("denied"));
    *lock(&rig.delegation.read_error) = Some(Error::Stale);
    assert_eq!(port.settle(&slot).await, Err(SessionError::Lost));
    // 지금 활성인 판본에서 직계 자식이 아니게 됐으면 이미 끝난 결과도 주지 않는다(저장소를 읽기 전에)
    let reads = lock(&rig.delegation.reads).len();
    let mut changed = snapshot();
    changed.agents.remove(&AgentId("main/research".into()));
    *lock(&rig.catalog.current) = Arc::new(changed);
    assert_eq!(text(port.settle(&slot).await.unwrap()), json!("denied"));
    assert_eq!(lock(&rig.delegation.reads).len(), reads);
    assert!(lock(&rig.delegation.requests).is_empty());
}
