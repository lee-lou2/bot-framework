//! Tests for SSE key-prefix holdback buffering and per-event size limits.

use super::*;
use std::time::Duration;

const PING: &str = "event: ping\ndata: {\"type\":\"ping\"}\n\n";

async fn relay(r: &Rig, chunks: &[&str], end: End) -> Vec<String> {
    let bytes: Vec<&[u8]> = chunks.iter().map(|c| c.as_bytes()).collect();
    r.upstream.respond_chunks(200, &bytes, end);
    let mut stream = r
        .proxy
        .forward_stream(request(STREAM_BODY, &[]))
        .await
        .unwrap();
    collect(&mut stream).await
}

fn leaked_head(all: &str) -> usize {
    (1..=KEY.len())
        .take_while(|n| all.contains(&KEY[..*n]))
        .count()
}

#[tokio::test]
async fn no_more_than_a_stub_of_a_split_key_ever_leaves() {
    for cut in 1..KEY.len() {
        let r = rig(|_| {});
        let events = [
            delta("hello "),
            delta(&KEY[..cut]),
            delta(&KEY[cut..]),
            delta(" bye"),
        ];
        let relayed = relay(&r, &events.each_ref().map(String::as_str), End::Eof).await;
        let all = relayed.concat();
        assert!(!all.contains(KEY), "cut {cut}: {all}");
        assert!(
            leaked_head(&all) < super::super::MIN_HOLD,
            "cut {cut}: leaked {} bytes of key prefix",
            leaked_head(&all)
        );
        assert!(
            relayed.last().unwrap().contains("blocked"),
            "cut {cut}: {all}"
        );
        assert!(!all.contains(" bye"));
    }
}

#[tokio::test]
async fn a_key_spread_over_many_events_with_other_events_between_is_dropped_whole() {
    let r = rig(|_| {});
    let events = [
        delta("a "),
        delta("provid"),
        PING.to_owned(),
        delta("er-key-"),
        PING.to_owned(),
        delta("for-"),
        delta("personal"),
        delta(" after"),
    ];
    let relayed = relay(&r, &events.each_ref().map(String::as_str), End::Eof).await;
    let all = relayed.concat();
    assert_eq!(relayed.len(), 2, "{all}");
    assert_eq!(relayed[0], events[0]);
    assert!(relayed[1].contains("blocked"));
    assert!(leaked_head(&all) < 3, "{all}");
}

#[tokio::test]
async fn held_events_go_out_in_order_once_no_window_looks_like_a_key_start() {
    let r = rig(|_| {});
    let events = [
        delta("a "),
        delta("see provide"),
        PING.to_owned(),
        delta("d it"),
        delta("fin"),
        STOP.to_owned(),
    ];
    let relayed = relay(&r, &events.each_ref().map(String::as_str), End::Eof).await;
    assert_eq!(relayed, events);
}

const STOP: &str = "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

#[tokio::test]
async fn only_a_message_stop_releases_what_is_held() {
    let r = rig(|c| c.stream_idle = Duration::from_millis(80));
    let events = [delta("a "), delta("provider-k")];
    let late = delta("after the end");
    let chunks = [events[0].as_str(), events[1].as_str(), STOP, late.as_str()];
    let relayed = relay(&r, &chunks, End::Eof).await;
    assert_eq!(relayed, [events[0].as_str(), events[1].as_str(), STOP]);
    let refs = events.each_ref().map(String::as_str);
    let relayed = relay(&r, &refs, End::Eof).await;
    assert_eq!(relayed.len(), 2, "{relayed:?}");
    assert_eq!(relayed[0], events[0]);
    assert!(relayed[1].contains("truncated"), "{relayed:?}");
    assert!(!relayed.concat().contains("provider"));
    for (end, class) in [(End::Fail, "unavailable"), (End::Hang, "timeout")] {
        let relayed = relay(&r, &refs, end).await;
        assert_eq!(relayed.len(), 2, "{relayed:?}");
        assert_eq!(relayed[0], events[0]);
        assert!(relayed[1].contains(class), "{relayed:?}");
        assert!(!relayed.concat().contains("provider"));
    }
}

