//! Delegation refusals are sealed before returning; transient store errors are not.

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
    // Store outage is transient and must not be sealed as a refusal.
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
        "only deterministic refusals leave markers"
    );
    assert!(
        r.park.seen().is_empty(),
        "must not park when delegation did not start"
    );
    // Once recovered, sealed positions replay their word without spawning children.
    r.delegation.fail_with(None);
    for (id, word) in [("toolu_d", "denied"), ("toolu_e", "conflict")] {
        assert!(
            rpc(&r, &delegate(id, input)).await.body.contains(word),
            "{id}"
        );
    }
    assert!(r.delegation.commands().is_empty());
    assert_eq!(
        r.turns.refusals().len(),
        2,
        "markers are written once per position"
    );
    // Unsealed positions are re-evaluated and proceed to spawn and park.
    assert!(
        rpc(&r, &delegate("toolu_f", input))
            .await
            .body
            .contains("child_pending")
    );
    assert_eq!(r.delegation.commands().len(), 1);
    assert_eq!(r.park.seen().len(), 1);
}
