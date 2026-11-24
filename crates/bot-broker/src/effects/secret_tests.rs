//! 도구가 보고하는 글(영수증·보류 사유·단계 이름)에 알려진 자격이 섞이면 원문을 저장하지 않는다: 확정 보고는 `receipt_credential`, 보류 사유는 `reason_credential`
//! 고정 사유로 **보류**하고 도구에는 `held`와 그 고정 사유만 돌려준다. 단계 이름이 자격이면 프레임이 `invalid`다. 토큰 모양은 자격을 몰라도 늘 막는다.

use super::*;
use crate::testing::MemJournal;
use bot_core::state::EffectState;

/// 짧은 비밀번호(12바이트 미만)도 이 호출이 쓰는 자격이면 막는다.
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

/// 토큰 모양 리터럴이 소스에 남지 않게 접두사를 실행 중에 이어 붙인다.
fn token() -> String {
    format!("{}-{}", "xoxb", "1234567890-abcdefghij")
}

#[tokio::test]
async fn a_receipt_that_carries_a_declared_credential_or_a_token_is_never_stored_and_the_step_is_held()
 {
    for (receipt, what) in [
        (format!("uploaded;pw={PASSWORD};"), "선언한 자격 값"),
        (format!("값은 {} 입니다", token()), "토큰 모양"),
    ] {
        let journal = Arc::new(MemJournal::new());
        let c = guarded(&journal);
        say(&c, UPLOAD).await;
        let reply = say(&c, &confirmed(&receipt)).await;
        // 도구에는 보류와 고정 사유만 돌려준다(원문은 되돌려 주지도 않는다)
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
            "{what}: 원문은 저장하지 않는다"
        );
        // 도구가 나중에 깨끗한 영수증으로 다시 확정하면 그 확정을 받는다(보류는 도구의 다음 보고나 운영자 판정으로 풀린다)
        say(&c, &confirmed("file-17")).await;
        assert_eq!(stored(&journal)[0].1, EffectState::Confirmed, "{what}");
    }
}

#[tokio::test]
async fn a_hold_reason_that_carries_a_credential_is_replaced_by_a_fixed_reason() {
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    // 사유 낱말은 소문자·숫자·밑줄이라 이 모양의 자격만 사유로 들어올 수 있다
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
    // 토큰 모양인 사유도 같다(`ghp_`는 소문자·숫자·밑줄로 이어진다)
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
        "자격과 비슷하기만 한 글은 막지 않는다"
    );
    let journal = Arc::new(MemJournal::new());
    let c = guarded(&journal);
    say(&c, UPLOAD).await;
    assert_eq!(
        say(&c, &held("timeout")).await,
        r#"{"v":1,"id":"2","state":"held","receipt":"timeout","send":false}"#
    );
    // 확정 거절은 영수증이 없어 검사할 글이 없다
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
    // 같은 글이 이름의 일부여도 같다(자격 값이 이름 안에 들어 있다)
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
    // 자격을 모르는 채널: 선언 자격 값은 막지 못하지만 토큰 모양은 막는다
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
