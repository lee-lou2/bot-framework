//! Tests that CLI token usage is marked measured only when every request reports non-zero counts matching the total.

use super::*;
use serde_json::json;

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
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
        end,
        json!({"type": "message_stop"}),
    ]
    .into_iter()
    .map(|event| json!({"type": "stream_event", "event": event}).to_string())
    .collect()
}

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
    json!({"type": "result", "subtype": "success", "is_error": false, "result": "Done", "usage": usage}).to_string()
}

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
    let out = finish(
        false,
        vec![reported(100, 0, 20)],
        json!({"input_tokens": 100, "output_tokens": 20}),
    );
    assert_eq!(out.outcome.usage, usage(100, 20));
    assert!(!out.outcome.truncated);
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
    let two = vec![reported(5, 0, 4), reported(9, 0, 4)];
    let out = finish(
        false,
        two.clone(),
        json!({"input_tokens": 14, "output_tokens": 8}),
    );
    assert_eq!(out.outcome.usage, usage(14, 8));
    let out = finish(
        false,
        vec![unreported(), reported(9, 0, 4)],
        json!({"input_tokens": 9, "output_tokens": 4}),
    );
    assert_eq!(out.outcome.usage, None);
    let out = finish(
        false,
        vec![reported(5, 0, 4), unreported()],
        json!({"input_tokens": 5, "output_tokens": 4}),
    );
    assert_eq!(out.outcome.usage, None);
    let out = finish(
        false,
        vec![unreported(), unreported()],
        json!({"input_tokens": 0, "output_tokens": 0}),
    );
    assert_eq!(out.outcome.usage, None);
    for total in [
        json!({"input_tokens": 20, "output_tokens": 8}),
        json!({"input_tokens": 14, "output_tokens": 9}),
        json!({"input_tokens": 13, "output_tokens": 8}),
    ] {
        let out = finish(false, two.clone(), total.clone());
        assert_eq!(out.outcome.usage, None, "{total}");
    }
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
        request(
            Some(json!({"input_tokens": 5, "output_tokens": 1})),
            Some(json!({})),
        ),
        request(
            Some(json!({"output_tokens": 1})),
            Some(json!({"output_tokens": 4})),
        ),
        request(
            Some(json!({"input_tokens": 0, "output_tokens": 1})),
            Some(json!({"output_tokens": 4})),
        ),
        request(
            Some(json!({"input_tokens": 5, "output_tokens": 1})),
            Some(json!({"output_tokens": 0})),
        ),
        request(Some(json!({"input_tokens": 5, "output_tokens": 1})), None)
            .into_iter()
            .filter(|line| !line.contains("message_delta"))
            .collect(),
    ];
    for (n, lines) in cases.into_iter().enumerate() {
        let out = finish(false, vec![lines], total.clone());
        assert_eq!(out.outcome.usage, None, "case {n}");
    }
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
    let only_start = request(Some(json!({"input_tokens": 5, "output_tokens": 4})), None);
    assert_eq!(finish(false, vec![only_start], total).outcome.usage, None);
}

#[test]
fn a_resumed_session_never_reports_a_total_but_the_observed_counts_stay_as_they_were() {
    let total = json!({"input_tokens": 100, "output_tokens": 20});
    let fresh = finish(false, vec![reported(100, 0, 20)], total.clone());
    let resumed = finish(true, vec![reported(100, 0, 20)], total);
    assert_eq!(fresh.outcome.usage, usage(100, 20));
    assert_eq!(resumed.outcome.usage, None);
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
    let out = finish(
        false,
        vec![],
        json!({"input_tokens": 100, "output_tokens": 20}),
    );
    assert_eq!(out.outcome.usage, None);
    let out = finish(
        false,
        vec![],
        json!({"input_tokens": 0, "output_tokens": 0}),
    );
    assert_eq!(out.outcome.usage, None);
}
