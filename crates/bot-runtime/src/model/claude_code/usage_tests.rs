//! 공급자가 **요청마다** 실제로 보고한 사용량만 실측으로 낸다: 한 요청이라도 보고가 없거나 일부이거나(CLI가 0으로 채운 값 포함), 요청별 합이 CLI의 총합과 어긋나거나,
//! 이어 받은 세션이면 `None`이고 0으로 지어내지 않는다.

use super::*;
use serde_json::json;

/// 한 요청의 스트림 이벤트 줄: 시작(입력 계량)·글·끝(출력 누적값)·종결. `None`이면 그 이벤트에 `usage`가 없다(공급자가 보고하지 않았다).
fn request(start: Option<Value>, delta: Option<Value>) -> Vec<String> {
    let mut message = json!({"id": "m", "type": "message", "role": "assistant", "content": []});
    if let Some(usage) = start {
        message["usage"] = usage;
    }
    let mut end = json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}});
    if let Some(usage) = delta {
        end["usage"] = usage;
    }
    [
        json!({"type": "message_start", "message": message}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "답"}}),
        end,
        json!({"type": "message_stop"}),
    ]
    .into_iter()
    .map(|event| json!({"type": "stream_event", "event": event}).to_string())
    .collect()
}

/// 보고된 요청: 시작에 입력 `input`(캐시 `cache`는 읽기), 끝에 출력 `output`.
fn reported(input: u64, cache: u64, output: u64) -> Vec<String> {
    request(
        Some(json!({"input_tokens": input, "output_tokens": 1, "cache_read_input_tokens": cache})),
        Some(json!({"output_tokens": output})),
    )
}

fn unreported() -> Vec<String> {
    request(None, None)
}

fn result(usage: Value) -> String {
    json!({"type": "result", "subtype": "success", "is_error": false, "result": "답이에요", "usage": usage}).to_string()
}

/// 요청들과 결과를 먹이고 끝낸다.
fn finish(resumed: bool, requests: Vec<Vec<String>>, total: Value) -> Output {
    let mut tracker = Tracker::new(resumed);
    for line in requests.iter().flatten() {
        tracker.feed(line);
    }
    tracker.feed(&result(total));
    Box::new(tracker).finish().unwrap()
}

fn usage(prompt: u64, completion: u64) -> Option<TokenUsage> {
    Some(TokenUsage { prompt, completion })
}

#[test]
fn one_fully_reported_request_is_measured_and_the_prompt_counts_the_cache_once() {
    // 입력 100·출력 20
    let out = finish(
        false,
        vec![reported(100, 0, 20)],
        json!({"input_tokens": 100, "output_tokens": 20}),
    );
    assert_eq!(out.outcome.usage, usage(100, 20));
    assert!(!out.outcome.truncated);
    // 캐시 읽기·쓰기는 입력에 한 번 포함된다
    let cached = request(
        Some(
            json!({"input_tokens": 100, "output_tokens": 1, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 5}),
        ),
        Some(json!({"output_tokens": 20})),
    );
    let out = finish(
        false,
        vec![cached],
        json!({"input_tokens": 100, "output_tokens": 20, "cache_read_input_tokens": 50, "cache_creation_input_tokens": 5}),
    );
    assert_eq!(out.outcome.usage, usage(155, 20));
    // 캐시만으로 채운 입력도 보고된 입력이다
    let out = finish(
        false,
        vec![request(
            Some(json!({"input_tokens": 0, "output_tokens": 1, "cache_read_input_tokens": 40})),
            Some(json!({"output_tokens": 3})),
        )],
        json!({"input_tokens": 0, "output_tokens": 3, "cache_read_input_tokens": 40}),
    );
    assert_eq!(out.outcome.usage, usage(40, 3));
}

