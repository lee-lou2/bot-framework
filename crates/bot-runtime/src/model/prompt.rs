//! CLI 실행기가 표준입력으로 주는 대화 문서. 지침(system)은 입력 폴더의 `AGENTS.md`로 따로 간다.

use super::{CallEnd, ModelRequest, Role};

/// `-p`/마지막 인자로 주는 고정 지시. 내용은 표준입력의 대화에 있다.
pub(crate) const INSTRUCTION: &str = "표준입력의 대화를 읽고 마지막 사용자 메시지에 답하세요.";
/// 세션을 이어 받을 때의 고정 지시. 원래 요청 본문을 다시 보내지 않는다(이미 세션 기록에 있고, 다시 보내면 같은 일을 또 할 수 있다).
pub(crate) const RESUME_INSTRUCTION: &str =
    "중단된 작업을 이어서 마무리하고 사용자에게 최종 답을 하세요.";
/// 도구 결과 하나를 문서에 싣는 글자 수 상한(넘으면 뒤를 자르고 표시한다)
const MAX_RESULT_CHARS: usize = 8_000;

/// 도구 결과를 모델에 보이는 글로 줄인다(CLI 실행기와 API 실행기가 같은 한도를 쓴다).
pub(crate) fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_RESULT_CHARS) {
        Some((end, _)) => format!("{}… (잘림)", &text[..end]),
        None => text.to_owned(),
    }
}

/// 대화와(있다면) 이번 run에서 이미 한 도구 호출을 Markdown 문서로 쓴다.
pub(crate) fn render_stdin(request: &ModelRequest) -> String {
    let mut out = String::from("# 대화\n");
    for message in &request.conversation {
        match (message.role, &message.speaker) {
            (Role::User, Some(who)) => out.push_str(&format!("\n### 사용자 ({})\n", who.0)),
            (Role::User, None) => out.push_str("\n### 사용자\n"),
            (Role::Assistant, _) => out.push_str("\n### 비서\n"),
        }
        out.push_str(message.text.trim());
        out.push('\n');
    }
    if !request.exchanges.is_empty() {
        out.push_str("\n# 이번 요청에서 이미 한 일\n");
        for exchange in &request.exchanges {
            if !exchange.text.trim().is_empty() {
                out.push_str(&format!("\n{}\n", exchange.text.trim()));
            }
            for call in &exchange.calls {
                let result = match &call.end {
                    CallEnd::Done(result) => format!("결과: {}", clip(&result.0)),
                    CallEnd::Failed(code) => format!("실패: {code}"),
                    CallEnd::Denied => "승인되지 않았거나 막혀서 실행하지 않았어요".to_owned(),
                };
                out.push_str(&format!(
                    "- `{}` 인자 {} → {result}\n",
                    call.tool.0,
                    clip(&call.args.0)
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DoneCall, Exchange, Limits, Message};
    use bot_core::{command::JsonPayload, id::*};
    use tokio::sync::watch;

    fn request(conversation: Vec<Message>, exchanges: Vec<Exchange>) -> ModelRequest {
        ModelRequest {
            attempt: Attempt {
                profile: ProfileId("p".into()),
                run: RunId("r".into()),
                claim: Some(bot_core::id::ClaimEpoch(1)),
                generation: Generation(1),
            },
            model: "assistant".into(),
            system: String::new(),
            conversation,
            tools: vec![],
            exchanges,
            limits: Limits::default(),
            cancel: watch::channel(false).1,
            builtin: Default::default(),
            relay: None,
            session: None,
        }
    }

    #[test]
    fn renders_speakers_and_the_last_message_last() {
        let user = |who: Option<&str>, text: &str| Message {
            role: Role::User,
            speaker: who.map(|w| PrincipalId(w.into())),
            text: text.into(),
        };
        let reply = Message {
            role: Role::Assistant,
            speaker: None,
            text: "네".into(),
        };
        let doc = render_stdin(&request(
            vec![
                user(Some("local:owner"), "안녕"),
                reply,
                user(None, "일정 알려줘\n"),
            ],
            vec![],
        ));
        assert_eq!(
            doc,
            "# 대화\n\n### 사용자 (local:owner)\n안녕\n\n### 비서\n네\n\n### 사용자\n일정 알려줘\n"
        );
    }

    #[test]
    fn renders_calls_and_clips_long_results() {
        let long = format!("\"{}\"", "가".repeat(MAX_RESULT_CHARS + 10));
        let exchange = Exchange {
            text: "확인할게요".into(),
            calls: vec![
                DoneCall {
                    call: CallId("c".into()),
                    tool: ToolId("agenda.list".into()),
                    args: JsonPayload("{}".into()),
                    end: CallEnd::Done(JsonPayload(long)),
                },
                DoneCall {
                    call: CallId("d".into()),
                    tool: ToolId("agenda.delete".into()),
                    args: JsonPayload(r#"{"event":"e1"}"#.into()),
                    end: CallEnd::Denied,
                },
            ],
            state: None,
        };
        let doc = render_stdin(&request(vec![], vec![exchange]));
        assert!(doc.contains("# 이번 요청에서 이미 한 일\n\n확인할게요\n"));
        assert!(doc.contains("- `agenda.list` 인자 {} → 결과: \"가"));
        assert!(doc.contains("… (잘림)\n"));
        assert!(doc.contains("- `agenda.delete` 인자 {\"event\":\"e1\"} → 승인되지 않았거나 막혀서 실행하지 않았어요\n"));
        assert!(!doc.contains(&"가".repeat(MAX_RESULT_CHARS + 1)));
    }
}
