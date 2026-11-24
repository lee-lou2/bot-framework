//! 데모 모델 시험: README 흐름(일정 조회 · 삭제)을 차례마다 확인한다. 같은 입력이면 같은 출력이다.

use super::*;
use crate::model::{Exchange, Limits, Message, ToolKind, ToolSpec};
use bot_core::{id::*, state::Risk};
use tokio::sync::watch;

fn tool(id: &str) -> ToolSpec {
    ToolSpec {
        id: ToolId(id.into()),
        kind: ToolKind::Skill,
        class: Risk::Read,
        description: String::new(),
        input: JsonPayload("{}".into()),
    }
}

fn request(text: &str, tools: &[&str], exchanges: Vec<Exchange>) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        model: "assistant".into(),
        system: String::new(),
        conversation: vec![
            Message {
                role: Role::User,
                speaker: None,
                text: "이전 말".into(),
            },
            Message {
                role: Role::Assistant,
                speaker: None,
                text: "이전 답".into(),
            },
            Message {
                role: Role::User,
                speaker: Some(PrincipalId("local:owner".into())),
                text: text.into(),
            },
        ],
        tools: tools.iter().map(|t| tool(t)).collect(),
        exchanges,
        limits: Limits::default(),
        cancel: watch::channel(false).1,
        builtin: Default::default(),
        relay: None,
        session: None,
    }
}

const BOTH: [&str; 2] = [LIST, DELETE];

async fn step(request: ModelRequest) -> ModelStep {
    DemoModel.step(request).await.unwrap()
}

fn done(tool: &str, args: &str, end: CallEnd) -> Exchange {
    Exchange {
        text: String::new(),
        calls: vec![DoneCall {
            call: call_id(&RunId("r1".into()), 0, 0),
            tool: ToolId(tool.into()),
            args: JsonPayload(args.into()),
            end,
        }],
    }
}

fn listed(result: &str) -> Exchange {
    done(LIST, "{}", CallEnd::Done(JsonPayload(result.into())))
}

const EVENTS: &str = r#"{"events":[{"id":"e-walk","title":"산책","start":"07:30"},{"id":"e-meet","title":"팀 회의","start":"14:00"}]}"#;

fn calls_of(step: &ModelStep) -> Vec<(String, String, String)> {
    match step {
        ModelStep::Calls { calls, .. } => calls
            .iter()
            .map(|c| (c.call.0.clone(), c.tool.0.clone(), c.args.0.clone()))
            .collect(),
        other => panic!("호출이어야 해요: {other:?}"),
    }
}

fn text_of(step: &ModelStep) -> &str {
    match step {
        ModelStep::Final { text, .. } => text,
        other => panic!("최종 답이어야 해요: {other:?}"),
    }
}

#[tokio::test]
async fn listing_asks_for_the_agenda_then_summarizes_it_in_korean() {
    let first = step(request("오늘 일정 알려줘", &BOTH, vec![])).await;
    assert_eq!(
        calls_of(&first),
        [("r1:0:0".into(), "agenda.list".into(), "{}".into())]
    );
    let second = step(request("오늘 일정 알려줘", &BOTH, vec![listed(EVENTS)])).await;
    assert_eq!(
        text_of(&second),
        "일정이 2건 있어요.\n- 07:30 산책\n- 14:00 팀 회의"
    );
}

