//! `stream: true`(실제 리스너): SSE 틀·사용량 규칙·UTF-8 조각·keep-alive·기한/승인 안내·헤더 뒤 오류·끊김과 재조회·종료.
use crate::openai_support::*;
use bot_core::{api::ApiState, command::TokenUsage};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const WITHIN: Duration = Duration::from_secs(8);

fn tuned(wait_ms: u64, keep_alive: Option<Duration>) -> Setup {
    Setup {
        keep_alive,
        tune: Box::new(move |host| {
            let endpoint = host.http.openai.get_mut("openai").unwrap();
            endpoint.wait_ms = Some(wait_ms);
            endpoint.max_active_runs = Some(8);
        }),
        ..Setup::default()
    }
}

/// 이벤트를 JSON(데이터)·keep-alive·DONE으로 나눈다
#[derive(Debug)]
enum Event {
    Json(Value),
    KeepAlive,
    Done,
}

fn events(chunks: &[String]) -> Vec<Event> {
    chunks
        .iter()
        .map(|chunk| match chunk.as_str() {
            ": keep-alive\n\n" => Event::KeepAlive,
            "data: [DONE]\n\n" => Event::Done,
            other => Event::Json(data_json(other).unwrap_or_else(|| panic!("{other:?}"))),
        })
        .collect()
}

fn objects(events: &[Event]) -> Vec<&Value> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Json(value) => Some(value),
            _ => None,
        })
        .collect()
}

fn text_of(events: &[Event]) -> String {
    objects(events)
        .iter()
        .filter_map(|c| c["choices"][0]["delta"]["content"].as_str())
        .collect()
}

