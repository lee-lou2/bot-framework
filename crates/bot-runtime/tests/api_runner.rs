//! API 직접 호출 실행기를 실제 HTTP(loopback 가짜 상류)로 확인한다: 전송 규칙(리다이렉트를 따르지 않음·숨은 재시도 없음·시간 한도·취소)과 상태·본문 분류. 두 형식(Anthropic·OpenAI 호환)을
//! 같은 시험으로 돌린다. 키 취급·주소 규칙은 `api_runner_keys.rs`다. 데이터베이스가 필요 없다.

mod support;

use bot_core::command::JsonPayload;
use bot_core::id::{CallId, ToolId};
use bot_runtime::model::{
    ApiKind, CallEnd, DoneCall, Exchange, Limits, ModelError, ModelRunner, ModelStep, RunnerKind,
};
use std::{sync::Arc, time::Duration};
use support::{
    api::*,
    model_request::*,
    upstream::{FakeApi, Reply, Seen},
};
use tokio::sync::watch;

#[tokio::test]
async fn a_final_answer_travels_over_real_http_with_the_key_only_in_the_secret_header() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "일정이 없어요", Some((9, 4))));
        let runner = api_runner(kind, &api);
        assert_eq!(runner.kind(), RunnerKind::Step);
        let Ok(ModelStep::Final { text, outcome, .. }) = step(&runner, request()).await else {
            panic!("{kind:?}: 최종 답이어야 해요");
        };
        assert_eq!(text, "일정이 없어요");
        assert_eq!(
            outcome.usage.map(|u| (u.prompt, u.completion)),
            Some((9, 4))
        );
        let seen = api.seen();
        assert_eq!(seen.len(), 1, "{kind:?}");
        assert_eq!(seen[0].path, path(kind));
        // 키는 머리에만 있고 본문에는 없다
        let body = String::from_utf8(seen[0].body.clone()).unwrap();
        assert!(!body.contains(KEY), "{kind:?}: 본문에 키가 있다");
        match kind {
            ApiKind::Anthropic => {
                assert_eq!(seen[0].header("x-api-key"), Some(KEY));
                assert_eq!(seen[0].header("anthropic-version"), Some("2023-06-01"));
            }
            ApiKind::OpenAi => {
                assert_eq!(
                    seen[0].header("authorization"),
                    Some(format!("Bearer {KEY}").as_str())
                );
            }
        }
        assert_eq!(seen[0].header("content-type"), Some("application/json"));
        // 시스템 글은 시스템 자리에, 사용자 줄은 사용자 메시지에 있다
        let json = seen[0].json();
        assert_eq!(json["model"], "model-x");
        let messages = json["messages"].as_array().unwrap();
        match kind {
            ApiKind::Anthropic => {
                assert_eq!(json["system"], "프로필 지침");
                assert_eq!(messages.len(), 1);
            }
            ApiKind::OpenAi => {
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages.len(), 2);
            }
        }
    }
}

