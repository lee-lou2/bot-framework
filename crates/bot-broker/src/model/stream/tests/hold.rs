//! 키의 앞부분으로 끝나는 창이 있는 동안 이벤트를 쥐는 규칙과, 완성된 이벤트의 크기 한도.

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

/// 나온 글 어디에든 들어 있는 키의 가장 긴 앞부분 길이.
fn leaked_head(all: &str) -> usize {
    (1..=KEY.len())
        .take_while(|n| all.contains(&KEY[..*n]))
        .count()
}

#[tokio::test]
async fn no_more_than_a_stub_of_a_split_key_ever_leaves() {
    // 키를 어디서 잘라 두 이벤트에 나눠 담아도 완성되는 이벤트는 물론 앞부분 이벤트도 나가지 않는다.
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
            "cut {cut}: 키 앞부분 {}바이트가 나갔다",
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
        delta("see provide"), // 창이 키의 앞부분으로 끝난다: 쥔다
        PING.to_owned(),      // 다른 경로의 이벤트도 순서 때문에 함께 쥔다
        delta("d it"),        // 앞부분이 아니게 되었다: 쥔 것까지 순서대로 낸다
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
    let events = [delta("a "), delta("provider-k")]; // 두 번째는 키 앞부분으로 끝나 쥔다
    let late = delta("after the end");
    // 정상 종결(message_stop)이 오면 쥔 이벤트까지 순서대로 나가고 스트림은 거기서 끝난다
    let chunks = [events[0].as_str(), events[1].as_str(), STOP, late.as_str()];
    let relayed = relay(&r, &chunks, End::Eof).await;
    assert_eq!(relayed, [events[0].as_str(), events[1].as_str(), STOP]);
    // 종결 이벤트 없이 연결만 끝나면 쥔 앞부분은 나가지 않고 고정 오류로 끝난다
    let refs = events.each_ref().map(String::as_str);
    let relayed = relay(&r, &refs, End::Eof).await;
    assert_eq!(relayed.len(), 2, "{relayed:?}");
    assert_eq!(relayed[0], events[0]);
    assert!(relayed[1].contains("truncated"), "{relayed:?}");
    assert!(!relayed.concat().contains("provider"));
    // 오류·시간 초과로 끝나도 마찬가지다
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
    // 글 값에 든 같은 문자열도 마찬가지다(이 경우는 창이 앞부분이 아니게 돼 평소처럼 나간다)
    let events = [delta("a "), text, STOP.to_owned()];
    let refs = events.each_ref().map(String::as_str);
    assert_eq!(relay(&r, &refs, End::Eof).await, events);
}

#[tokio::test]
async fn short_key_beginnings_pass_immediately_and_longer_ones_wait() {
    let r = rig(|c| c.stream_idle = Duration::from_millis(80));
    // 두 글자(`pr`)는 일반 글에서도 흔해 쥐지 않는다: 스트림이 곧 멎어도 앞의 이벤트는 이미 나갔다
    let events = [delta("hi p"), delta("r")];
    let relayed = relay(&r, &events.each_ref().map(String::as_str), End::Hang).await;
    assert_eq!(relayed.len(), 3, "{relayed:?}");
    assert_eq!(relayed[..2], events);
    // 세 글자(`pro`)부터는 쥔다
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
    // 쥔 것이 없어도, 이벤트가 하나도 없어도, 이벤트 중간에서 끊겨도 같다
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
    // 이름이 다르거나 없거나, 데이터 줄이 둘이면 종결로 보지 않는다: 스트림은 이어지다가 EOF에서 잘린 것으로 끝난다
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
            "{fake}: 종결이 아니므로 뒤 이벤트도 나온다: {joined}"
        );
        assert!(
            relayed.last().unwrap().contains("truncated"),
            "{fake}: {relayed:?}"
        );
    }
    // 이름과 종류가 모두 맞으면 거기서 끝난다
    let relayed = relay(&r, &[STOP, after.as_str()], End::Eof).await;
    assert_eq!(relayed, [STOP]);
}
