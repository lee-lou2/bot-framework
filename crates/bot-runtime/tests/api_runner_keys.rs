//! API 직접 호출 실행기의 키 취급과 주소 규칙을 실제 HTTP(loopback 가짜 상류)로 확인한다: 읽기 전 판본 확인·쓸 수 없는 키·응답에 키가 있으면 닫기·로그에 키 없음·키 없이는 loopback만.
//! 두 형식(Anthropic·OpenAI 호환)을 같은 시험으로 돌린다. 데이터베이스가 필요 없다.

mod support;

use bot_runtime::model::{ApiEndpoint, ApiKind, ApiRunner, ApiSettings, ModelError, ModelStep};
use std::sync::Arc;
use support::{
    api::*,
    model_request::*,
    upstream::{FakeApi, Reply, Seen},
};

#[tokio::test]
async fn logs_carry_only_a_fixed_failure_kind_never_the_key_the_conversation_or_the_answer() {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    for kind in KINDS {
        // 성공: 아무것도 남기지 않는다
        let api = FakeApi::start().await;
        api.script(text(kind, "상류의 비밀스러운 답", Some((9, 4))));
        assert!(matches!(
            step(&api_runner(kind, &api), request()).await,
            Ok(ModelStep::Final { .. })
        ));
        // 전송 실패(응답 없이 연결이 닫힘)·상태 오류·키가 응답에 든 경우
        let api = FakeApi::start().await;
        api.script(Reply::Close);
        api.script(Reply::Json(401, format!(r#"{{"error":"{KEY}"}}"#)));
        api.script(text(kind, &format!("키는 {KEY}"), None));
        for _ in 0..3 {
            let _ = step(&api_runner(kind, &api), request()).await;
        }
    }
    let seen = logs.text();
    for secret in [
        KEY,
        "오늘 일정 알려줘",
        "상류의 비밀스러운 답",
        "프로필 지침",
        "사유",
    ] {
        assert!(!seen.contains(secret), "로그에 {secret:?}이 있다: {seen}");
    }
}

#[tokio::test]
async fn an_agent_that_needs_a_builtin_file_tool_is_refused_before_anything_is_sent() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "답", None));
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
            api.script(text(kind, "보내면 안 되는 답", None));
            let keys = Keys::with("model-key", KEY.as_bytes());
            let runner = ApiRunner::new(
                settings(kind, &api, Some("model-key")),
                keys.clone(),
                Arc::new(gate),
            )
            .unwrap();
            let result = step(&runner, request()).await;
            // 일시 오류다: 구판이면 저장소가 시도를 쓰지 않고 대기열로 돌려준다
            assert_eq!(result, Err(failed(code, true)), "{kind:?}");
            assert!(api.seen().is_empty(), "{kind:?}: 구판이 상류로 보냈다");
            assert_eq!(keys.reads(), 0, "{kind:?}: 구판이 키를 읽었다");
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
                Keys::with("model-key", "키-한글-값-입니다".as_bytes()),
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
            api.script(text(kind, "답", None));
            let runner = ApiRunner::new(
                settings(kind, &api, Some("model-key")),
                keys,
                Arc::new(Gate::Open),
            )
            .unwrap();
            assert_eq!(step(&runner, request()).await, Err(want), "{kind:?}");
            assert!(api.seen().is_empty());
        }
        // 값 앞뒤의 줄바꿈·공백은 뗀다(보관소 파일 끝의 줄바꿈)
        let api = FakeApi::start().await;
        api.script(text(kind, "답", None));
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
        // 최종 글·호출과 함께 온 글·호출 인자·무시되는 필드(원문 점검)·JSON 이스케이프된 모양
        let leaky: Vec<(&str, Reply)> = vec![
            (
                "final text",
                text(kind, &format!("제 키는 {KEY}예요"), Some((9, 4))),
            ),
            (
                "call text",
                calls(
                    kind,
                    &format!("키 {KEY}"),
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
                            serde_json::json!({"id": KEY, "content": [{"type": "text", "text": "답"}], "stop_reason": "end_turn"})
                        }
                        ApiKind::OpenAi => {
                            serde_json::json!({"id": KEY, "choices": [{"message": {"content": "답"}, "finish_reason": "stop"}]})
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
        // 키가 없는 응답은 그대로 간다(대조)
        let api = FakeApi::start().await;
        api.script(text(kind, "키 없는 답", None));
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
        // loopback 서버는 키 없이 부른다: 인증 머리가 없다
        let api = FakeApi::start().await;
        api.script(text(kind, "로컬 답", None));
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
        // loopback이 아닌 주소는 키 없이 만들 수 없다
        let remote = ApiSettings {
            endpoint: ApiEndpoint::parse("https://api.example.com/v1").unwrap(),
            secret: None,
            ..settings(kind, &api, None)
        };
        assert!(ApiRunner::new(remote, Keys::empty(), Arc::new(Gate::Open)).is_err());
        // 모델 ID는 비어 있을 수 없다
        let nameless = ApiSettings {
            model: " ".into(),
            ..settings(kind, &api, Some("model-key"))
        };
        assert!(ApiRunner::new(nameless, Keys::empty(), Arc::new(Gate::Open)).is_err());
    }
    // 비 loopback `http`·`localhost`·자격이 든 주소는 주소 단계에서 거절한다
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
        api.script(text(kind, "보내면 안 되는 답", Some((9, 4))));
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
            "{kind:?}: 철회된 목적지가 키와 대화를 받았다"
        );
        assert_eq!(keys.reads(), 1, "{kind:?}: 키는 한 번 읽고 버렸다");
    }
}

/// JSON 이스케이프가 필요한 글자(`"`·`\`)가 든 키
const ESCAPED_KEY: &str = "sk-te\"st\\0123456789";

#[tokio::test]
async fn a_key_that_needs_json_escaping_is_still_caught_in_every_decoded_string() {
    for kind in KINDS {
        let mut replies = vec![
            // 인자 안
            calls(
                kind,
                "",
                vec![call("agenda.list", serde_json::json!({"q": ESCAPED_KEY}))],
                Some((5, 2)),
            ),
            // 최종 글 안
            text(kind, &format!("제 키는 {ESCAPED_KEY}예요"), Some((5, 2))),
        ];
        if kind == ApiKind::OpenAi {
            // 호출 서명 안: 워커가 체크포인트에 적는 불투명 상태로 들어갈 자리다
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
