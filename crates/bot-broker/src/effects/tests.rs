use super::*;
use crate::testing::MemJournal;
use tokio::io::{AsyncBufReadExt, duplex};

pub(super) fn channel(journal: &Arc<MemJournal>, generation: u64) -> EffectChannel {
    EffectChannel::new(
        journal.clone(),
        Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(generation),
        },
        CallId("c1".into()),
        ToolId("agenda.sync".into()),
    )
}

pub(super) async fn say(channel: &EffectChannel, frame: &str) -> String {
    channel.handle(frame.as_bytes()).await
}

pub(super) const UPLOAD: &str = r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary","target":"inbox"}}"#;

#[tokio::test]
async fn the_first_request_may_send_and_a_repeat_never_may() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#
    );
    // 전송 여부가 불명인 started는 held, send=false
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"held","effect":"e-1","receipt":"response_unknown","send":false}"#
    );
    assert_eq!(
        journal.states(),
        vec![(
            "upload".to_string(),
            EffectState::Held,
            Some("response_unknown".into())
        )]
    );
}

#[tokio::test]
async fn a_confirmed_step_returns_its_receipt_and_is_never_sent_again() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    let done = say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#).await;
    assert_eq!(
        done,
        r#"{"v":1,"id":"2","state":"confirmed","receipt":"file-17","send":false}"#
    );
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"confirmed","effect":"e-1","receipt":"file-17","send":false}"#
    );
    // 같은 영수증을 다시 보고해도 결과는 같다. 다른 영수증은 충돌이다.
    let again = r#"{"v":1,"id":"3","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#;
    assert!(say(&c, again).await.contains(r#""state":"confirmed""#));
    let other = r#"{"v":1,"id":"4","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-99"}"#;
    assert_eq!(
        say(&c, other).await,
        r#"{"v":1,"id":"4","error":"conflict"}"#
    );
}

#[tokio::test]
async fn the_same_step_with_other_arguments_conflicts() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    let other = r#"{"v":1,"id":"9","method":"effect.request","step":"upload","args":{"file":"other","target":"inbox"}}"#;
    assert_eq!(
        say(&c, other).await,
        r#"{"v":1,"id":"9","error":"conflict"}"#
    );
    // 인자 글의 표기나 키 순서가 달라도 같은 인자다
    let same = r#"{"v":1,"id":"8","method":"effect.request","step":"upload","args":{"target":"inbox","file":"summary"}}"#;
    assert!(say(&c, same).await.contains(r#""state":"held""#));
}

#[tokio::test]
async fn status_without_an_outcome_is_a_lookup() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"started","send":false}"#
    );
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"6","method":"effect.status","effect":"e-404"}"#
        )
        .await,
        r#"{"v":1,"id":"6","error":"invalid"}"#
    );
}

#[tokio::test]
async fn rejected_and_held_are_recorded_with_their_reason() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    let held = say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    assert_eq!(
        held,
        r#"{"v":1,"id":"2","state":"held","receipt":"timeout","send":false}"#
    );
    let no_reason = say(
        &c,
        r#"{"v":1,"id":"3","method":"effect.status","effect":"e-1","outcome":"held"}"#,
    )
    .await;
    assert_eq!(no_reason, r#"{"v":1,"id":"3","error":"invalid"}"#);
    let rejected = say(
        &c,
        r#"{"v":1,"id":"4","method":"effect.status","effect":"e-1","outcome":"rejected"}"#,
    )
    .await;
    assert_eq!(
        rejected,
        r#"{"v":1,"id":"4","state":"rejected","send":false}"#
    );
}

