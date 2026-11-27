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
    // Ambiguous started step transitions to held with send=false.
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
    // Key ordering is canonicalized.
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
    journal.set_generation("r1", 2);
    assert_eq!(
        say(&old, UPLOAD).await,
        r#"{"v":1,"id":"1","error":"stale"}"#
    );
    let late = r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"late"}"#;
    assert_eq!(say(&old, late).await, r#"{"v":1,"id":"2","error":"stale"}"#);
    assert_eq!(
        journal.states()[0].1,
        EffectState::Started,
        "stale report must not mutate the journal"
    );
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
        // Ambiguous frames (duplicate keys) are untrusted and omit the request id.
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
        "malformed frames must not write to the journal"
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

// ---- Lookup form: `effect.status{step,args}` ----

pub(super) const LOOK: &str = r#"{"v":1,"id":"7","method":"effect.status","step":"upload","args":{"target":"inbox","file":"summary"}}"#;

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
    assert_eq!(journal.step_count(), 0, "lookup must not create a step");
    // Lookup did not allocate an effect ID, so the first request still gets e-1 with send=true.
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
    // Lookup reads started without transitioning to held (only a repeat request transitions to held).
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
        "held reason code is not a destination receipt and must be omitted from lookup"
    );
    // Effect-ID status form still includes the hold reason code.
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
    assert!(say(&c, LOOK).await.contains(r#""state":"started""#));
    let spaced = r#"{ "v":1, "id":"8", "method":"effect.status", "step":"upload", "args":{ "file" : "summary", "target":"inbox" } }"#;
    assert!(say(&c, spaced).await.contains(r#""state":"started""#));
    let different = r#"{"v":1,"id":"9","method":"effect.status","step":"upload","args":{"file":"other","target":"inbox"}}"#;
    assert_eq!(
        say(&c, different).await,
        r#"{"v":1,"id":"9","error":"conflict"}"#
    );
    assert_eq!(
        journal.states(),
        vec![("upload".to_string(), EffectState::Started, None)]
    );
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
    let other = channel_for(&journal, "c2", "agenda.sync");
    assert_eq!(
        say(&other, LOOK).await,
        r#"{"v":1,"id":"7","state":"none","send":false}"#
    );
    // Frame-supplied call/attempt/tool/run fields are ignored in favor of channel bindings.
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
    journal.set_generation("r1", 2);
    assert_eq!(say(&old, LOOK).await, r#"{"v":1,"id":"7","error":"stale"}"#);
    let fresh = channel(&journal, 2);
    assert!(say(&fresh, LOOK).await.contains(r#""state":"started""#));
    // Unreadable journal returns unavailable rather than none.
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
        // Mixed lookup and effect-ID/outcome/receipt/reason fields.
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"effect":"e-1"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"outcome":"confirmed","receipt":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"receipt":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"reason":"x"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"x","step":"upload","args":{}}"#,
        // Incomplete lookup forms.
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","args":{"file":"summary","target":"inbox"}}"#,
        // Invalid step name or args.
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
    // Duplicate keys are ambiguous and omit the request id.
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
        "mixed frames must not mutate the journal"
    );
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"started","send":false}"#
    );
}
