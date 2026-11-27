//! Post-intake recovery for private calls: pre-start lease release, not-sent reclaim, unknown holds, and rate limiting.

use super::*;
use crate::{keyed, testing::*};
use bot_core::{id::ClaimEpoch, state::Risk};

fn fingerprint_of(call: &PrivateCall, canonical: &str) -> [u8; 32] {
    keyed::hmac(
        KEY,
        &[
            call.key.profile.0.as_bytes(),
            call.key.binding.0.as_bytes(),
            call.key.principal.0.as_bytes(),
            call.key.request.0.as_bytes(),
            call.agent.0.as_bytes(),
            call.tool.0.as_bytes(),
            call.conversation.id.as_bytes(),
            call.conversation.thread.as_deref().unwrap_or("").as_bytes(),
            &[u8::from(call.conversation.thread.is_some())],
            canonical.as_bytes(),
        ],
    )
}

#[tokio::test]
async fn a_delivery_made_while_the_first_is_still_in_flight_is_not_acknowledged() {
    let r = rig(true);
    // First delivery holds a lease but never started (simulates crashed worker).
    use bot_core::ledger::{PrivateLedger, PrivateRequest};
    let first = standard("req-1");
    let sealed = FakeSealer
        .seal(&tool(Risk::Write, &["inbox.write"]), &first.args)
        .unwrap();
    let fingerprint = fingerprint_of(&first, &sealed.canonical);
    let permit = r
        .ledger
        .begin(PrivateRequest {
            key: first.key.clone(),
            fingerprint,
            pass_fingerprint: None,
            revision: Revision("r1".into()),
            agent: AgentId("main".into()),
            tool: ToolId("inbox.receive".into()),
            conversation: first.conversation.clone(),
            scopes: ["inbox.write".to_owned()].into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        permit,
        bot_core::ledger::PrivatePermit::Start { .. }
    ));
    let again = receive(&r, standard("req-1")).await;
    assert!(
        matches!(again, Err(Error::Unavailable)),
        "in-flight intake must trigger retry rather than ACK"
    );
    assert!(
        r.runner.jobs().is_empty(),
        "must not ACK with zero executions"
    );
}

#[tokio::test]
async fn a_failure_before_the_start_record_gives_the_lease_back_and_the_redelivery_runs_once() {
    let r = rig(true);
    // Active revision flips before start commit: lease is released without invoking the runner.
    let ledger = r.ledger.clone();
    r.ledger
        .set_before_start(move || ledger.set_active_revision("r2"));
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Stale)
    ));
    assert_eq!(r.ledger.outcomes(), ["retry_before_start"]);
    assert!(
        r.runner.jobs().is_empty(),
        "runner must not be called on pre-start failure"
    );
    assert!(r.ledger.starts().is_empty());
    // Publishing the new revision lets the redelivery proceed under a new epoch and execute once.
    r.catalog.publish(world("r2"));
    let mut again = standard("req-1");
    again.revision = Revision("r2".into());
    receive(&r, again).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1, "executed once after redelivery");
    assert_eq!(r.ledger.outcomes(), ["retry_before_start", "completed"]);
}

#[tokio::test]
async fn a_runner_that_confirms_nothing_was_sent_releases_the_claim_for_a_redelivery() {
    let r = rig(true);
    r.runner.script(crate::ports::Ran::NotSent);
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(r.ledger.outcomes(), ["not_sent"]);
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.ledger.outcomes(), ["not_sent", "completed"]);
    let claims: Vec<_> = r.runner.jobs().iter().map(|j| j.attempt.claim).collect();
    assert_eq!(claims, [Some(ClaimEpoch(1)), Some(ClaimEpoch(2))]);
}

#[tokio::test]
async fn an_unknown_result_is_held_for_good_and_never_rerun() {
    let r = rig(true);
    r.runner
        .script(crate::ports::Ran::Unknown("response_unknown"));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held"]);
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.runner.jobs().len(),
        1,
        "held intake must not be re-executed"
    );
    let r = rig(true);
    r.runner.fail_next();
    receive(&r, standard("req-2")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held"]);
}

#[tokio::test]
async fn a_started_claim_that_loses_its_lease_becomes_a_durable_hold_without_a_redelivery() {
    let r = rig(true);
    use bot_core::ledger::{PrivateLedger, PrivateRequest, PrivateStart};
    let call = standard("req-1");
    let sealed = FakeSealer
        .seal(&tool(Risk::Write, &["inbox.write"]), &call.args)
        .unwrap();
    let fingerprint = fingerprint_of(&call, &sealed.canonical);
    let permit = r
        .ledger
        .begin(PrivateRequest {
            key: call.key.clone(),
            fingerprint,
            pass_fingerprint: None,
            revision: Revision("r1".into()),
            agent: AgentId("main".into()),
            tool: ToolId("inbox.receive".into()),
            conversation: call.conversation.clone(),
            scopes: ["inbox.write".to_owned()].into(),
        })
        .await
        .unwrap();
    let bot_core::ledger::PrivatePermit::Start { attempt, .. } = permit else {
        panic!()
    };
    r.ledger
        .start(PrivateStart {
            attempt,
            tool: ToolId("inbox.receive".into()),
            fingerprint,
            checked_revision: Revision("r1".into()),
        })
        .await
        .unwrap();
    // Started worker vanished: looks in-flight until expiry sweeps it to held.
    assert!(matches!(
        receive(&r, standard("req-1")).await,
        Err(Error::Unavailable)
    ));
    r.ledger.expire_started();
    receive(&r, standard("req-1")).await.unwrap();
    assert!(
        r.runner.jobs().is_empty(),
        "started intake must never be re-executed"
    );
}