#[tokio::test]
async fn the_words_message_stop_inside_other_data_do_not_count_as_the_end() {
    let r = rig(|_| {});
    let fake = "event: ping\ndata: {\"type\":\"ping\",\"note\":\"message_stop\"}\n\n";
    let text = delta("message_stop");
    let events = [delta("a "), delta("provider-k"), fake.to_owned()];
    let refs = events.each_ref().map(String::as_str);
    let relayed = relay(&r, &refs, End::Eof).await;
    assert!(relayed.last().unwrap().contains("truncated"), "{relayed:?}");
    assert!(!relayed.concat().contains("provider"), "{relayed:?}");
    let events = [delta("a "), text, STOP.to_owned()];
    let refs = events.each_ref().map(String::as_str);
    assert_eq!(relay(&r, &refs, End::Eof).await, events);
}

#[tokio::test]
async fn short_key_beginnings_pass_immediately_and_longer_ones_wait() {
    let r = rig(|c| c.stream_idle = Duration::from_millis(80));
    // Prefixes shorter than MIN_HOLD (e.g. "pr") flush immediately; 3+ bytes ("pro") hold.
    let events = [delta("hi p"), delta("r")];
    let relayed = relay(&r, &events.each_ref().map(String::as_str), End::Hang).await;
    assert_eq!(relayed.len(), 3, "{relayed:?}");
    assert_eq!(relayed[..2], events);
    let relayed = relay(&r, &[delta("hi pro").as_str()], End::Hang).await;
    assert_eq!(relayed.len(), 1, "{relayed:?}");
    assert!(relayed[0].contains("timeout"));
}

#[tokio::test]
async fn a_complete_event_over_the_size_limit_is_refused_even_with_its_terminator() {
    let r = rig(|c| c.max_event = 64);
    let big = format!("data: {{\"text\":\"{}\"}}\n\n", "a".repeat(100));
    let relayed = relay(
        &r,
        &["data: {}\n\n", big.as_str(), "data: {}\n\n"],
        End::Eof,
    )
    .await;
    assert_eq!(relayed.len(), 2, "{relayed:?}");
    assert_eq!(relayed[0], "data: {}\n\n");
    assert!(relayed[1].contains("too_large"));
    assert!(!relayed.concat().contains("aaaa"));
}

#[tokio::test]
async fn any_end_of_stream_without_a_message_stop_is_a_truncated_one() {
    let r = rig(|_| {});
    for chunks in [
        vec![delta("hello")],
        vec![],
        vec![delta("hello"), "data: {\"cut\":".to_owned()],
    ] {
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let relayed = relay(&r, &refs, End::Eof).await;
        assert!(
            relayed
                .last()
                .unwrap()
                .contains("\"message\":\"truncated\""),
            "{relayed:?}"
        );
        assert!(!relayed.concat().contains("message_stop"));
    }
}

#[tokio::test]
async fn only_an_event_named_and_typed_message_stop_ends_the_stream() {
    let r = rig(|_| {});
    let after = delta("after");
    for fake in [
        "event: ping\ndata: {\"type\":\"message_stop\"}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"ping\"}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\ndata: {}\n\n",
        "event: message_stop\nevent: ping\ndata: {\"type\":\"message_stop\"}\n\n",
    ] {
        let relayed = relay(&r, &[fake, after.as_str()], End::Eof).await;
        let joined = relayed.concat();
        assert!(
            joined.contains("after"),
            "{fake}: must not stop stream on fake stop event: {joined}"
        );
        assert!(
            relayed.last().unwrap().contains("truncated"),
            "{fake}: {relayed:?}"
        );
    }
    let relayed = relay(&r, &[STOP, after.as_str()], End::Eof).await;
    assert_eq!(relayed, [STOP]);
}
