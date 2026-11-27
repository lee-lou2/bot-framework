//! Credential-leak tests for effect channel receipts, hold reasons, and step names.

use super::*;
use crate::testing::MemJournal;
use bot_core::state::EffectState;

/// Short passwords (<12 bytes) are still blocked when declared for the call.
const PASSWORD: &str = "hunter2xyz";

fn guarded(journal: &Arc<MemJournal>) -> EffectChannel {
    EffectChannel::new(
        journal.clone(),
        Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        CallId("c1".into()),
        ToolId("agenda.sync".into()),
    )
    .with_credentials(vec![PASSWORD.as_bytes().to_vec()])
}

async fn say(channel: &EffectChannel, frame: &str) -> String {
    channel.handle(frame.as_bytes()).await
}

const UPLOAD: &str =
    r#"{"v":1,"id":"1","method":"effect.request","step":"upload","args":{"file":"summary"}}"#;

fn confirmed(receipt: &str) -> String {
    format!(
        r#"{{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"confirmed","receipt":{}}}"#,
        json_quote(receipt)
    )
}

fn held(reason: &str) -> String {
    format!(
        r#"{{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"held","reason":{}}}"#,
        json_quote(reason)
    )
}

fn stored(journal: &MemJournal) -> Vec<(String, EffectState, Option<String>)> {
    journal.states()
}

/// Constructed at runtime to avoid storing a token-shaped literal in source.
fn token() -> String {
    format!("{}-{}", "xoxb", "1234567890-abcdefghij")
}

#[tokio::test]
async fn a_receipt_that_carries_a_declared_credential_or_a_token_is_never_stored_and_the_step_is_held()
 {
    for (receipt, what) in [
        (format!("uploaded;pw={PASSWORD};"), "declared credential"),
        (format!("value is {}", token()), "token shape"),
    ] {
        let journal = Arc::new(MemJournal::new());
        let c = guarded(&journal);
        say(&c, UPLOAD).await;
        let reply = say(&c, &confirmed(&receipt)).await;
        assert_eq!(
            reply, r#"{"v":1,"id":"2","state":"held","receipt":"receipt_credential","send":false}"#,
            "{what}"
        );
        assert!(
            !reply.contains(PASSWORD) && !reply.contains("xoxb"),
            "{what}"
        );
        assert_eq!(
            stored(&journal),
            vec![(
                "upload".to_string(),
                EffectState::Held,
                Some("receipt_credential".to_string())
            )],
            "{what}: raw receipt must not be stored"
        );
        // A subsequent clean confirmation clears the hold.
        say(&c, &confirmed("file-17")).await;
        assert_eq!(stored(&journal)[0].1, EffectState::Confirmed, "{what}");
    }
}

#[tokio::test]
async fn a_hold_reason_that_carries_a_credential_is_replaced_by_a_fixed_reason() {
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    let reply = say(&c, &held(PASSWORD)).await;
    assert_eq!(
        reply,
        r#"{"v":1,"id":"2","state":"held","receipt":"reason_credential","send":false}"#
    );
    assert_eq!(
        stored(&journal),
        vec![(
            "upload".to_string(),
            EffectState::Held,
            Some("reason_credential".to_string())
        )]
    );
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    let shaped = format!("{}_{}", "ghp", "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6");
    let reply = say(&c, &held(&shaped)).await;
    assert!(
        reply.contains("reason_credential") && !reply.contains("ghp"),
        "{reply}"
    );
    assert!(!stored(&journal)[0].2.clone().unwrap().contains("ghp"));
}

#[tokio::test]
async fn clean_reports_are_stored_as_they_are() {
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    assert_eq!(
        say(&c, &confirmed("file-17 (hunter) ok")).await,
        r#"{"v":1,"id":"2","state":"confirmed","receipt":"file-17 (hunter) ok","send":false}"#,
        "substring similar to credential must not be blocked"
    );
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    assert_eq!(
        say(&c, &held("timeout")).await,
        r#"{"v":1,"id":"2","state":"held","receipt":"timeout","send":false}"#
    );
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    say(
        &c,
        r#"{"v":1,"id":"2","method":"effect.status","effect":"e-1","outcome":"rejected"}"#,
    )
    .await;
    assert_eq!(stored(&journal)[0].1, EffectState::Rejected);
}

#[tokio::test]
async fn a_step_name_that_is_a_credential_is_invalid_and_nothing_is_recorded_or_read() {
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    let request =
        format!(r#"{{"v":1,"id":"1","method":"effect.request","step":"{PASSWORD}","args":{{}}}}"#);
    assert_eq!(
        say(&c, &request).await,
        r#"{"v":1,"id":"1","error":"invalid"}"#
    );
    let look =
        format!(r#"{{"v":1,"id":"3","method":"effect.status","step":"{PASSWORD}","args":{{}}}}"#);
    assert_eq!(
        say(&c, &look).await,
        r#"{"v":1,"id":"3","error":"invalid"}"#
    );
    assert_eq!(journal.step_count(), 0);
    let embedded = format!(
        r#"{{"v":1,"id":"1","method":"effect.request","step":"up-{PASSWORD}-1","args":{{}}}}"#
    );
    assert_eq!(
        say(&c, &embedded).await,
        r#"{"v":1,"id":"1","error":"invalid"}"#
    );
    assert_eq!(journal.step_count(), 0);
}

#[tokio::test]
async fn a_channel_without_declared_credentials_still_blocks_token_shapes_and_nothing_else() {
    let journal = Arc::new(MemJournal::new());
    let c = super::tests::channel(&journal, 1);
    say(&c, UPLOAD).await;
    assert_eq!(
        say(&c, &confirmed(&format!("pw={PASSWORD}"))).await,
        format!(r#"{{"v":1,"id":"2","state":"confirmed","receipt":"pw={PASSWORD}","send":false}}"#)
    );
    let journal = Arc::new(MemJournal::new());
    let c = super::tests::channel(&journal, 1);
    say(&c, UPLOAD).await;
    assert!(
        say(&c, &confirmed(&token()))
            .await
            .contains("receipt_credential")
    );
}
