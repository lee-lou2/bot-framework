//! Rate-limit refusals return `Error::RateLimited` directly so an unread refusal never seals a later `Unavailable` storage error at the same position.

use super::{
    cli::{call, record},
    refusal::{judged, word},
    refuse_all::{Inject, Wrapped, one_token, wrapper},
    *,
};
use bot_core::{
    cli::CliRefusal,
    command::{JsonPayload, ToolCall},
    port::Broker,
};

const SAY: &str = r#"{"text":"hi"}"#;

#[tokio::test]
async fn a_limit_refusal_nobody_read_cannot_seal_a_later_storage_failure_at_the_same_position() {
    let wrapped = Arc::new(Wrapped::default());
    let r = rig_full(|_| {}, |_| {}, judged(300), one_token(), wrapper(&wrapped)).await;
    // First position consumes the single rate-limit token.
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let reply = rpc(&r, &call("toolu_a", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    // An earlier request hits the rate limit at position 2, but the client disconnects before the front reads it.
    record(&r, 8, &[("toolu_b", "echo_say", SAY)]);
    let abandoned = wrapped
        .call(ToolCall {
            attempt: r.attempt.clone(),
            call: CallId("cli-8-0".into()),
            tool: ToolId("echo.say".into()),
            args: JsonPayload(SAY.into()),
        })
        .await;
    assert!(matches!(abandoned, Err(bot_core::port::Error::RateLimited)));
    // After the limit recovers, a real storage failure at the same position must not be sealed as a refusal.
    r.clock.advance(Duration::from_secs(60));
    wrapped.fail_with(Some(Inject::Unavailable));
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert!(
        r.turns.refusals().is_empty(),
        "sealed a storage failure as a rate-limit refusal: {:?}",
        r.turns.refusals()
    );
    assert_eq!(r.runner.jobs().len(), 1);
    // Once storage recovers, retrying the same position executes it.
    wrapped.fail_with(None);
    let reply = rpc(&r, &call("toolu_b", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 2);
    // A real rate-limit refusal seals the position with `CliRefusal::RateLimited`.
    record(&r, 12, &[("toolu_c", "echo_say", SAY)]);
    assert_eq!(word(&r, "toolu_c", "echo_say", SAY).await, "unavailable");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-12-0".into()), CliRefusal::RateLimited)]
    );
    assert_eq!(
        r.runner.jobs().len(),
        2,
        "rate-limited call must not execute"
    );
}