#[tokio::test]
async fn a_blocked_result_is_held_because_the_effect_is_unknown_and_never_rerun() {
    let r = rig(true);
    r.runner
        .script(crate::ports::Ran::Blocked("result_credential"));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.ledger.outcomes(),
        ["held"],
        "tool already executed so blocked output must be held, not rejected"
    );
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn private_calls_are_rate_limited_and_a_refused_one_gives_the_lease_back() {
    let clock = Arc::new(ManualClock::new(1_000));
    let r = rig(true);
    let gate = PrivateGate::new(
        PrivateParts {
            catalog: r.catalog.clone(),
            ledger: r.ledger.clone(),
            secrets: Arc::new(FakeSecrets::new(&[(
                "personal",
                FINGERPRINT_SECRET,
                "profile-intake-key-bytes",
            )])),
            sealer: Arc::new(FakeSealer),
            runner: r.runner.clone(),
            tracer: r.tracer.clone(),
            clock: clock.clone(),
        },
        crate::rate::RateConfig {
            per_minute: 60,
            burst: 1,
            concurrent: 4,
        },
    );
    gate.receive(standard("req-1")).await.unwrap();
    r.ledger.set_attempt(Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("private-run-2".into()),
        generation: Generation(1),
        claim: Some(ClaimEpoch(1)),
    });
    assert!(matches!(
        gate.receive(standard("req-2")).await,
        Err(Error::Unavailable)
    ));
    assert_eq!(r.ledger.outcomes(), ["completed", "retry_before_start"]);
    clock.advance(std::time::Duration::from_secs(2));
    gate.receive(standard("req-2")).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn the_reply_destination_is_bound_to_the_first_request_including_the_thread() {
    let r = rig(true);
    let mut first = standard("req-1");
    first.conversation.thread = Some("t-1".into());
    r.runner.script(crate::ports::Ran::NotSent);
    assert!(receive(&r, first).await.is_err());
    // Changing thread or conversation ID for the same key and args must conflict.
    for (thread, conversation) in [(Some("t-2"), "dm-1"), (None, "dm-1"), (Some("t-1"), "dm-2")] {
        let mut moved = standard("req-1");
        moved.conversation.thread = thread.map(str::to_owned);
        moved.conversation.id = conversation.into();
        assert!(
            matches!(receive(&r, moved).await, Err(Error::Conflict)),
            "{thread:?}/{conversation}"
        );
    }
    assert_eq!(r.runner.jobs().len(), 1);
    let mut same = standard("req-1");
    same.conversation.thread = Some("t-1".into());
    receive(&r, same).await.unwrap();
    assert_eq!(r.runner.jobs().len(), 2);
}

#[tokio::test]
async fn a_tool_that_reports_an_error_is_held_and_a_definite_refusal_is_rejected() {
    let fresh_run = |r: &Rig, n: u32| {
        r.ledger.set_attempt(Attempt {
            profile: ProfileId("personal".into()),
            run: RunId(format!("private-run-{n}")),
            generation: Generation(1),
            claim: Some(ClaimEpoch(1)),
        })
    };
    use crate::ports::Ran;
    use bot_core::command::JsonPayload;
    let r = rig(true);
    r.runner.script(Ran::Done(JsonPayload(
        r#"{"content":[{"text":"boom","type":"text"}],"isError":true}"#.into(),
    )));
    receive(&r, standard("req-1")).await.unwrap();
    assert_eq!(
        r.ledger.outcomes(),
        ["held"],
        "a tool returning isError may still have produced partial side effects"
    );
    // Non-MCP-shaped results are not treated as successful completion.
    r.runner
        .script(Ran::Done(JsonPayload(r#"{"ok":true}"#.into())));
    fresh_run(&r, 2);
    receive(&r, standard("req-2")).await.unwrap();
    assert_eq!(r.ledger.outcomes(), ["held", "held"]);
    // Server rejected before execution: no external side effects.
    r.runner.script(Ran::Rejected);
    fresh_run(&r, 3);
    assert!(matches!(
        receive(&r, standard("req-3")).await,
        Err(Error::Denied)
    ));
    assert_eq!(r.ledger.outcomes(), ["held", "held", "rejected"]);
}
