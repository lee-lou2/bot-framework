//! 운영자 판정(`resolution`)이 조회에 어떻게 실리고 소비되는지, 도구가 판정을 주입할 수 없는지 확인한다.
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
    // 판정 전: 보류이고 판정은 없다
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#
    );
    // 운영자가 미실행을 입증했다: 조회가 판정을 싣는다(몇 번을 읽어도 같고 아무것도 소비하지 않는다)
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    for _ in 0..2 {
        assert_eq!(
            say(&c, LOOK).await,
            r#"{"v":1,"id":"7","state":"held","effect":"e-1","resolution":"not_executed","send":false}"#
        );
    }
    // 효과 ID 양식의 기존 응답 모양은 그대로다(판정을 싣지 않는다)
    assert_eq!(
        say(
            &c,
            r#"{"v":1,"id":"5","method":"effect.status","effect":"e-1"}"#
        )
        .await,
        r#"{"v":1,"id":"5","state":"held","receipt":"timeout","send":false}"#
    );
    // 같은 단계로 request하면 한 번만 점유한다(send=true). 그 뒤 판정은 소비돼 조회에 없다
    assert_eq!(
        say(&c, UPLOAD).await,
        r#"{"v":1,"id":"1","state":"started","effect":"e-1","send":true}"#
    );
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"started","effect":"e-1","send":false}"#
    );
    // 다시 request하면 이번 점유의 결과가 불명이라 보류이고 보내지 않는다
    assert!(say(&c, UPLOAD).await.contains(r#""send":false"#));
    // 보류가 아닌 단계에는 판정을 줄 수 없다
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
        // 요청·효과 ID 양식(기록)·조회 양식 어디에도 판정을 실을 수 없다
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
        "판정을 주입하려는 프레임은 원장에 아무것도 남기지 않는다"
    );
    // 조회에도 판정이 없다
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","send":false}"#
    );
    // 도구의 `held` 보고는 판정을 지운다(운영자 판정 뒤에 다시 보류로 보고하면 판정은 소비된 것이다)
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    say(&c, UPLOAD).await; // 다시 점유(send=true)
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
    // 시작만 한 단계: 판정도 영수증도 싣지 않는다
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"started","effect":"e-1","send":false}"#
    );
    journal.lying(false);
    say(&c, r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":"timeout"}"#).await;
    journal.lying(true);
    // 보류 단계: 판정은 원장이 말한 대로 싣고, 보류 사유 코드는 싣지 않는다
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"held","effect":"e-1","resolution":"not_executed","send":false}"#
    );
    journal.lying(false);
    // 확정 단계: 영수증은 싣고 판정은 싣지 않는다
    say(&c, UPLOAD).await; // 보류는 다시 점유되지 않는다(판정 없음)
    assert!(journal.resolve_not_executed("r1", "c1", "upload"));
    say(&c, UPLOAD).await; // 판정으로 한 번 더 점유
    say(&c, r#"{"v":1,"id":"3","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":"file-17"}"#).await;
    journal.lying(true);
    assert_eq!(
        say(&c, LOOK).await,
        r#"{"v":1,"id":"7","state":"confirmed","effect":"e-1","receipt":"file-17","send":false}"#
    );
}