#[tokio::test]
async fn a_stream_has_the_official_frame_and_never_carries_usage_unless_asked() {
    let rig = rig().await;
    rig.api.start_as(answer("안녕하세요 😀 세계"));
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("s1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(sse.status, 200);
    assert_eq!(
        sse.header("content-type"),
        Some("text/event-stream; charset=utf-8")
    );
    assert_eq!(sse.header("cache-control"), Some("no-cache"));
    assert_eq!(sse.header("x-accel-buffering"), Some("no"));
    assert_eq!(sse.header("idempotency-key"), Some("s1"));
    assert!(sse.header("x-request-id").is_some());
    let all = events(&sse.rest(WITHIN).await);
    assert!(matches!(all.last(), Some(Event::Done)), "{all:?}");
    let chunks = objects(&all);
    // 첫 청크는 역할만, 마지막 선택지는 빈 delta와 종료 사유
    assert_eq!(
        chunks[0]["choices"][0]["delta"],
        json!({"role": "assistant", "content": ""})
    );
    assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
    let last = chunks.last().unwrap();
    assert_eq!(
        (
            last["choices"][0]["delta"].clone(),
            last["choices"][0]["finish_reason"].clone()
        ),
        (json!({}), json!("stop"))
    );
    assert_eq!(text_of(&all), "안녕하세요 😀 세계");
    for chunk in &chunks {
        assert_eq!(chunk["object"], "chat.completion.chunk");
        assert_eq!(chunk["id"], "chatcmpl-00000000-0000-4000-8000-000000000001");
        assert_eq!(chunk["created"], 1_700_000_001u64);
        assert_eq!(chunk["model"], "assistant");
        assert!(
            chunk.get("usage").is_none(),
            "요청하지 않으면 usage 키 자체가 없다: {chunk}"
        );
    }
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn usage_chunks_follow_include_usage_and_only_measured_numbers_are_sent() {
    let rig = rig().await;
    rig.api.start_as(ApiState::Final {
        text: "측정된 답".into(),
        truncated: false,
        usage: Some(TokenUsage {
            prompt: 5,
            completion: 7,
        }),
    });
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("us1"),
        &ask_stream("assistant", "q", true),
    )
    .await;
    let all = events(&sse.rest(WITHIN).await);
    let chunks = objects(&all);
    let (stats, ordinary) = chunks.split_last().unwrap();
    // 일반 청크는 usage:null, 마지막 통계 청크만 choices:[] + 실측값
    assert!(
        ordinary
            .iter()
            .all(|c| c["usage"].is_null() && c.get("usage").is_some()),
        "{ordinary:?}"
    );
    assert_eq!(stats["choices"], json!([]));
    assert_eq!(
        stats["usage"],
        json!({"prompt_tokens": 5, "completion_tokens": 7, "total_tokens": 12})
    );
    assert!(matches!(all.last(), Some(Event::Done)));
    assert_eq!(
        ordinary.last().unwrap()["choices"][0]["finish_reason"],
        "stop"
    );
    // 사용량을 모르면 통계 청크를 생략한다(일반 청크의 usage:null은 유지)
    rig.api.start_as(answer("모르는 답"));
    let mut unknown = Sse::open(
        &rig,
        BUILD_KEY,
        Some("us2"),
        &ask_stream("assistant", "q", true),
    )
    .await;
    let all = events(&unknown.rest(WITHIN).await);
    let chunks = objects(&all);
    assert!(
        chunks
            .iter()
            .all(|c| c["usage"].is_null() && !c["choices"].as_array().unwrap().is_empty()),
        "{chunks:?}"
    );
    assert!(matches!(all.last(), Some(Event::Done)));
    // 잘린 답은 length
    rig.api.start_as(ApiState::Final {
        text: "잘림".into(),
        truncated: true,
        usage: None,
    });
    let mut clipped = Sse::open(
        &rig,
        BUILD_KEY,
        Some("us3"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let all = events(&clipped.rest(WITHIN).await);
    assert_eq!(
        objects(&all).last().unwrap()["choices"][0]["finish_reason"],
        "length"
    );
}

#[tokio::test]
async fn a_long_answer_is_split_without_breaking_characters_and_rebuilds_exactly() {
    let rig = rig().await;
    let text = format!("{}\n\"끝\"\\{}", "한글 😀 ".repeat(3000), "e\u{301}");
    rig.api.start_as(answer(&text));
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("long"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let raw = sse.rest(WITHIN).await;
    assert!(
        raw.iter().filter(|c| c.contains("\"content\":\"")).count() > 3,
        "여러 조각이다"
    );
    assert!(
        raw.iter().all(|c| c.is_char_boundary(c.len())),
        "각 이벤트가 온전한 UTF-8"
    );
    assert_eq!(text_of(&events(&raw)), text);
}

#[tokio::test]
async fn keep_alive_comments_arrive_while_waiting_and_the_answer_follows() {
    let rig = rig_with(tuned(4000, Some(Duration::from_millis(200)))).await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("ka1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let first = sse.next(WITHIN).await;
    assert!(
        matches!(&first, Next::Data(chunk) if chunk.contains("\"role\":\"assistant\"")),
        "{first:?}"
    );
    // 쓸 것이 없는 동안 keep-alive가 이어진다(간격의 몇 배 안에 둘 이상)
    let mut beats = 0;
    let started = Instant::now();
    while beats < 2 {
        match sse.next(Duration::from_millis(900)).await {
            Next::Data(chunk) => {
                assert_eq!(chunk, ": keep-alive\n\n");
                beats += 1;
            }
            other => panic!(
                "keep-alive가 와야 해요({:?} 경과): {other:?}",
                started.elapsed()
            ),
        }
    }
    rig.api.set_state("ka1", answer("기다린 보람"));
    let rest = events(&sse.rest(WITHIN).await);
    assert_eq!(text_of(&rest), "기다린 보람");
    assert!(matches!(rest.last(), Some(Event::Done)));
}

#[tokio::test]
async fn the_default_keep_alive_stays_under_the_documented_fifteen_seconds() {
    // 운영 기본값의 상한을 코드로 고정한다(느린 시험이 아니라 값 자체를 본다)
    assert!(bot_gateway::http::KEEP_ALIVE_DEFAULT <= Duration::from_secs(15));
}

#[tokio::test]
async fn the_deadline_and_an_approval_wait_end_the_stream_with_the_fixed_notice() {
    let rig = rig_with(tuned(1200, None)).await;
    let started = Instant::now();
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("dl1"),
        &ask_stream("assistant", "q", true),
    )
    .await;
    let all = events(&sse.rest(WITHIN).await);
    let waited = started.elapsed();
    assert!(waited < Duration::from_millis(1250), "{waited:?}");
    assert_eq!(
        text_of(&all),
        "작업을 처리 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
    );
    assert!(matches!(all.last(), Some(Event::Done)));
    // 고정 안내에는 측정값이 없다: include_usage여도 통계 청크가 없다
    assert!(
        objects(&all)
            .iter()
            .all(|c| !c["choices"].as_array().unwrap().is_empty())
    );
    // 승인 대기는 기한을 기다리지 않고 즉시 안내한다
    rig.api.set_state("dl1", ApiState::NeedsHuman);
    let started = Instant::now();
    let mut approval = Sse::open(
        &rig,
        BUILD_KEY,
        Some("dl1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    let all = events(&approval.rest(WITHIN).await);
    assert!(
        started.elapsed() < Duration::from_millis(800),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        text_of(&all),
        "승인 대기 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요."
    );
    // 같은 키로 비스트림 재조회하면 최종 답을 새 실행 없이 받는다
    rig.api.set_state("dl1", answer("최종 답"));
    let json = chat(&rig, BUILD_KEY, Some("dl1"), &ask("assistant", "q")).await;
    assert_eq!(json.json()["choices"][0]["message"]["content"], "최종 답");
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn an_error_after_the_headers_is_one_error_event_and_a_closed_stream() {
    let rig = rig().await;
    rig.api.start_as(ApiState::Failed {
        reason: "model_error".into(),
    });
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("er1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(sse.status, 200, "접수까지 끝난 뒤라 이미 200이다");
    let raw = sse.rest(WITHIN).await;
    let all = events(&raw);
    let objects = objects(&all);
    assert_eq!(objects.len(), 2, "{raw:?}");
    assert_eq!(objects[0]["choices"][0]["delta"]["role"], "assistant");
    assert_eq!(
        objects[1],
        &json!({"error": {"message": objects[1]["error"]["message"], "type": "server_error", "param": null, "code": "run_failed"}})
    );
    assert!(
        !all.iter().any(|e| matches!(e, Event::Done)),
        "성공 종료 표지가 없다"
    );
    assert!(!raw.iter().any(|c| c.contains("finish_reason\":\"stop")));
    assert!(
        !raw.iter().any(|c| c.contains("model_error")),
        "내부 사유를 되풀이하지 않는다"
    );
}

#[tokio::test]
async fn revoking_access_while_streaming_ends_with_an_error_event_and_no_content() {
    let rig = rig_with(tuned(2000, None)).await;
    finish_later(
        &rig,
        "rv1",
        answer("권한 철회 뒤에 나가면 안 되는 글"),
        Duration::from_millis(400),
    );
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("rv1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(chunk) if chunk.contains("assistant")));
    rig.catalog.replace([fixture_shaped(Shape {
        build_member: false,
        ..Shape::default()
    })
    .build()]);
    let raw = sse.rest(WITHIN).await;
    assert_eq!(raw.len(), 1, "{raw:?}");
    let error = data_json(&raw[0]).unwrap();
    assert_eq!(
        (
            error["error"]["type"].as_str(),
            error["error"]["code"].as_str()
        ),
        (Some("permission_error"), Some("access_denied"))
    );
    assert!(!raw.concat().contains("나가면 안 되는"));
}

#[tokio::test]
async fn problems_before_acceptance_are_plain_json_errors_even_when_streaming_was_asked() {
    let rig = rig().await;
    let bad = json!({"model": "assistant", "stream": true, "messages": [], "tools": []});
    let reply = chat(&rig, BUILD_KEY, Some("pj1"), &bad).await;
    assert_eq!(
        (reply.status, reply.header("content-type")),
        (400, Some("application/json"))
    );
    let denied = chat(
        &rig,
        "nope-0123456789abcdef0123456789abcdef0",
        Some("pj2"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(
        (denied.status, denied.header("content-type")),
        (401, Some("application/json"))
    );
    let missing = chat(
        &rig,
        BUILD_KEY,
        Some("pj3"),
        &ask_stream("nope", "q", false),
    )
    .await;
    assert_eq!(
        (missing.status, missing.error().1.as_str()),
        (404, "model_not_found")
    );
    rig.api.fault(bot_gateway::testing::api::Fault::Unavailable);
    let down = chat(
        &rig,
        BUILD_KEY,
        Some("pj4"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(
        (down.status, down.header("content-type")),
        (503, Some("application/json"))
    );
    assert_eq!(rig.api.runs(), 0);
}

#[tokio::test]
async fn a_dropped_connection_stops_waiting_but_not_the_work_and_the_same_key_collects_it() {
    // keep-alive는 기본(10초)이다: 끊김을 주석 쓰기로 알아챌 수 없으니 기다림이 끊김 자체를 지켜보아야 곧바로 멈춘다
    let rig = rig_with(tuned(5000, None)).await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("dc1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(_)));
    drop(sse);
    // 서버의 기다림이 멈춘다(조회가 더는 늘지 않는다)
    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled = rig.api.lookups();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        rig.api.lookups(),
        settled,
        "끊긴 스트림이 계속 조회한다: {} → {}",
        settled,
        rig.api.lookups()
    );
    // 작업은 그대로이고 같은 키·같은 요청으로 새 실행 없이 되찾는다
    assert_eq!(
        rig.api.run_of("dc1").map(|r| r.state),
        Some(ApiState::Active)
    );
    rig.api.set_state("dc1", answer("끊겨도 남은 결과"));
    let again = chat(&rig, BUILD_KEY, Some("dc1"), &ask("assistant", "q")).await;
    assert_eq!(
        again.json()["choices"][0]["message"]["content"],
        "끊겨도 남은 결과"
    );
    assert_eq!(rig.api.runs(), 1);
}

#[tokio::test]
async fn shutting_down_ends_a_waiting_stream_with_the_notice_and_the_done_marker() {
    let rig = rig_with(tuned(4000, None)).await;
    let mut sse = Sse::open(
        &rig,
        BUILD_KEY,
        Some("sd1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert!(matches!(sse.next(WITHIN).await, Next::Data(_)));
    let started = Instant::now();
    rig.stop.cancel();
    let all = events(&sse.rest(WITHIN).await);
    assert!(
        started.elapsed() < Duration::from_millis(1000),
        "{:?}",
        started.elapsed()
    );
    assert!(text_of(&all).starts_with("작업을 처리 중입니다."));
    assert!(matches!(all.last(), Some(Event::Done)));
}

#[tokio::test]
async fn a_replayed_run_whose_model_was_reassigned_is_a_plain_404_before_any_stream_starts() {
    let rig = rig().await;
    rig.api.start_as(answer("접수 당시의 결과"));
    assert_eq!(
        chat(&rig, BUILD_KEY, Some("sr1"), &ask("assistant", "q"))
            .await
            .status,
        200
    );
    rig.api.reassign("sr1", "helper", "side");
    let reply = chat(
        &rig,
        BUILD_KEY,
        Some("sr1"),
        &ask_stream("assistant", "q", false),
    )
    .await;
    assert_eq!(
        (reply.status, reply.header("content-type")),
        (404, Some("application/json")),
        "{}",
        reply.body
    );
    assert_eq!(reply.error().1, "model_not_found");
    assert!(!reply.body.contains("접수 당시"));
}

#[tokio::test]
async fn measured_usage_is_never_streamed_when_it_was_not_asked_for() {
    let rig = rig().await;
    rig.api.start_as(ApiState::Final {
        text: "측정된 답".into(),
        truncated: false,
        usage: Some(TokenUsage {
            prompt: 5,
            completion: 7,
        }),
    });
    for (key, include) in [("nu1", false), ("nu2", false)] {
        let mut body = ask_stream("assistant", "q", false);
        if include {
            body["stream_options"] = json!({"include_usage": false});
        }
        let mut sse = Sse::open(&rig, BUILD_KEY, Some(key), &body).await;
        let raw = sse.rest(WITHIN).await;
        assert!(!raw.concat().contains("usage"), "{raw:?}");
        assert!(!raw.concat().contains("prompt_tokens"));
    }
    // include_usage:false를 명시해도 같다
    let body = json!({"model": "assistant", "stream": true, "stream_options": {"include_usage": false},
        "messages": [{"role": "user", "content": "q"}]});
    let mut sse = Sse::open(&rig, BUILD_KEY, Some("nu3"), &body).await;
    assert!(!sse.rest(WITHIN).await.concat().contains("usage"));
}

#[tokio::test]
async fn a_stream_whose_lookup_is_pending_at_the_deadline_ends_with_the_notice_not_an_error() {
    for lookup_ms in [20u64, 60_000] {
        let rig = rig_with(tuned(900, None)).await;
        rig.api.delay_lookups(Duration::from_millis(lookup_ms));
        let mut sse = Sse::open(
            &rig,
            BUILD_KEY,
            Some("pend2"),
            &ask_stream("assistant", "q", false),
        )
        .await;
        let all = events(&sse.rest(WITHIN).await);
        assert_eq!(
            text_of(&all),
            "작업을 처리 중입니다. 같은 Idempotency-Key와 같은 요청으로 다시 조회하세요.",
            "조회 {lookup_ms}ms"
        );
        assert!(matches!(all.last(), Some(Event::Done)), "{all:?}");
    }
}