#[test]
fn several_requests_are_measured_only_when_every_one_was_reported_and_the_sums_match() {
    // 도구 한 번 + 최종 답: 두 요청 모두 보고 → 합
    let two = vec![reported(5, 0, 4), reported(9, 0, 4)];
    let out = finish(
        false,
        two.clone(),
        json!({"input_tokens": 14, "output_tokens": 8}),
    );
    assert_eq!(out.outcome.usage, usage(14, 8));
    // 첫 요청만 미보고: CLI의 총합은 나머지 합(9/4)이라 양수지만 전체 합이 아니다
    let out = finish(
        false,
        vec![unreported(), reported(9, 0, 4)],
        json!({"input_tokens": 9, "output_tokens": 4}),
    );
    assert_eq!(out.outcome.usage, None);
    // 마지막 요청만 미보고
    let out = finish(
        false,
        vec![reported(5, 0, 4), unreported()],
        json!({"input_tokens": 5, "output_tokens": 4}),
    );
    assert_eq!(out.outcome.usage, None);
    // 모두 미보고(CLI가 0으로 채운 총합)
    let out = finish(
        false,
        vec![unreported(), unreported()],
        json!({"input_tokens": 0, "output_tokens": 0}),
    );
    assert_eq!(out.outcome.usage, None);
    // 총합이 요청별 합과 어긋난다: 이 스트림에 보이지 않은 요청이 더해졌거나 일부가 빠졌다
    for total in [
        json!({"input_tokens": 20, "output_tokens": 8}),
        json!({"input_tokens": 14, "output_tokens": 9}),
        json!({"input_tokens": 13, "output_tokens": 8}),
    ] {
        let out = finish(false, two.clone(), total.clone());
        assert_eq!(out.outcome.usage, None, "{total}");
    }
    // 총합 자체가 없거나 한쪽이 없다
    for total in [
        json!({}),
        json!({"input_tokens": 14}),
        json!({"output_tokens": 8}),
    ] {
        let out = finish(false, two.clone(), total.clone());
        assert_eq!(out.outcome.usage, None, "{total}");
    }
}

#[test]
fn a_request_with_a_one_sided_zero_or_cut_report_is_not_measured() {
    let total = json!({"input_tokens": 5, "output_tokens": 4});
    let cases = [
        // 입력만 보고(끝에 출력 누적값이 없다)
        request(
            Some(json!({"input_tokens": 5, "output_tokens": 1})),
            Some(json!({})),
        ),
        // 출력만 보고(시작에 입력이 없다)
        request(
            Some(json!({"output_tokens": 1})),
            Some(json!({"output_tokens": 4})),
        ),
        // 0으로 보고(실제 모델 요청은 입력도 출력도 0일 수 없다)
        request(
            Some(json!({"input_tokens": 0, "output_tokens": 1})),
            Some(json!({"output_tokens": 4})),
        ),
        request(
            Some(json!({"input_tokens": 5, "output_tokens": 1})),
            Some(json!({"output_tokens": 0})),
        ),
        // 끊긴 스트림: 시작만 있고 끝이 없다
        request(Some(json!({"input_tokens": 5, "output_tokens": 1})), None)
            .into_iter()
            .filter(|line| !line.contains("message_delta"))
            .collect(),
    ];
    for (n, lines) in cases.into_iter().enumerate() {
        let out = finish(false, vec![lines], total.clone());
        assert_eq!(out.outcome.usage, None, "case {n}");
    }
    // 0을 그대로 보고한 요청(총합도 0이라 합은 맞는다)은 «0토큰 실측»이 아니다
    let zeros = request(
        Some(json!({"input_tokens": 0, "output_tokens": 0})),
        Some(json!({"output_tokens": 0})),
    );
    let out = finish(
        false,
        vec![zeros],
        json!({"input_tokens": 0, "output_tokens": 0}),
    );
    assert_eq!(out.outcome.usage, None);
    // 시작 때의 출력 수는 잠정값이라 끝이 없으면 쓰지 않는다
    let only_start = request(Some(json!({"input_tokens": 5, "output_tokens": 4})), None);
    assert_eq!(finish(false, vec![only_start], total).outcome.usage, None);
}

#[test]
fn a_resumed_session_never_reports_a_total_but_the_observed_counts_stay_as_they_were() {
    let total = json!({"input_tokens": 100, "output_tokens": 20});
    let fresh = finish(false, vec![reported(100, 0, 20)], total.clone());
    let resumed = finish(true, vec![reported(100, 0, 20)], total);
    assert_eq!(fresh.outcome.usage, usage(100, 20));
    // 이어 받은 세션의 보고는 그 CLI 실행 몫뿐이다(앞 실행은 도구 승인·위임으로 끝나 보고가 없다)
    assert_eq!(resumed.outcome.usage, None);
    // 관측용 값은 그대로다(0으로 채운 기존 동작은 실측으로 쓰지 않을 뿐 바꾸지 않는다)
    assert_eq!(fresh.usage, resumed.usage);
    assert_eq!(
        fresh.usage,
        Usage {
            input_tokens: 100,
            output_tokens: 20
        }
    );
}

#[test]
fn a_result_without_any_stream_events_has_no_measurement() {
    // `--include-partial-messages` 이벤트가 없으면(요청별 보고를 확인할 수 없다) 총합이 양수여도 실측이 아니다
    let out = finish(
        false,
        vec![],
        json!({"input_tokens": 100, "output_tokens": 20}),
    );
    assert_eq!(out.outcome.usage, None);
    // 총합이 0이어도 요청이 하나도 보이지 않으면 «0토큰 실측»이 아니다
    let out = finish(
        false,
        vec![],
        json!({"input_tokens": 0, "output_tokens": 0}),
    );
    assert_eq!(out.outcome.usage, None);
}