#[tokio::test]
async fn a_tool_call_comes_back_with_the_tool_id_restored_and_a_made_up_name_is_not_called() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(calls(
            kind,
            "조회할게요",
            vec![call("agenda.list", serde_json::json!({"day": "오늘"}))],
            Some((5, 2)),
        ));
        let runner = api_runner(kind, &api);
        let Ok(ModelStep::Calls {
            text,
            calls,
            measured,
            ..
        }) = step(&runner, request()).await
        else {
            panic!("{kind:?}: 호출이어야 해요");
        };
        assert_eq!(text, "조회할게요");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].tool,
            ToolId("agenda.list".into()),
            "별칭이 도구 ID로 되돌아간다"
        );
        assert_eq!(calls[0].args.0, r#"{"day":"오늘"}"#);
        assert_eq!(measured.map(|u| (u.prompt, u.completion)), Some((5, 2)));
        // 요청이 보인 이름은 점이 없는 별칭이다
        assert!(
            api.seen()[0]
                .tool_names()
                .iter()
                .all(|name| !name.contains('.'))
        );
        // 표에 없는 이름(점이 든 원래 ID 포함)은 도구를 부르지 않고 고정 코드로 끝난다
        let api = FakeApi::start().await;
        api.script(Reply::With(Arc::new(move |_seen: &Seen| {
            let name = "agenda.list";
            let body = match kind {
                ApiKind::Anthropic => serde_json::json!({"content": [{"type": "tool_use", "id": "t", "name": name, "input": {}}], "stop_reason": "tool_use"}),
                ApiKind::OpenAi => serde_json::json!({"choices": [{"message": {"tool_calls": [{"id": "t", "type": "function", "function": {"name": name, "arguments": "{}"}}]}, "finish_reason": "tool_calls"}]}),
            };
            Reply::Json(200, body.to_string())
        })));
        assert_eq!(
            step(&api_runner(kind, &api), request()).await,
            Err(failed("unknown_tool", false)),
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn http_statuses_are_classified_and_nothing_is_ever_resent() {
    for kind in KINDS {
        for (status, code, transient) in [
            (400, "upstream_rejected", false),
            (401, "upstream_auth", false),
            (403, "upstream_auth", false),
            (404, "upstream_rejected", false),
            (422, "upstream_rejected", false),
            (408, "network", true),
            (429, "rate_limited", true),
            (500, "overloaded", true),
            (503, "overloaded", true),
            (529, "overloaded", true),
        ] {
            let api = FakeApi::start().await;
            api.script(Reply::Json(
                status,
                r#"{"error":{"message":"사유"}}"#.into(),
            ));
            let result = step(&api_runner(kind, &api), request()).await;
            assert_eq!(result, Err(failed(code, transient)), "{kind:?} {status}");
            assert_eq!(api.seen().len(), 1, "{kind:?} {status}: 다시 보내지 않는다");
        }
    }
}

#[tokio::test]
async fn a_429_that_says_the_quota_is_used_up_is_permanent_and_any_other_429_stays_transient() {
    let quota = r#"{"error":{"message":"You exceeded your current quota","type":"insufficient_quota","code":"insufficient_quota"}}"#;
    for kind in KINDS {
        for (body, code, transient) in [
            (quota.to_owned(), "quota_exhausted", false),
            (
                r#"{"error":{"type":"insufficient_quota"}}"#.to_owned(),
                "quota_exhausted",
                false,
            ),
            (
                r#"{"error":{"code":"insufficient_quota"}}"#.to_owned(),
                "quota_exhausted",
                false,
            ),
            (
                r#"{"error":{"type":"rate_limit_error","code":"rate_limit_exceeded"}}"#.to_owned(),
                "rate_limited",
                true,
            ),
            (String::new(), "rate_limited", true),
            ("not json".to_owned(), "rate_limited", true),
            // 상한을 넘는 본문은 끝까지 읽지 않고, 상한 뒤의 글은 보지 않는다
            (
                format!("{}{quota}", " ".repeat(200 * 1024)),
                "rate_limited",
                true,
            ),
        ] {
            let api = FakeApi::start().await;
            api.script(Reply::Json(429, body));
            let result = step(&api_runner(kind, &api), request()).await;
            assert_eq!(result, Err(failed(code, transient)), "{kind:?}");
            assert_eq!(api.seen().len(), 1, "{kind:?}: 다시 보내지 않는다");
        }
    }
}

#[tokio::test]
async fn broken_empty_or_cut_responses_are_classified_without_a_retry() {
    for kind in KINDS {
        for (reply, want) in [
            (
                Reply::Json(200, "{broken".into()),
                failed("invalid_response", false),
            ),
            (
                Reply::Json(200, String::new()),
                failed("invalid_response", false),
            ),
            (
                Reply::Json(200, "[]".into()),
                failed("invalid_response", false),
            ),
            (
                Reply::Json(200, r#"{"unexpected":true}"#.into()),
                failed("invalid_response", false),
            ),
            (
                Reply::Json(204, String::new()),
                failed("invalid_response", false),
            ),
            // 응답 없이 연결이 닫혔다: 일시 오류
            (Reply::Close, failed("network", true)),
        ] {
            let api = FakeApi::start().await;
            api.script(reply);
            let result = step(&api_runner(kind, &api), request()).await;
            assert_eq!(result, Err(want), "{kind:?}");
            assert_eq!(api.seen().len(), 1, "{kind:?}: 다시 보내지 않는다");
        }
        // 출력 한도에서 끊긴 응답은 글도 호출도 쓰지 않는다
        for with_call in [false, true] {
            let api = FakeApi::start().await;
            api.script(cut(kind, with_call));
            let result = step(&api_runner(kind, &api), request()).await;
            assert_eq!(
                result,
                Err(failed("output_limit", false)),
                "{kind:?} {with_call}"
            );
        }
    }
}

#[tokio::test]
async fn a_redirect_is_never_followed_so_the_key_never_reaches_another_host() {
    for kind in KINDS {
        let target = FakeApi::start().await;
        target.script(text(kind, "빼돌린 답", None));
        let api = FakeApi::start().await;
        api.script(Reply::Redirect(format!("{}/stolen", target.url())));
        let result = step(&api_runner(kind, &api), request()).await;
        assert_eq!(result, Err(failed("upstream_redirect", false)), "{kind:?}");
        assert_eq!(api.seen().len(), 1);
        assert!(
            target.seen().is_empty(),
            "{kind:?}: 리다이렉트 목적지가 요청을 받았다"
        );
    }
}

#[tokio::test]
async fn a_cancel_signal_cuts_the_request_and_the_total_limit_ends_a_slow_upstream() {
    for kind in KINDS {
        // 취소: 상류가 받고 답하지 않는 동안 신호가 오면 요청을 끊는다
        let api = FakeApi::start().await;
        api.script(Reply::Hang);
        let (cancel, signal) = watch::channel(false);
        let runner = Arc::new(api_runner(kind, &api));
        let running = {
            let runner = runner.clone();
            tokio::spawn(async move { runner.step(request_with(signal, Limits::default())).await })
        };
        api.wait_for(1).await;
        cancel.send(true).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("취소가 요청을 끊어야 한다")
            .unwrap();
        assert_eq!(result, Err(ModelError::Cancelled), "{kind:?}");
        // 이미 취소된 요청은 보내지도 않는다
        let (_cancel, done) = watch::channel(true);
        let before = api.seen().len();
        assert_eq!(
            runner.step(request_with(done, Limits::default())).await,
            Err(ModelError::Cancelled)
        );
        assert_eq!(api.seen().len(), before);
        // 전체 시간: 상류가 늦으면 시간 초과(일시)
        let slow = FakeApi::start().await;
        slow.script(Reply::Delayed(
            Duration::from_secs(30),
            Box::new(text(kind, "늦은 답", None)),
        ));
        let limits = Limits {
            total: Duration::from_millis(300),
            idle: Duration::from_secs(5),
        };
        let started = std::time::Instant::now();
        let result = step(
            &api_runner(kind, &slow),
            request_with(watch::channel(false).1, limits),
        )
        .await;
        assert_eq!(result, Err(ModelError::Timeout), "{kind:?}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

#[tokio::test]
async fn earlier_turns_go_back_as_tool_calls_and_results_in_the_same_request() {
    for kind in KINDS {
        let api = FakeApi::start().await;
        api.script(text(kind, "끝났어요", Some((9, 4))));
        let mut req = request();
        req.exchanges = vec![Exchange {
            text: "조회할게요".into(),
            calls: vec![DoneCall {
                call: CallId("run-1:0:0".into()),
                tool: ToolId("agenda.list".into()),
                args: JsonPayload("{}".into()),
                end: CallEnd::Done(JsonPayload(r#"{"events":[{"title":"산책"}]}"#.into())),
            }],
            state: None,
        }];
        assert!(matches!(
            step(&api_runner(kind, &api), req).await,
            Ok(ModelStep::Final { .. })
        ));
        let body = api.seen()[0].json();
        let text = body["messages"].to_string();
        assert!(
            text.contains("산책"),
            "{kind:?}: 도구 결과가 다음 요청에 실렸다"
        );
        assert!(body["messages"].as_array().unwrap().len() >= 3);
        // 이 호출은 도구 별칭으로 나간다
        let alias = api.seen()[0].alias("agenda.list");
        assert!(text.contains(&alias), "{kind:?}");
    }
}