#[tokio::test]
async fn a_late_generation_cannot_touch_the_journal() {
    let journal = Arc::new(MemJournal::new());
    journal.set_generation("r1", 1);
    let old = channel(&journal, 1);
    say(&old, UPLOAD).await;
    journal.set_generation("r1", 2); // 워커 인계
    assert_eq!(
        say(&old, UPLOAD).await,
        r#"{"v":1,"id":"1","error":"stale"}"#
    );
    let late = r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"late"}"#;
    assert_eq!(say(&old, late).await, r#"{"v":1,"id":"2","error":"stale"}"#);
    assert_eq!(
        journal.states()[0].1,
        EffectState::Started,
        "늦은 보고는 원장을 바꾸지 못한다"
    );
    // 새 세대는 같은 호출·같은 단계로 이어 간다
    let fresh = channel(&journal, 2);
    assert!(say(&fresh, UPLOAD).await.contains(r#""state":"held""#));
}

#[tokio::test]
async fn another_call_cannot_update_this_calls_steps() {
    let journal = Arc::new(MemJournal::new());
    let mine = channel(&journal, 1);
    say(&mine, UPLOAD).await;
    let other = EffectChannel::new(
        journal.clone(),
        Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        CallId("c2".into()),
        ToolId("agenda.sync".into()),
    );
    let steal = r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"x"}"#;
    assert_eq!(
        say(&other, steal).await,
        r#"{"v":1,"id":"2","error":"denied"}"#
    );
}

#[tokio::test]
async fn malformed_frames_get_a_classification_only() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    let cases: [(&str, &str); 12] = [
        ("not json", r#"{"v":1,"error":"invalid"}"#),
        (r#"[1]"#, r#"{"v":1,"error":"invalid"}"#),
        (
            r#"{"v":1,"method":"effect.request"}"#,
            r#"{"v":1,"error":"invalid"}"#,
        ),
        (
            r#"{"v":2,"id":"1","method":"effect.request"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.unknown"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.request","step":"Upload","args":{}}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.request","step":"../x","args":{}}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.request","step":"s","args":[1]}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.request","step":"s","args":{"n":9007199254740993}}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        // 모호한 프레임은 어느 부분도 믿지 않는다(번호도 싣지 않는다)
        (
            r#"{"v":1,"id":"1","method":"effect.request","step":"s","args":{"a":1,"a":2}}"#,
            r#"{"v":1,"error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","outcome":"confirmed"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
        (
            r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","outcome":"maybe"}"#,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
        ),
    ];
    for (frame, expected) in cases {
        assert_eq!(say(&c, frame).await, expected, "{frame}");
    }
    assert!(
        journal.states().is_empty(),
        "잘못된 프레임은 원장에 아무것도 남기지 않는다"
    );
}

#[tokio::test]
async fn the_serve_loop_answers_line_by_line_and_stops_on_signal_or_eof() {
    let journal = Arc::new(MemJournal::new());
    let (mut tool_write, broker_read) = duplex(1 << 16);
    let (broker_write, tool_read) = duplex(1 << 16);
    let (stop, signal) = watch::channel(false);
    let task = tokio::spawn(channel(&journal, 1).serve(
        Box::new(broker_read),
        Box::new(broker_write),
        signal,
    ));
    tool_write
        .write_all(format!("{UPLOAD}\n").as_bytes())
        .await
        .unwrap();
    let mut replies = BufReader::new(tool_read);
    let mut line = String::new();
    replies.read_line(&mut line).await.unwrap();
    assert!(line.contains(r#""send":true"#), "{line}");
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn an_oversized_frame_closes_the_channel() {
    let journal = Arc::new(MemJournal::new());
    let (mut tool_write, broker_read) = duplex(1 << 20);
    let (broker_write, tool_read) = duplex(1 << 16);
    let (_stop, signal) = watch::channel(false);
    let task = tokio::spawn(channel(&journal, 1).serve(
        Box::new(broker_read),
        Box::new(broker_write),
        signal,
    ));
    let big = format!(
        "{{\"v\":1,\"id\":\"1\",\"method\":\"effect.request\",\"step\":\"s\",\"args\":{{\"x\":\"{}\"}}}}\n",
        "a".repeat(MAX_FRAME)
    );
    tool_write.write_all(big.as_bytes()).await.unwrap();
    let mut replies = BufReader::new(tool_read);
    let mut line = String::new();
    replies.read_line(&mut line).await.unwrap();
    assert_eq!(line.trim(), r#"{"v":1,"error":"invalid"}"#);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(journal.states().is_empty());
}

// ---- 조회 양식: `effect.status{step,args}` ----

pub(super) const LOOK: &str = r#"{"v":1,"id":"7","method":"effect.status","step":"upload","args":{"target":"inbox","file":"summary"}}"#;

/// 같은 run·도구의 다른 호출 채널.
fn channel_for(journal: &Arc<MemJournal>, call: &str, tool: &str) -> EffectChannel {
    EffectChannel::new(
        journal.clone(),
        Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        CallId(call.into()),
        ToolId(tool.into()),
    )
}

#[tokio::test]
async fn a_lookup_of_a_step_that_was_never_requested_says_none_and_writes_nothing() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"none","send":false}"#
    );
    assert_eq!(journal.step_count(), 0, "조회는 단계를 만들지 않는다");
    // 조회가 효과 ID를 쓰지 않았다: 첫 요청이 그대로 첫 효과이고 전송을 허가한다
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#
    );
}

#[tokio::test]
async fn a_lookup_reads_each_state_without_changing_it_or_permitting_a_send() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    // 시작만 한 단계: 조회는 `started`로 읽고 보류로 바꾸지 않는다(같은 단계를 다시 `request`하는 것만 보류로 바꾼다)
    for _ in 0..3 {
        assert_eq!(
            say(&c, LOOK).await,
            r#"{"v":1,"id":"7","state":"started","effect":"e-1","send":false}"#
        );
    }
    assert_eq!(
        journal.states(),
        vec![("upload".to_string(), EffectState::Started, None)]
    );
    // 확정·거절·보류와 영수증
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#).await;
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"confirmed","effect":"e-1","receipt":"file-17","send":false}"#
    );
    let other = Arc::new(MemJournal::new());
    let c = channel(&other, 1);
    say(&c, UPLOAD).await;
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#,
        "수신처 영수증이 아닌 보류 사유 코드는 조회 응답에 싣지 않는다"
    );
    // 효과 ID 양식의 기존 응답은 그대로 사유 코드를 싣는다
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"held","receipt":"timeout","send":false}"#
    );
    let third = Arc::new(MemJournal::new());
    let c = channel(&third, 1);
    say(&c, UPLOAD).await;
    say(
        &c,
        r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"rejected"}"#,
    )
    .await;
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"rejected","effect":"e-1","send":false}"#
    );
}

