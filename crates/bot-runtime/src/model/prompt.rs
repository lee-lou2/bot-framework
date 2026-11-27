//! Markdown stdin conversation rendering and tool-output clipping for CLI model runners.

use super::{CallEnd, ModelRequest, Role};

pub(crate) const INSTRUCTION: &str =
    "Read the conversation from stdin and reply to the last user message.";
/// Fixed instruction for resumed sessions (original prompt is already in session history).
pub(crate) const RESUME_INSTRUCTION: &str =
    "Continue the interrupted task and provide the final answer to the user.";
const MAX_RESULT_CHARS: usize = 8_000;

/// Truncates tool results to `MAX_RESULT_CHARS` (shared by CLI and API runners).
pub(crate) fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_RESULT_CHARS) {
        Some((end, _)) => format!("{}… (truncated)", &text[..end]),
        None => text.to_owned(),
    }
}

pub(crate) fn render_stdin(request: &ModelRequest) -> String {
    let mut out = String::from("# Conversation\n");
    for message in &request.conversation {
        match (message.role, &message.speaker) {
            (Role::User, Some(who)) => out.push_str(&format!("\n### User ({})\n", who.0)),
            (Role::User, None) => out.push_str("\n### User\n"),
            (Role::Assistant, _) => out.push_str("\n### Assistant\n"),
        }
        out.push_str(message.text.trim());
        out.push('\n');
    }
    if !request.exchanges.is_empty() {
        out.push_str("\n# Completed steps in this request\n");
        for exchange in &request.exchanges {
            if !exchange.text.trim().is_empty() {
                out.push_str(&format!("\n{}\n", exchange.text.trim()));
            }
            for call in &exchange.calls {
                let result = match &call.end {
                    CallEnd::Done(result) => format!("result: {}", clip(&result.0)),
                    CallEnd::Failed(code) => format!("failed: {code}"),
                    CallEnd::Denied => "Not executed: approval denied or blocked.".to_owned(),
                };
                out.push_str(&format!(
                    "- `{}` args {} → {result}\n",
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
            text: "Sure".into(),
        };
        let doc = render_stdin(&request(
            vec![
                user(Some("local:owner"), "Hello"),
                reply,
                user(None, "Show today's agenda\n"),
            ],
            vec![],
        ));
        assert_eq!(
            doc,
            "# Conversation\n\n### User (local:owner)\nHello\n\n### Assistant\nSure\n\n### User\nShow today's agenda\n"
        );
    }

    #[test]
    fn renders_calls_and_clips_long_results() {
        let long = format!("\"{}\"", "€".repeat(MAX_RESULT_CHARS + 10));
        let exchange = Exchange {
            text: "Checking now".into(),
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
        assert!(doc.contains("# Completed steps in this request\n\nChecking now\n"));
        assert!(doc.contains("- `agenda.list` args {} → result: \"€"));
        assert!(doc.contains("… (truncated)\n"));
        assert!(doc.contains("- `agenda.delete` args {\"event\":\"e1\"} → Not executed: approval denied or blocked.\n"));
        assert!(!doc.contains(&"€".repeat(MAX_RESULT_CHARS + 1)));
    }
}