#[tokio::test]
async fn understands_common_tool_result_shapes() {
    for (shape, want) in [
        (r#"[{"id":1,"name":"운동"}]"#, "일정이 1건 있어요.\n- 운동"),
        (
            r#"{"structuredContent":{"events":[{"id":"a","title":"점심","time":"12:00"}]}}"#,
            "일정이 1건 있어요.\n- 12:00 점심",
        ),
        (
            r#"{"content":[{"type":"text","text":"{\"events\":[{\"id\":\"a\",\"summary\":\"독서\"}]}"}]}"#,
            "일정이 1건 있어요.\n- 독서",
        ),
        (r#"{"events":[]}"#, "등록된 일정이 없어요."),
        (r#"[]"#, "등록된 일정이 없어요."),
        (
            r#"{"content":[{"type":"text","text":"오늘은 일정이 없습니다"}]}"#,
            "오늘은 일정이 없습니다",
        ),
        ("not json at all", "not json at all"),
    ] {
        let reply = step(request("일정 보여줘", &BOTH, vec![listed(shape)])).await;
        assert_eq!(text_of(&reply), want, "{shape}");
    }
}

#[tokio::test]
async fn listing_failures_are_reported_plainly() {
    let denied = done(LIST, "{}", CallEnd::Denied);
    assert_eq!(
        text_of(&step(request("일정 알려줘", &BOTH, vec![denied])).await),
        "일정을 조회할 권한이 없어요."
    );
    let failed = done(LIST, "{}", CallEnd::Failed("unavailable".into()));
    assert!(
        text_of(&step(request("일정 알려줘", &BOTH, vec![failed])).await)
            .starts_with("일정을 가져오지 못했어요")
    );
}

#[tokio::test]
async fn deleting_finds_the_event_by_title_then_deletes_it_by_id() {
    let text = "산책 일정을 삭제해줘";
    let first = step(request(text, &BOTH, vec![])).await;
    assert_eq!(
        calls_of(&first),
        [("r1:0:0".into(), "agenda.list".into(), "{}".into())]
    );

    let second = step(request(text, &BOTH, vec![listed(EVENTS)])).await;
    assert_eq!(
        calls_of(&second),
        [(
            "r1:1:0".into(),
            "agenda.delete".into(),
            r#"{"event":"e-walk"}"#.into()
        )]
    );

    let deleted = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload("{}".into())),
    );
    let third = step(request(text, &BOTH, vec![listed(EVENTS), deleted])).await;
    assert_eq!(text_of(&third), "'산책' 일정을 삭제했어요.");

    // 시뮬레이션 결과(가상 영수증)는 영구 삭제라고 말하지 않는다: 그대로·MCP 감싼 모양 모두
    let sim = r#"{"simulated":true,"persisted":false,"receipt":"sim-7","event":"e-walk"}"#;
    let wrapped = serde_json::json!({"content": [{"type": "text", "text": sim}], "isError": false});
    for payload in [sim.to_owned(), wrapped.to_string()] {
        let simulated = done(
            DELETE,
            r#"{"event":"e-walk"}"#,
            CallEnd::Done(JsonPayload(payload)),
        );
        let said = step(request(text, &BOTH, vec![listed(EVENTS), simulated])).await;
        assert_eq!(
            text_of(&said),
            "'산책' 일정 삭제를 시뮬레이션했어요(가상 영수증 sim-7)."
        );
    }
    let no_receipt = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload(r#"{"simulated":true}"#.into())),
    );
    let said = step(request(text, &BOTH, vec![listed(EVENTS), no_receipt])).await;
    assert_eq!(
        text_of(&said),
        "'산책' 일정 삭제를 시뮬레이션했어요(영구 삭제는 아니에요)."
    );
    let not_simulated = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Done(JsonPayload(r#"{"simulated":false}"#.into())),
    );
    let said = step(request(text, &BOTH, vec![listed(EVENTS), not_simulated])).await;
    assert_eq!(text_of(&said), "'산책' 일정을 삭제했어요.");

    let denied = done(DELETE, r#"{"event":"e-walk"}"#, CallEnd::Denied);
    let refused = step(request(text, &BOTH, vec![listed(EVENTS), denied])).await;
    assert_eq!(
        text_of(&refused),
        "삭제가 승인되지 않아 '산책' 일정은 그대로 두었어요."
    );

    let failed = done(
        DELETE,
        r#"{"event":"e-walk"}"#,
        CallEnd::Failed("unavailable".into()),
    );
    let broken = step(request(text, &BOTH, vec![listed(EVENTS), failed])).await;
    assert!(text_of(&broken).starts_with("삭제하지 못했어요"));
}

#[tokio::test]
async fn deleting_never_guesses_when_the_event_is_missing_or_ambiguous() {
    let none = step(request("회식 일정 삭제해줘", &BOTH, vec![listed(EVENTS)])).await;
    assert!(
        text_of(&none).starts_with("말씀하신 일정을 찾지 못했어요.\n일정이 2건 있어요."),
        "{}",
        text_of(&none)
    );
    let many = r#"[{"id":"1","title":"운동"},{"id":"2","title":"점심"}]"#;
    let ambiguous = step(request("운동이랑 점심 지워줘", &BOTH, vec![listed(many)])).await;
    assert_eq!(
        text_of(&ambiguous),
        "맞는 일정이 여러 개예요: 운동, 점심. 어떤 일정을 삭제할까요?"
    );
    // 더 구체적인 제목이 있으면 그쪽을 고른다
    let nested = r#"[{"id":"1","title":"산책"},{"id":"2","title":"저녁 산책"}]"#;
    let pick = step(request("저녁 산책 삭제해줘", &BOTH, vec![listed(nested)])).await;
    assert_eq!(calls_of(&pick)[0].2, r#"{"event":"2"}"#);
}

#[tokio::test]
async fn missing_tools_and_unknown_requests_get_a_plain_answer() {
    assert!(
        text_of(&step(request("일정 알려줘", &[], vec![])).await)
            .starts_with("일정 도구를 쓸 수 없어요")
    );
    let no_delete = step(request("산책 일정 삭제해줘", &[LIST], vec![listed(EVENTS)])).await;
    assert!(text_of(&no_delete).starts_with("삭제 도구를 쓸 수 없어요"));
    assert_eq!(text_of(&step(request("안녕", &BOTH, vec![])).await), HELP);
}

#[tokio::test]
async fn the_demo_model_is_a_step_runner_that_never_needs_a_relay() {
    assert_eq!(DemoModel.kind(), RunnerKind::Step);
    let with_relay = request("일정 알려줘", &BOTH, vec![]);
    assert!(with_relay.relay.is_none());
}

#[tokio::test]
async fn the_same_request_always_gives_the_same_step_and_cancel_is_honored() {
    let a = step(request("산책 일정을 삭제해줘", &BOTH, vec![listed(EVENTS)])).await;
    let b = step(request("산책 일정을 삭제해줘", &BOTH, vec![listed(EVENTS)])).await;
    assert_eq!(a, b);
    let (tx, rx) = watch::channel(false);
    tx.send(true).unwrap();
    let mut cancelled = request("일정 알려줘", &BOTH, vec![]);
    cancelled.cancel = rx;
    assert_eq!(DemoModel.step(cancelled).await, Err(ModelError::Cancelled));
}
