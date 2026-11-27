//! Operator resolution visibility, single-use consumption, and tool-injection rejection.
use super::tests::{LOOK, UPLOAD, channel, say};
use super::*;
use crate::testing::MemJournal;

#[tokio::test]
async fn an_operator_verdict_of_not_executed_is_shown_by_the_lookup_and_the_next_request_takes_the_step_once()
 {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#
    );
    // Operator proves non-execution; lookup surfaces the verdict without consuming it.
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    for _ in 0..2 {
        assert_eq!(
            say(&c, LOOK).await,
            r#"{"v":1,"id":"7","state":"held","effect":"e-1","resolution":"not_executed","send":false}"#
        );
    }
    // Lookup by effect ID keeps its legacy shape and omits resolution.
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"held","receipt":"timeout","send":false}"#
    );
    // Re-requesting the step claims it once (send=true) and consumes the verdict.
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#
    );
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"started","effect":"e-1","send":false}"#
    );
    assert!(say(&c, UPLOAD).await.contains(r#""send":false"#));
    assert!(!journal.resolve_not_executed("r1", "c1", "missing"));
}

#[tokio::test]
async fn a_tool_can_never_set_the_resolution_in_any_frame() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    let before = journal.states();
    for frame in [
        // Reject resolution injection across request, record, and lookup frames.
        r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary","target":"inbox"},"resolution":"not_executed"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","outcome":"held","reason":"x","resolution":"not_executed"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","effect":"e-1","resolution":"not_executed"}"#,
        r#"{"v":1,"id":"1","method":"effect.status","step":"upload","args":{"file":"summary","target":"inbox"},"resolution":"not_executed"}"#,
        r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary","target":"inbox"},"resolution":null}"#,
    ] {
        assert_eq!(
            say(&c, frame).await,
            r#"{"v":1,"id":"1","error":"invalid"}"#,
            "{frame}"
        );
    }
    assert_eq!(
        journal.states(),
        before,
        "frames attempting to inject resolution must not mutate the journal"
    );
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#
    );
    // A subsequent held report clears any prior resolution.
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    say(&c, UPLOAD).await;
    say(&c, r#"{"v":1,"id":"3","method":"effect.status","effect":"e-1","outcome":"held","reason":"again"}"#).await;
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#
    );
}

#[tokio::test]
async fn a_lookup_carries_only_what_the_rules_allow_even_if_the_journal_says_more() {
    let journal = Arc::new(MemJournal::new());
    let c = channel(&journal, 1);
    say(&c, UPLOAD).await;
    journal.lying(true);
    // Started step: omit both resolution and receipt.
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"started","effect":"e-1","send":false}"#
    );
    journal.lying(false);
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    journal.lying(true);
    // Held step: forward journal resolution, omit hold reason code.
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","resolution":"not_executed","send":false}"#
    );
    journal.lying(false);
    // Confirmed step: forward receipt, omit resolution.
    say(&c, UPLOAD).await;
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    say(&c, UPLOAD).await;
    say(&c, r#"{"v":1,"id":"3","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#).await;
    journal.lying(true);
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"confirmed","effect":"e-1","receipt":"file-17","send":false}"#
    );
}
