//! Redelivery of `schedule.create` calls completed under legacy sealing (no `quiet`, raw `at` JSON).

use super::{
    schedule::{AT, WITH_SCHEDULE, at_args, create, schedule_world, snapshot_with},
    *,
};
use crate::canon::sha256;
use bot_core::ledger::{CallLedger, CallPhase};

fn old_hash() -> [u8; 32] {
    sha256(&format!(r#"{{"at":"{AT}","instruction":"once"}}"#))
}

fn plant(w: &World, call: &str, tool: &str, hash: [u8; 32]) {
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        claim: Some(w.ledger.claim_epoch("r1")),
        generation: Generation(1),
    };
    block_on(
        w.ledger
            .begin(attempt, CallId(call.into()), ToolId(tool.into()), hash),
    )
    .expect("begin");
}

#[test]
fn a_call_finished_under_the_old_seal_replays_its_receipt_and_nothing_else() {
    let w = schedule_world();
    plant(&w, "old1", "schedule.create", old_hash());
    w.ledger
        .force_done("r1", "old1", r#"{"schedule":"sched-007"}"#);
    assert_eq!(
        done(create(&w, "old1", &at_args("once"))),
        r#"{"schedule":"sched-007"}"#
    );
    assert_eq!(w.schedules.count(), 0);
    assert_eq!(
        w.ledger.call_hash("r1", "old1"),
        old_hash(),
        "ledger fingerprint must remain unchanged"
    );
    assert!(matches!(
        create(&w, "old1", &at_args("something else")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("old1", "schedule.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("old1", "schedule.cancel", r#"{"id":"sched-007"}"#),
        Err(Error::Conflict)
    ));
    // Redelivery after the scheduled time has passed still replays the receipt (future check applies only to new calls).
    w.clock.set(2_000_000_000 + 1_800_000_000);
    assert_eq!(
        done(create(&w, "old1", &at_args("once"))),
        r#"{"schedule":"sched-007"}"#
    );
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn the_old_fingerprint_follows_the_old_sealing_for_a_filled_in_timezone_too() {
    let w = schedule_world();
    // Legacy sealing filled an omitted cron timezone with the pinned profile timezone (UTC).
    let old = sha256(r#"{"cron":"0 9 * * *","instruction":"standup","timezone":"UTC"}"#);
    plant(&w, "old2", "schedule.create", old);
    w.ledger
        .force_done("r1", "old2", r#"{"schedule":"sched-008"}"#);
    assert_eq!(
        done(create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","instruction":"standup"}"#
        )),
        r#"{"schedule":"sched-008"}"#
    );
    assert_eq!(
        done(create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","timezone":"UTC","instruction":"standup"}"#
        )),
        r#"{"schedule":"sched-008"}"#
    );
    assert!(matches!(
        create(
            &w,
            "old2",
            r#"{"cron":"0 9 * * *","timezone":"Asia/Seoul","instruction":"standup"}"#
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn an_unfinished_call_with_the_old_fingerprint_is_never_reused() {
    let w = schedule_world();
    plant(&w, "open", "schedule.create", old_hash());
    assert!(matches!(
        create(&w, "open", &at_args("once")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "open"),
        CallPhase::Open { approval: None }
    ));
    plant(&w, "started", "schedule.create", old_hash());
    w.ledger.force_started("r1", "started", 1, "owner.started");
    assert!(matches!(
        create(&w, "started", &at_args("once")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.ledger.phase("r1", "started"),
        CallPhase::Started { .. }
    ));
    assert_eq!(w.schedules.count(), 0);
    assert!(
        w.audit
            .verdicts()
            .iter()
            .all(|(action, _)| action.as_str() != "call.deny")
    );
    // Broker still rejects mismatched hashes even if the store does not filter on `begin`.
    w.ledger.set_lenient_begin(true);
    for call in ["open", "started"] {
        let result = create(&w, call, &at_args("once"));
        assert!(matches!(result, Err(Error::Conflict)), "{call}: {result:?}");
    }
    assert_eq!(w.schedules.count(), 0);
    assert!(matches!(
        w.ledger.phase("r1", "open"),
        CallPhase::Open { approval: None }
    ));
}

#[test]
fn a_finished_call_is_replayed_only_while_the_requester_is_still_authorized() {
    let w = schedule_world();
    plant(&w, "old3", "schedule.create", old_hash());
    w.ledger
        .force_done("r1", "old3", r#"{"schedule":"sched-009"}"#);
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        create(&w, "old3", &at_args("once")),
        Err(Error::Denied)
    ));
    w.publish(snapshot_with("r3").build());
    assert_eq!(
        done(create(&w, "old3", &at_args("once"))),
        r#"{"schedule":"sched-009"}"#
    );
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    let denied = w.call_on(
        &w.broker,
        "r2",
        1,
        "old3",
        "schedule.create",
        &at_args("once"),
    );
    assert!(matches!(denied, Err(Error::Denied)));
}