#[tokio::test]
async fn a_lookup_uses_the_same_canonical_arguments_as_the_request_and_a_different_fingerprint_conflicts()
 {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    // 키 순서·공백이 달라도 같은 인자다(LOOK은 키 순서가 UPLOAD와 다르다)
    assert!(say(&c, LOOK).await.contains(r#""state":"started""#));
    let spaced = r#"{ "v":1, "id":"8", "method":"effect.status", "step":"upload", "args":{ "file" : "summary", "target":"inbox" } }"#;
    assert!(say(&c, spaced).await.contains(r#""state":"started""#));
    // 인자가 다르면 충돌이고 원장은 그대로다
    let different = r#"{"v":1,"id":"9","method":"effect.status","step":"upload","args":{"file":"other","target":"inbox"}}"#;
    assert_eq!(
        say(&c, different).await,
        r#"{"v":1,"id":"9","error":"conflict"}"#
    );
    assert_eq!(
        journal.states(),
        vec![("upload".to_string(), EffectState::Started, None)]
    );
    // 다른 단계 이름은 없는 단계다
    let step = r#"{"v":1,"id":"6","method":"effect.status","step":"notify","args":{"file":"summary","target":"inbox"}}"#;
    assert_eq!(
        say(&c, step).await,
        r#"{"v":1,"id":"6","state":"none","send":false}"#
    );
}

#[tokio::test]
async fn a_lookup_only_sees_the_call_its_channel_was_opened_for_and_never_takes_identifiers_from_the_frame()
 {
    let journal = Arc::new(MemJournal::new());
    let mine = channel_for(&journal, "c1", "agenda.sync");
    say(&mine, UPLOAD).await;
    // 다른 호출의 채널은 같은 단계 이름·인자로도 이 호출의 단계를 보지 못한다
    let other = channel_for(&journal, "c2", "agenda.sync");
    assert_eq!(
        say(&other, LOOK).await,
        r#"{"v":1,"id":"7","state":"none","send":false}"#
    );
    // 프레임이 호출·시도·도구·run을 지정해도 읽지 않는다(채널이 정한 값만 쓴다)
    let forged = r#"{"v":1,"id":"5","method":"effect.status","step":"upload","args":{"target":"inbox","file":"summary"},"call":"c1","run":"r1","attempt":{"generation":1},"tool":"agenda.sync"}"#;
    assert_eq!(
        say(&other, forged).await,
        r#"{"v":1,"id":"5","state":"none","send":false}"#
    );
    assert_eq!(journal.step_count(), 1);
}

#[tokio::test]
async fn a_lookup_checks_the_tool_the_channel_was_opened_for() {
    let journal = Arc::new(MemJournal::new());
    journal.bind_call("r1", "c1", "agenda.sync");
    let right = channel_for(&journal, "c1", "agenda.sync");
    say(&right, UPLOAD).await;
    assert!(say(&right, LOOK).await.contains(r#""state":"started""#));
    // 호출 ID는 같아도 도구가 다른 채널은 원장이 거절한다
    let wrong = channel_for(&journal, "c1", "agenda.delete");
    assert_eq!(
        say(&wrong, LOOK).await,
        r#"{"v":1,"id":"7","error":"denied"}"#
    );
}

#[tokio::test]
async fn a_lookup_obeys_the_generation_fence_and_reports_an_unreadable_journal() {
    let journal = Arc::new(MemJournal::new());
    journal.set_generation("r1", 1);
    let old = channel(&journal, 1);
    say(&old, UPLOAD).await;
    journal.set_generation("r1", 2); // 워커 인계
    assert_eq!(say(&old, LOOK).await, r#"{"v":1,"id":"7","error":"stale"}"#);
    // 새 세대는 같은 단계를 읽는다
    let fresh = channel(&journal, 2);
    assert!(say(&fresh, LOOK).await.contains(r#""state":"started""#));
    // 원장을 읽지 못하면 없다고 하지 않고 읽지 못했다고 한다
    journal.lookup_down(true);
    assert_eq!(
        say(&fresh, LOOK).await,
        r#"{"v":1,"id":"7","error":"unavailable"}"#
    );
}

#[tokio::test]
async fn mixing_the_two_status_forms_or_leaving_one_half_out_is_invalid_and_changes_nothing() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    let before = journal.states();
    let cases = [
        // 조회 양식에 효과 ID·결과·영수증·사유를 섞었다: 어느 쪽도 믿지 않는다
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"effect":"e-1"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"outcome":"confirmed","receipt":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"receipt":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"reason":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"x","step":"upload","args":{}}"#,
        // 반쪽 양식
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","args":{"file":"summary","target":"inbox"}}"#,
        // 잘못된 단계 이름·인자
        r#"{"v":1,"id":"1","method":"effect.status","step":"Upload","args":{}}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":[1]}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"n":9007199254740993}}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"","args":{}}"#,
    ];
    for frame in cases {
        assert_eq!(
            say(&c, frame).await,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
            "{frame}"
        );
    }
    // 모호한 프레임(같은 키 두 번)은 번호도 싣지 않는다
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"1","method":"effect.status","step":"upload","step":"x","args":{}}"#
        )
        .await,
        r#"{"v":1,"error":"invalid"}"#
    );
    assert_eq!(
        journal.states(),
        before,
        "섞인 프레임은 원장에 아무것도 남기지 않는다"
    );
    // 기존 효과 ID 양식은 그대로다
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"started","send":false}"#
    );
}
