//! Secret handling, activation-gate, leak detection, and endpoint validation tests for the direct API runner (no database required).

use crate::support::{
    api::*,
    model_request::*,
    upstream::{FakeApi, Reply, Seen},
};
use bot_runtime::model::{ApiEndpoint, ApiKind, ApiRunner, ApiSettings, ModelError, ModelStep};
use std::sync::Arc;

#[tokio::test]
async fn logs_carry_only_a_fixed_failure_kind_never_the_key_the_conversation_or_the_answer() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "Secret upstream answer", Some((9, 4))));
        assert!(matches!(
            step(&api_runner(kind, &api), request()).await,
            Ok(ModelStep::Final { .. })
        ));
        let api = FakeApi::start().await;
        api.script(Reply::Close);
        api.script(Reply::Json(401, format!(r#"{{"error":"{KEY}"}}"#)));
        api.script(text(kind, &format!("Key is {KEY}"), None));
        for _ in 0..3 {
            let _ = step(&api_runner(kind, &api), request()).await;
        }
    }
    let seen = logs.text();
    for secret in [
        KEY,
        "Show today's agenda",
        "Secret upstream answer",
        "profile instructions",
        "reason",
    ] {
        assert!(
            !seen.contains(secret),
            "{secret:?} leaked into logs: {seen}"
        );
    }
}

#[tokio::test]
async fn an_agent_that_needs_a_builtin_file_tool_is_refused_before_anything_is_sent() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "answer", None));
        let mut req = request();
        req.builtin = [bot_core::profile::Builtin::Read].into();
        let result = step(&api_runner(kind, &api), req).await;
        assert_eq!(
            result,
            Err(failed("unsupported_builtin", false)),
            "{kind:?}"
        );
        assert!(api.seen().is_empty());
    }
}

#[tokio::test]
async fn an_old_process_sends_nothing_and_does_not_even_read_the_key() {
    for kind in KINDS {
        for (gate, code) in [
            (Gate::Stale, "stale_worker"),
            (Gate::Unreadable, "activation_unverified"),
        ] {
            let api = FakeApi::start().await;
            api.script(text(kind, "Must not be sent", None));
            let keys = Keys::with("model-key", KEY.as_bytes());
            let runner = ApiRunner::new(
                settings(kind, &api, Some("model-key")),
                keys.clone(),
                Arc::new(gate),
            )
            .unwrap();
            let result = step(&runner, request()).await;
            assert_eq!(result, Err(failed(code, true)), "{kind:?}");
            assert!(api.seen().is_empty(), "{kind:?}: stale worker sent request");
            assert_eq!(keys.reads(), 0, "{kind:?}: stale worker read secret");
        }
    }
}

#[tokio::test]
async fn a_key_that_cannot_be_used_fails_with_a_fixed_code_before_anything_is_sent() {
    for kind in KINDS {
        let cases: [(Arc<Keys>, ModelError); 7] = [
            (Keys::empty(), failed("model_key_missing", false)),
            (Keys::unavailable(), failed("secret_unavailable", true)),
            (
                Keys::with("model-key", b"short"),
                failed("model_key_invalid", false),
            ),
            (
                Keys::with("model-key", b"has a space inside"),
                failed("model_key_invalid", false),
            ),
            (
                Keys::with("model-key", "key-non-ascii-€€€-val".as_bytes()),
                failed("model_key_invalid", false),
            ),
            (
                Keys::with("model-key", b"line\nbreak-key-123"),
                failed("model_key_invalid", false),
            ),
            (
                Keys::with(
                    "model-key",
                    &[0xff, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa, 0xf9, 0xf8],
                ),
                failed("model_key_invalid", false),
            ),
        ];
        for (keys, want) in cases {
            let api = FakeApi::start().await;
            api.script(text(kind, "answer", None));
            let runner = ApiRunner::new(
                settings(kind, &api, Some("model-key")),
                keys,
                Arc::new(Gate::Open),
            )
            .unwrap();
            assert_eq!(step(&runner, request()).await, Err(want), "{kind:?}");
            assert!(api.seen().is_empty());
        }
        // Leading/trailing whitespace and newlines (e.g. from secret files) are trimmed.
        let api = FakeApi::start().await;
        api.script(text(kind, "answer", None));
        let keys = Keys::with("model-key", format!("  {KEY}\n").as_bytes());
        let runner = ApiRunner::new(
            settings(kind, &api, Some("model-key")),
            keys,
            Arc::new(Gate::Open),
        )
        .unwrap();
        assert!(matches!(
            step(&runner, request()).await,
            Ok(ModelStep::Final { .. })
        ));
        let header = match kind {
            ApiKind::Anthropic => api.seen()[0].header("x-api-key").map(str::to_owned),
            ApiKind::OpenAi => api.seen()[0].header("authorization").map(str::to_owned),
        };
        assert!(header.unwrap().ends_with(KEY));
    }
}

