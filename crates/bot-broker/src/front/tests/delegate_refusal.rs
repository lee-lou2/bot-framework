//! 위임(`delegate`)의 거절도 같은 규칙이다: 저장소가 거절한 호출은 모델이 본 단어를 표식으로 먼저 남기고 같은 위치는 그 단어를 재생한다(자식을 만들지 않는다).
//! 저장소 장애는 거절이 아니라 봉인하지 않는다.

use super::delegate::{delegate, plan, with_family};
use super::*;
use bot_core::cli::CliRefusal;

#[tokio::test]
async fn store_refusals_become_single_words_and_never_park_and_a_confirmed_one_is_replayed_for_the_same_position()
 {
    let r = with_family().await;
    let input = r#"{"agent":"main/research","instruction":"go"}"#;
    for (turn, id) in [(4, "toolu_d"), (5, "toolu_e"), (6, "toolu_f")] {
        plan(&r, turn, id, input);
    }
    r.delegation.fail_with(Some(bot_core::port::Error::Denied));
    assert!(
        rpc(&r, &delegate("toolu_d", input))
            .await
            .body
            .contains("denied")
    );
    r.delegation
        .fail_with(Some(bot_core::port::Error::Conflict));
    assert!(
        rpc(&r, &delegate("toolu_e", input))
            .await
            .body
            .contains("conflict")
    );
    // 저장소가 응답하지 못한 것은 거절이 아니다: 표식 없이 `unavailable`이다
    r.delegation
        .fail_with(Some(bot_core::port::Error::Unavailable));
    assert!(
        rpc(&r, &delegate("toolu_f", input))
            .await
            .body
            .contains("unavailable")
    );
    assert_eq!(
        r.turns.refusals(),
        vec![
            (CallId("cli-4-0".into()), CliRefusal::Denied),
            (CallId("cli-5-0".into()), CliRefusal::Conflict),
        ],
        "확정한 두 거절만 표식이 남는다"
    );
    assert!(
        r.park.seen().is_empty(),
        "시작하지 못했으면 대기에 들어가지 않는다"
    );
    // 저장소가 돌아와도 확정한 위치는 모델이 본 단어를 그대로 재생하고 자식을 만들지 않는다
    r.delegation.fail_with(None);
    for (id, word) in [("toolu_d", "denied"), ("toolu_e", "conflict")] {
        assert!(
            rpc(&r, &delegate(id, input)).await.body.contains(word),
            "{id}"
        );
    }
    assert!(r.delegation.commands().is_empty());
    assert_eq!(r.turns.refusals().len(), 2, "표식은 위치마다 한 번이다");
    // 표식이 없던 위치는 다시 판정해 자식을 시작하고 부모는 대기로 들어간다
    assert!(
        rpc(&r, &delegate("toolu_f", input))
            .await
            .body
            .contains("child_pending")
    );
    assert_eq!(r.delegation.commands().len(), 1);
    assert_eq!(r.park.seen().len(), 1);
}
