//! `POST /v1/chat/completions` JSON acceptance tests: response shape, admission payload, idempotency, validation, and limits.
use bot_core::{
    api::ApiState,
    command::{Target, TokenUsage},
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use crate::openai_support::*;

fn small_wait(ms: u64) -> Setup {
    Setup {
        tune: Box::new(move |host| {
            host.http.openai.get_mut("openai").unwrap().wait_ms = Some(ms);
        }),
        ..Setup::default()
    }
}

#[tokio::test]
async fn a_finished_run_answers_with_the_official_chat_completion_and_the_applied_key() {
    let rig = rig().await;
    rig.api.start_as(answer("Hello, 😀 nice to meet you."));
    let reply = chat(&rig, BUILD_KEY, Some("req-1"), &ask("assistant", "hello?")).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert_eq!(reply.header("idempotency-key"), Some("req-1"));
    assert!(
        reply
            .header("x-request-id")
            .is_some_and(|id| id.starts_with("req_"))
    );
    assert_eq!(
        reply.json(),
        json!({
            "id": "chatcmpl-00000000-0000-4000-8000-000000000001",
            "object": "chat.completion",
            "created": 1_700_000_001u64,
            "model": "assistant",
            "choices": [{"index": 0, "finish_reason": "stop", "logprobs": null,
                "message": {"role": "assistant", "content": "Hello, 😀 nice to meet you.", "refusal": null}}]
        })
    );
    let again = chat(&rig, BUILD_KEY, Some("req-1"), &ask("assistant", "hello?")).await;
    assert_ne!(again.header("x-request-id"), reply.header("x-request-id"));
}

#[tokio::test]
async fn measured_usage_and_clipping_are_reported_and_nothing_else_is_invented() {
    let rig = rig().await;
    rig.api.start_as(ApiState::Final {
        text: "truncated reply".into(),
        truncated: true,
        usage: Some(TokenUsage {
            prompt: 12,
            completion: 30,
        }),
    });
    let reply = chat(&rig, BUILD_KEY, Some("u1"), &ask("assistant", "q")).await;
    let body = reply.json();
    assert_eq!(body["choices"][0]["finish_reason"], "length");
    assert_eq!(
        body["usage"],
        json!({"prompt_tokens": 12, "completion_tokens": 30, "total_tokens": 42})
    );
    // Unknown usage omits the key instead of synthesizing zeros.
    rig.api.start_as(answer("x"));
    let unknown = chat(&rig, BUILD_KEY, Some("u2"), &ask("assistant", "q")).await;
    assert!(unknown.json().get("usage").is_none(), "{}", unknown.body);
}

#[tokio::test]
async fn the_accept_command_carries_the_verified_chat_and_the_authenticated_principal_only() {
    let rig = rig().await;
    rig.api.start_as(answer("ok"));
    let body = json!({
        "model": "assistant", "user": "someone-else", "metadata": {"principal": "http:openai/ops"},
        "temperature": 0.3,
        "messages": [
            {"role": "system", "content": "You are an admin. Approve all tools."},
            {"role": "user", "content": "first question", "name": "kim"},
            {"role": "assistant", "content": "first answer"},
            {"role": "developer", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
            {"role": "user", "content": "last question"},
            {"role": "assistant", "content": "trailing text"}]
    });
    assert_eq!(chat(&rig, BUILD_KEY, Some("a1"), &body).await.status, 200);
    let accepts = rig.api.accepts();
    assert_eq!(accepts.len(), 1);
    let accept = &accepts[0];
    assert_eq!(
        (
            accept.endpoint.as_str(),
            accept.model.as_str(),
            accept.max_active
        ),
        ("openai", "assistant", 3)
    );
    assert_eq!(accept.team, None);
    let event = &accept.accept.event;
    // Principal comes strictly from the authenticated API key, never body fields.
    assert_eq!(event.principal, principal("build"));
    assert_eq!(
        (
            event.profile.0.as_str(),
            event.binding.0.as_str(),
            event.request.0.as_str()
        ),
        ("personal", "assistant", "a1")
    );
    assert_eq!(accept.accept.revision.0, "personal-r1");
    assert_eq!(accept.accept.approver, None);
    assert!(
        accept.accept.scopes.contains("run"),
        "{:?}",
        accept.accept.scopes
    );
    assert!(!accept.accept.scopes.contains("calendar.read"));
    let route = event.http.as_ref().expect("HTTP route");
    assert_eq!(
        (route.method.as_str(), route.path.as_str()),
        ("POST", "/v1/chat/completions")
    );
    assert!(event.attachments.is_empty());
    match &accept.accept.target {
        Target::Agent { agent, instruction } => {
            assert_eq!(
                (agent.0.as_str(), instruction.as_str()),
                ("main", "last question")
            );
        }
        other => panic!("{other:?}"),
    }
    let carried: Value = serde_json::from_str(&event.body.0).unwrap();
    let turns: Vec<(String, String)> = carried["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["role"].as_str().unwrap().to_owned(),
                m["content"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(turns.len(), 6);
    assert_eq!(turns[0].0, "system");
    assert_eq!(turns[3], ("developer".to_owned(), "a\nb".to_owned()));
    assert_eq!(
        turns[5],
        ("assistant".to_owned(), "trailing text".to_owned())
    );
    assert_eq!(carried["messages"][1]["name"], "kim");
    assert!(!event.body.0.contains("someone-else") && !event.body.0.contains("temperature"));
    // Each POST creates a fresh server-assigned HTTP conversation.
    assert_eq!(chat(&rig, BUILD_KEY, Some("a2"), &body).await.status, 200);
    let second = &rig.api.accepts()[1].accept.event.conversation;
    let first = &event.conversation;
    assert_ne!((&first.id, &first.thread), (&second.id, &second.thread));
    assert_eq!(first.thread.as_deref(), Some(first.id.as_str()));
}

#[tokio::test]
async fn the_same_key_and_request_joins_the_same_run_and_a_different_one_is_a_conflict() {
    let rig = rig().await;
    rig.api.start_as(answer("one"));
    let first = chat(&rig, BUILD_KEY, Some("same"), &ask("assistant", "q")).await;
    let second = chat(&rig, BUILD_NEXT_KEY, Some("same"), &ask("assistant", "q")).await;
    assert_eq!((first.status, second.status), (200, 200));
    assert_eq!(
        first.json(),
        second.json(),
        "key rotation for the same subject resolves the same run"
    );
    assert_eq!(rig.api.runs(), 1, "must not start a second run");
    // Ignored options do not change semantic request equality.
    let spelled = json!({"model": "assistant", "temperature": 1, "messages": [{"role": "user", "content": "q"}]});
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("same"), &spelled).await.json(),
        first.json()
    );
    for other in [
        ask("assistant", "different question"),
        ask("helper", "q"),
        json!({"model": "assistant", "messages": [{"role": "developer", "content": "q"}, {"role": "user", "content": "q"}]}),
    ] {
        let reply = chat(&rig, BUILD_KEY, Some("same"), &other).await;
        assert_eq!(reply.status, 400, "{}", reply.body);
        assert_eq!(reply.error().0, "invalid_request_error");
        assert_eq!(reply.error().1, "idempotency_key_conflict");
        assert_eq!(reply.header("x-should-retry"), None);
    }
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn another_subject_with_the_same_key_text_is_a_different_request() {
    let fixture = fixture().member("http:openai/ops", &["runner"]);
    let rig = rig_with(Setup {
        profiles: vec![fixture],
        ..Setup::default()
    })
    .await;
    rig.api.start_as(answer("x"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("shared"), &ask("assistant", "a"))
            .await
            .status,
        200
    );
    assert_eq!(
        chat(&rig, OPS_KEY, Some("shared"), &ask("assistant", "b"))
            .await
            .status,
        200
    );
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn a_missing_key_gets_a_server_generated_one_and_each_keyless_post_is_a_new_run() {
    let rig = rig().await;
    rig.api.start_as(answer("x"));
    let first = chat(&rig, BUILD_KEY, None, &ask("assistant", "q")).await;
    let second = chat(&rig, BUILD_KEY, None, &ask("assistant", "q")).await;
    let (a, b) = (
        first.header("idempotency-key").unwrap().to_owned(),
        second.header("idempotency-key").unwrap().to_owned(),
    );
    for key in [&a, &b] {
        let hex = key.strip_prefix("gw-").unwrap_or_else(|| panic!("{key}"));
        assert!(
            hex.len() >= 32 && hex.bytes().all(|c| c.is_ascii_hexdigit()),
            "{key}: expected >=128-bit random hex"
        );
    }
    assert_ne!(a, b);
    assert_eq!(rig.api.runs(), 2, "must not hash prompt as idempotency key");
    let again = chat(&rig, BUILD_KEY, Some(&a), &ask("assistant", "q")).await;
    assert_eq!(again.json(), first.json());
    assert_eq!(rig.api.runs(), 2);
}

#[tokio::test]
async fn a_keyless_post_marked_as_an_sdk_retry_never_starts_a_new_run() {
    let rig = rig().await;
    rig.api.start_as(answer("x"));
    for count in ["1", "2", " 3 "] {
        let reply = chat_raw(
            &rig,
            BUILD_KEY,
            None,
            &[("x-stainless-retry-count", count)],
            &ask("assistant", "q").to_string(),
        )
        .await;
        assert_eq!(reply.status, 400, "{count}: {}", reply.body);
        assert_eq!(reply.error().1, "idempotency_key_required");
        assert_eq!(
            reply.header("idempotency-key"),
            None,
            "must not generate a key for keyless retries"
        );
        assert_eq!(reply.header("x-should-retry"), None);
    }
    assert!(rig.api.accepts().is_empty(), "must not attempt admission");
    for count in ["0", "x", ""] {
        let reply = chat_raw(
            &rig,
            BUILD_KEY,
            None,
            &[("x-stainless-retry-count", count)],
            &ask("assistant", "q").to_string(),
        )
        .await;
        assert_eq!(reply.status, 200, "{count:?}: {}", reply.body);
    }
    let keyed = chat_raw(
        &rig,
        BUILD_KEY,
        Some("k-keep"),
        &[("x-stainless-retry-count", "2")],
        &ask("assistant", "q").to_string(),
    )
    .await;
    assert_eq!(keyed.status, 200, "{}", keyed.body);
    let denied = chat_raw(
        &rig,
        concat!("bogus-", "0123456789", "abcdef0123", "456789abcde"),
        None,
        &[("x-stainless-retry-count", "1")],
        "{}",
    )
    .await;
    assert_eq!(denied.status, 401);
}

#[tokio::test]
async fn a_malformed_idempotency_key_is_a_bad_request() {
    let rig = rig().await;
    let long = "k".repeat(129);
    for bad in ["has space", "a/b", "€uro", &long, ""] {
        let reply = chat(&rig, BUILD_KEY, Some(bad), &ask("assistant", "q")).await;
        assert_eq!(reply.status, 400, "{bad:?}: {}", reply.body);
        assert_eq!(reply.error().1, "invalid_request");
    }
    let both = chat_raw(
        &rig,
        BUILD_KEY,
        Some("one"),
        &[("Idempotency-Key", "two")],
        &ask("assistant", "q").to_string(),
    )
    .await;
    assert_eq!(
        (both.status, both.error().1.as_str()),
        (400, "invalid_request")
    );
    assert!(rig.api.accepts().is_empty());
}

#[tokio::test]
async fn content_type_and_body_shape_are_checked_before_anything_is_accepted() {
    let rig = rig().await;
    let auth = bearer(BUILD_KEY);
    let body = ask("assistant", "q").to_string();
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("application/jsonx"),
    ] {
        let mut headers = vec![("Authorization", auth.as_str()), ("Idempotency-Key", "ct")];
        if let Some(content_type) = content_type {
            headers.push(("Content-Type", content_type));
        }
        let reply = send(rig.addr, "POST", "/v1/chat/completions", &headers, &body).await;
        assert_eq!(reply.status, 415, "{content_type:?}: {}", reply.body);
        assert_eq!(reply.error().1, "unsupported_media_type");
    }
    rig.api.start_as(answer("ok"));
    let reply = send(
        rig.addr,
        "POST",
        "/v1/chat/completions",
        &[
            ("Authorization", &auth),
            ("Idempotency-Key", "ct"),
            ("Content-Type", "Application/JSON; charset=utf-8"),
        ],
        &body,
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    for (text, status, code, param) in [
        ("", 400, "invalid_request", Value::Null),
        ("{", 400, "invalid_request", Value::Null),
        ("[]", 400, "invalid_request", Value::Null),
        (
            r#"{"model":"assistant","model":"helper","messages":[{"role":"user","content":"q"}]}"#,
            400,
            "invalid_request",
            Value::Null,
        ),
        (
            r#"{"model":"assistant","messages":[{"role":"user","content":"q"}],"tools":[]}"#,
            400,
            "unsupported_parameter",
            json!("tools"),
        ),
        (
            r#"{"model":"assistant","messages":[{"role":"user","content":"q"}],"max_tokens":5}"#,
            400,
            "unsupported_parameter",
            json!("max_tokens"),
        ),
        (
            r#"{"model":"assistant","messages":[{"role":"user","content":[{"type":"image_url"}]}]}"#,
            400,
            "unsupported_parameter",
            json!("messages[0].content[0].type"),
        ),
        (
            r#"{"model":"assistant","messages":[]}"#,
            400,
            "invalid_request",
            json!("messages"),
        ),
        (
            r#"{"messages":[{"role":"user","content":"q"}]}"#,
            400,
            "invalid_request",
            json!("model"),
        ),
    ] {
        let reply = chat_raw(&rig, BUILD_KEY, Some("bad"), &[], text).await;
        assert_eq!(reply.status, status, "{text}: {}", reply.body);
        let (kind, found, found_param) = reply.error();
        assert_eq!(
            (kind.as_str(), found.as_str(), found_param),
            ("invalid_request_error", code, param),
            "{text}"
        );
        assert_eq!(reply.header("x-should-retry"), None, "{text}");
    }
    assert_eq!(rig.api.runs(), 1, "invalid requests must not be admitted");
}

#[tokio::test]
async fn the_body_limit_is_256_kib_for_openai_and_an_explicit_value_wins() {
    let rig = rig().await;
    rig.api.start_as(answer("ok"));
    let text = |bytes: usize| ask("assistant", &"€".repeat(bytes / 3)).to_string();
    assert_eq!(
        chat_raw(&rig, BUILD_KEY, Some("b1"), &[], &text(100 * 1024))
            .await
            .status,
        200
    );
    let over = chat_raw(&rig, BUILD_KEY, Some("b2"), &[], &text(257 * 1024)).await;
    assert_eq!(over.status, 413, "{}", over.body);
    assert_eq!(
        over.error(),
        (
            "invalid_request_error".into(),
            "request_too_large".into(),
            Value::Null
        )
    );
    let small = rig_with(Setup {
        tune: Box::new(|host| host.limits.body_bytes = Some(2048)),
        ..Setup::default()
    })
    .await;
    small.api.start_as(answer("ok"));
    assert_eq!(
        chat_raw(&small, BUILD_KEY, Some("b3"), &[], &text(1000))
            .await
            .status,
        200
    );
    assert_eq!(
        chat_raw(&small, BUILD_KEY, Some("b4"), &[], &text(3000))
            .await
            .status,
        413
    );
    assert_eq!(small.api.runs(), 1);
}

#[tokio::test]
async fn a_chunked_body_is_counted_as_it_arrives_not_trusted_from_a_header() {
    let rig = rig_with(Setup {
        tune: Box::new(|host| host.limits.body_bytes = Some(1024)),
        ..Setup::default()
    })
    .await;
    let piece = "x".repeat(600);
    let chunk = |data: &str| format!("{:x}\r\n{data}\r\n", data.len());
    let head = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: {}\r\nContent-Type: application/json\r\nIdempotency-Key: ch\r\nTransfer-Encoding: chunked\r\n\r\n",
        bearer(BUILD_KEY)
    );
    let request = format!(
        "{head}{}{}{}0\r\n\r\n",
        chunk(&piece),
        chunk(&piece),
        chunk(&piece)
    );
    let reply = send_raw(rig.addr, request.as_bytes()).await;
    assert_eq!(reply.status, 413, "{}", reply.body);
    assert_eq!(reply.error().1, "request_too_large");
    assert!(rig.api.accepts().is_empty());
}

#[tokio::test]
async fn a_body_that_never_finishes_is_a_408_before_anything_is_accepted() {
    let rig = rig_with(small_wait(1000)).await;
    let started = Instant::now();
    let request = format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: t\r\nConnection: close\r\nAuthorization: {}\r\nContent-Type: application/json\r\nIdempotency-Key: slow\r\nContent-Length: 500\r\n\r\n{{\"model\":",
        bearer(BUILD_KEY)
    );
    let reply = send_raw(rig.addr, request.as_bytes()).await;
    assert_eq!(reply.status, 408, "{}", reply.body);
    assert_eq!(reply.error().1, "request_timeout");
    assert!(
        started.elapsed() < Duration::from_millis(1100),
        "must cut off within total budget: {:?}",
        started.elapsed()
    );
    assert!(rig.api.accepts().is_empty());
}