#[tokio::test]
async fn a_response_that_contains_the_request_key_is_closed_with_a_fixed_code_in_every_place() {
    for kind in KINDS {
        let leaky: Vec<(&str, Reply)> = vec![
            (
                "final text",
                text(kind, &format!("My key is {KEY}"), Some((9, 4))),
            ),
            (
                "call text",
                calls(
                    kind,
                    &format!("key {KEY}"),
                    vec![call("agenda.list", serde_json::json!({}))],
                    Some((9, 4)),
                ),
            ),
            (
                "call args",
                calls(
                    kind,
                    "",
                    vec![call(
                        "agenda.list",
                        serde_json::json!({"memo": format!("[{KEY}]")}),
                    )],
                    Some((9, 4)),
                ),
            ),
            (
                "ignored field",
                Reply::With(Arc::new(move |_seen: &Seen| {
                    let body = match kind {
                        ApiKind::Anthropic => {
                            serde_json::json!({"id": KEY, "content": [{"type": "text", "text": "answer"}], "stop_reason": "end_turn"})
                        }
                        ApiKind::OpenAi => {
                            serde_json::json!({"id": KEY, "choices": [{"message": {"content": "answer"}, "finish_reason": "stop"}]})
                        }
                    };
                    Reply::Json(200, body.to_string())
                })),
            ),
            (
                "escaped",
                Reply::With(Arc::new(move |_seen: &Seen| {
                    let escaped: String = KEY
                        .chars()
                        .map(|c| format!("\\u{:04x}", c as u32))
                        .collect();
                    let raw = match kind {
                        ApiKind::Anthropic => format!(
                            r#"{{"content":[{{"type":"text","text":"{escaped}"}}],"stop_reason":"end_turn"}}"#
                        ),
                        ApiKind::OpenAi => format!(
                            r#"{{"choices":[{{"message":{{"content":"{escaped}"}},"finish_reason":"stop"}}]}}"#
                        ),
                    };
                    Reply::Json(200, raw)
                })),
            ),
        ];
        for (place, reply) in leaky {
            let api = FakeApi::start().await;
            api.script(reply);
            let result = step(&api_runner(kind, &api), request()).await;
            assert_eq!(
                result,
                Err(failed("secret_in_response", false)),
                "{kind:?} {place}"
            );
        }
        let api = FakeApi::start().await;
        api.script(text(kind, "Answer without key", None));
        assert!(matches!(
            step(&api_runner(kind, &api), request()).await,
            Ok(ModelStep::Final { .. })
        ));
    }
}

#[tokio::test]
async fn only_a_loopback_server_may_be_called_without_a_key_and_the_address_rules_are_enforced_up_front()
 {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "Local answer", None));
        let keys = Keys::empty();
        let runner = ApiRunner::new(
            settings(kind, &api, None),
            keys.clone(),
            Arc::new(Gate::Open),
        )
        .unwrap();
        assert!(matches!(
            step(&runner, request()).await,
            Ok(ModelStep::Final { .. })
        ));
        let seen = api.seen();
        assert!(seen[0].header("authorization").is_none() && seen[0].header("x-api-key").is_none());
        assert_eq!(keys.reads(), 0);

        let remote = ApiSettings {
            endpoint: ApiEndpoint::parse("https://api.example.com/v1").unwrap(),
            secret: None,
            ..settings(kind, &api, None)
        };
        assert!(ApiRunner::new(remote, Keys::empty(), Arc::new(Gate::Open)).is_err());

        let nameless = ApiSettings {
            model: " ".into(),
            ..settings(kind, &api, Some("model-key"))
        };
        assert!(ApiRunner::new(nameless, Keys::empty(), Arc::new(Gate::Open)).is_err());
    }
    for bad in [
        "http://example.com/v1",
        "http://localhost:11434/v1",
        "http://10.0.0.5/v1",
        "https://user:pw@example.com",
    ] {
        assert!(ApiEndpoint::parse(bad).is_err(), "{bad}");
    }
}

#[tokio::test]
async fn a_destination_revoked_while_the_key_is_being_read_receives_nothing() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "Must not be sent", Some((9, 4))));
        let keys = Keys::with("model-key", KEY.as_bytes());
        let runner = ApiRunner::new(
            settings(kind, &api, Some("model-key")),
            keys.clone(),
            Arc::new(RevokedByKeyRead(keys.clone())),
        )
        .unwrap();
        let result = step(&runner, request()).await;
        assert_eq!(result, Err(failed("stale_worker", true)), "{kind:?}");
        assert!(
            api.seen().is_empty(),
            "{kind:?}: revoked endpoint received request"
        );
        assert_eq!(keys.reads(), 1);
    }
}

/// Key containing JSON-escapable characters (`"` and `\`).
const ESCAPED_KEY: &str = "sk-te\"st\\0123456789";

#[tokio::test]
async fn a_key_that_needs_json_escaping_is_still_caught_in_every_decoded_string() {
    for kind in KINDS {
        let mut replies = vec![
            calls(
                kind,
                "",
                vec![call("agenda.list", serde_json::json!({"q": ESCAPED_KEY}))],
                Some((5, 2)),
            ),
            text(kind, &format!("My key is {ESCAPED_KEY}"), Some((5, 2))),
        ];
        if kind == ApiKind::OpenAi {
            let signed = Call {
                signature: Some(ESCAPED_KEY),
                ..call("agenda.list", serde_json::json!({}))
            };
            replies.push(calls(kind, "", vec![signed], Some((5, 2))));
        }
        for (n, reply) in replies.into_iter().enumerate() {
            let api = FakeApi::start().await;
            api.script(reply);
            let runner = ApiRunner::new(
                settings(kind, &api, Some("model-key")),
                Keys::with("model-key", ESCAPED_KEY.as_bytes()),
                Arc::new(Gate::Open),
            )
            .unwrap();
            let result = step(&runner, request()).await;
            assert_eq!(
                result,
                Err(failed("secret_in_response", false)),
                "{kind:?} #{n}"
            );
        }
    }
}
