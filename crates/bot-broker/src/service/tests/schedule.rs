//! Built-in `schedule.create`, `schedule.list`, and `schedule.cancel` broker tests.

use super::*;
use bot_core::schedule::utc::parse_utc;
use bot_core::{
    ledger::{ApprovalState, CallLedger, CallPhase, StartCall},
    schedule::{MAX_ACTIVE_PER_OWNER, Timing},
    state::{ConversationKind, Risk},
};

pub(super) const WITH_SCHEDULE: [&str; 10] = [
    "run",
    "calendar.read",
    "calendar.delete",
    "calendar.write",
    "web.search",
    "mail.send",
    "chat.post",
    "schedule.create",
    "schedule.list",
    "schedule.cancel",
];
pub(super) const OWNER: &str = "local:owner";
pub(super) const AT: &str = "1970-01-13T12:00:00Z";

pub(super) fn snapshot_with(revision: &str) -> SnapshotBuilder {
    base(revision)
        .role("owner", &WITH_SCHEDULE)
        .policy_scopes(&WITH_SCHEDULE)
}

pub(super) fn schedule_world() -> World {
    let w = World::new();
    w.routes.register(
        "personal",
        "team",
        bot_core::state::ConversationKind::Channel,
        "channel-a",
        "channel-a",
    );
    w.publish(snapshot_with("r1").build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    w
}

pub(super) fn create(w: &World, call: &str, args: &str) -> Result<CallResult, Error> {
    w.call(call, "schedule.create", args)
}

pub(super) fn at_args(instruction: &str) -> String {
    format!(r#"{{"at":"{AT}","instruction":"{instruction}"}}"#)
}

fn id_of(payload: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(payload).unwrap();
    value["schedule"].as_str().expect("schedule").to_owned()
}

pub(super) fn refused(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("schedule.refuse".into(), word.into()))
}

#[test]
fn an_owner_creates_lists_and_cancels_their_own_schedule() {
    let w = schedule_world();
    let created = done(create(&w, "c1", &at_args("check the mail")));
    let id = id_of(&created);
    let (instruction, timing, quiet) = w.schedules.get(&id).expect("stored");
    assert_eq!(instruction, "check the mail");
    assert_eq!(timing, Timing::At(parse_utc(AT).unwrap()));
    assert!(!quiet, "quiet defaults to false");
    let listed = done(w.call("c2", "schedule.list", "{}"));
    assert_eq!(
        listed,
        format!(r#"{{"schedules":[{{"at":"{AT}","id":"{id}","quiet":false}}]}}"#)
    );
    assert!(!listed.contains("check the mail"));
    assert_eq!(
        done(w.call("c3", "schedule.cancel", &format!(r#"{{"id":"{id}"}}"#))),
        format!(r#"{{"schedule":"{id}"}}"#)
    );
    assert_eq!(w.schedules.count(), 0);
    assert_eq!(
        done(w.call("c4", "schedule.list", "{}")),
        r#"{"schedules":[]}"#
    );
    assert!(w.runner.jobs().is_empty(), "must not invoke a tool server");
}

#[test]
fn a_cron_schedule_uses_the_profile_timezone_unless_one_is_given() {
    let w = schedule_world();
    let plain = id_of(&done(create(
        &w,
        "c1",
        r#"{"cron":"0 9 * * 1-5","instruction":"standup","quiet":true}"#,
    )));
    let (_, timing, quiet) = w.schedules.get(&plain).unwrap();
    assert!(
        matches!(&timing, Timing::Cron { expression, timezone }
            if expression.as_str() == "0 9 * * 1-5" && timezone == "UTC"),
        "{timing:?}"
    );
    assert!(quiet);
    let seoul = id_of(&done(create(
        &w,
        "c2",
        r#"{"cron":"0 9 * * *","timezone":"Asia/Seoul","instruction":"morning"}"#,
    )));
    assert!(matches!(
        &w.schedules.get(&seoul).unwrap().1,
        Timing::Cron { timezone, .. } if timezone == "Asia/Seoul"
    ));
    let listed = done(w.call("c3", "schedule.list", "{}"));
    assert!(
        listed.contains(r#""cron":"0 9 * * 1-5""#) && listed.contains(r#""timezone":"Asia/Seoul""#)
    );
    assert!(matches!(
        create(
            &w,
            "c4",
            r#"{"cron":"0 9 * * *","timezone":"Mars/Base","instruction":"x"}"#
        ),
        Err(Error::Invalid)
    ));
}

#[test]
fn bad_arguments_never_create_a_call_record_or_a_schedule() {
    let w = schedule_world();
    for (i, args) in [
        r#"{"instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00Z","cron":"* * * * *","instruction":"x"}"#,
        r#"{"at":"2030-01-01T12:00:00+09:00","instruction":"x"}"#,
        r#"{"at":"1970-01-02T00:00:00Z","instruction":"x"}"#, // earlier than fixture clock (1970-01-12)
        r#"{"at":"2030-01-01T12:00:00Z","instruction":""}"#,
        r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","extra":1}"#,
        r#"{"cron":"every day","instruction":"x"}"#,
        "[]",
    ]
    .iter()
    .enumerate()
    {
        let call = format!("bad{i}");
        assert!(
            matches!(create(&w, &call, args), Err(Error::Invalid)),
            "{args}"
        );
        assert!(!w.ledger.has_call("r1", &call), "{args}");
    }
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_redelivered_call_returns_its_receipt_even_after_the_time_has_passed() {
    let w = schedule_world();
    let first = done(create(&w, "c1", &at_args("once")));
    assert_eq!(done(create(&w, "c1", &at_args("once"))), first);
    assert_eq!(w.schedules.count(), 1, "must not create a duplicate");
    w.clock.set(14 * 24 * 3600);
    assert_eq!(done(create(&w, "c1", &at_args("once"))), first);
    assert!(matches!(
        create(&w, "c1", &at_args("something else")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Missing | Error::Conflict | Error::Invalid | Error::Denied) | Ok(_)
    ));
    assert_eq!(w.schedules.count(), 1);
}

#[test]
fn the_scope_the_human_requester_and_the_destination_are_all_required() {
    let w = World::new();
    w.publish(
        base("r1")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        create(&w, "c1", &at_args("x")),
        Err(Error::Denied)
    ));
    assert!(refused(&w, "scope_missing"));
    assert!(!w.ledger.has_call("r1", "c1"));
    let w = schedule_world();
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "service:morning",
        "main",
        &WITH_SCHEDULE,
    ));
    let result = w.call_on(&w.broker, "r2", 1, "c1", "schedule.create", &at_args("x"));
    assert!(matches!(result, Err(Error::Denied)));
    assert!(refused(&w, "not_human"));
    let other = r#"{"at":"2030-01-01T12:00:00Z","instruction":"x","destination":{"binding":"b","conversation":{"kind":"direct","id":"D1"}}}"#;
    assert!(matches!(create(&w, "c2", other), Err(Error::Denied)));
    assert!(refused(&w, "destination_unsupported"));
    assert!(!w.ledger.has_call("r1", "c2"));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn the_owner_limit_is_enforced_by_the_store_and_a_refusal_changes_nothing() {
    let w = schedule_world();
    w.schedules.fill("personal", OWNER, MAX_ACTIVE_PER_OWNER);
    assert!(matches!(
        create(&w, "c1", &at_args("one too many")),
        Err(Error::Conflict)
    ));
    assert_eq!(w.schedules.count(), MAX_ACTIVE_PER_OWNER);
    w.ledger.add_run(run_context(
        "personal",
        "r2",
        1,
        "local:alice",
        "main",
        &WITH_SCHEDULE,
    ));
    let result = w.call_on(
        &w.broker,
        "r2",
        1,
        "c1",
        "schedule.create",
        &at_args("alice"),
    );
    assert!(matches!(result, Ok(CallResult::Done(_))), "{result:?}");
}

#[test]
fn only_the_owner_can_cancel_and_the_list_shows_only_their_own() {
    let w = schedule_world();
    w.schedules.fill("personal", "local:alice", 1);
    let mine = id_of(&done(create(&w, "c1", &at_args("mine"))));
    assert!(matches!(
        w.call("c2", "schedule.cancel", r#"{"id":"sched-001"}"#),
        Err(Error::Missing)
    ));
    assert!(
        w.schedules.get("sched-001").is_some(),
        "another owner's schedule must remain intact"
    );
    let listed = done(w.call("c3", "schedule.list", "{}"));
    assert!(
        listed.contains(&mine) && !listed.contains("sched-001"),
        "{listed}"
    );
    assert!(matches!(
        w.call("c4", "schedule.list", r#"{"all":true}"#),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        w.call("c5", "schedule.cancel", "{}"),
        Err(Error::Invalid)
    ));
}

#[test]
fn a_policy_that_requires_approval_shows_the_instruction_and_creates_nothing_until_it_is_approved()
{
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("send the weekly report")));
    assert_eq!(w.schedules.count(), 0);
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.tool.0, "schedule.create");
    assert_eq!(stored.approval.tool_revision.0, "builtin-1");
    assert_eq!(stored.approval.risk, Risk::Write);
    assert_eq!(stored.reasons, vec!["policy".to_string()]);
    assert!(
        stored.card.0.contains("send the weekly report"),
        "{}",
        stored.card.0
    );
    w.decide(&nonce, OWNER, true).unwrap();
    let receipt = done(create(&w, "c1", &at_args("send the weekly report")));
    assert_eq!(w.schedules.count(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(
        done(create(&w, "c1", &at_args("send the weekly report"))),
        receipt
    );
    assert_eq!(w.schedules.count(), 1);
    assert!(matches!(
        w.call("c2", "schedule.list", "{}"),
        Ok(CallResult::Done(_))
    ));
}

#[test]
fn an_old_approval_button_goes_stale_when_the_policy_or_the_scope_changed() {
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("x")));
    w.publish(snapshot_with("r2").approve(&["schedule.create"]).build());
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    let w = World::new();
    w.publish(snapshot_with("r1").approve(&["schedule.create"]).build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let nonce = awaiting(create(&w, "c1", &at_args("x")));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(w.decide(&nonce, OWNER, true), Err(Error::Stale)));
    assert_eq!(w.schedules.count(), 0);
}

#[test]
fn a_result_is_hidden_when_the_revision_or_the_claim_moved_while_it_was_made() {
    let w = schedule_world();
    let first = done(create(&w, "c1", &at_args("x")));
    w.ledger.set_active_revision("personal", "r2");
    assert!(matches!(
        create(&w, "c1", &at_args("x")),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c2", "schedule.list", "{}"),
        Err(Error::Conflict | Error::Stale)
    ));
    w.ledger.set_active_revision("personal", "r1");
    assert_eq!(done(create(&w, "c1", &at_args("x"))), first);
    let old = w.ledger.claim_epoch("r1");
    w.ledger.bump_claim("r1");
    let stale = block_on(w.broker.call(ToolCall {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(old),
        },
        call: CallId("c9".into()),
        tool: ToolId("schedule.create".into()),
        args: JsonPayload(at_args("late")),
    }));
    assert!(matches!(stale, Err(Error::Stale)), "{stale:?}");
}

#[test]
fn the_host_only_id_and_the_generic_start_are_closed() {
    let w = schedule_world();
    assert!(matches!(
        w.call("c1", "schedule.deliver", "{}"),
        Err(Error::Missing)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
    for (call, tool) in [("g1", "schedule.create"), ("g2", "schedule.cancel")] {
        let attempt = Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            generation: Generation(1),
            claim: Some(w.ledger.claim_epoch("r1")),
        };
        block_on(w.ledger.begin(
            attempt.clone(),
            CallId(call.into()),
            ToolId(tool.into()),
            [0; 32],
        ))
        .unwrap();
        let start = StartCall {
            checked_revision: Revision("r1".into()),
            risk: Risk::Write,
            tool_revision: Revision("builtin-1".into()),
            attempt,
            call: CallId(call.into()),
            approval: None,
            private_recall: false,
            owner: "x".into(),
        };
        assert!(block_on(w.ledger.start(start)).is_err(), "{tool}");
    }
}

#[test]
fn without_a_store_every_schedule_operation_is_unavailable() {
    let w = schedule_world();
    let bare = {
        use crate::service::{BrokerService, Parts};
        BrokerService::new(
            Parts {
                catalog: w.catalog.clone(),
                memory: w.memory.clone(),
                ledger: w.ledger.clone(),
                effects: w.effects.clone(),
                audit: w.audit.clone(),
                runner: w.runner.clone(),
                sealer: Arc::new(FakeSealer),
                tracer: w.tracer.clone(),
                clock: w.clock.clone(),
                entropy: w.entropy.clone(),
            },
            Config::default(),
        )
    };
    let try_call = |call: &str, tool: &str, args: &str| w.call_on(&bare, "r1", 1, call, tool, args);
    assert!(matches!(
        try_call("c1", "schedule.create", &at_args("x")),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        try_call("c2", "schedule.list", "{}"),
        Err(Error::Unavailable)
    ));
    assert!(matches!(
        try_call("c3", "schedule.cancel", r#"{"id":"s"}"#),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.schedules.count(), 0);
    let _ = ConversationKind::Cli;
}

#[test]
fn a_list_is_recorded_as_a_call_and_its_result_is_reused_after_the_authority_is_checked_again() {
    let w = schedule_world();
    let first = done(w.call("c1", "schedule.list", "{}"));
    assert_eq!(first, r#"{"schedules":[]}"#);
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Done(p) if p.0 == first),
        "{:?}",
        w.ledger.phase("r1", "c1")
    );
    done(create(&w, "c2", &at_args("later")));
    assert_eq!(done(w.call("c1", "schedule.list", "{}")), first);
    assert!(done(w.call("c3", "schedule.list", "{}")).contains("sched-001"));
    assert!(matches!(
        w.call("c1", "schedule.cancel", r#"{"id":"x"}"#),
        Err(Error::Conflict)
    ));
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .build(),
    );
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn a_list_that_could_not_be_read_is_given_back_so_a_redelivery_reads_it_again() {
    let w = schedule_world();
    w.schedules.fail_list_with(Some(Error::Unavailable));
    assert!(matches!(
        w.call("c1", "schedule.list", "{}"),
        Err(Error::Unavailable)
    ));
    assert!(
        matches!(&w.ledger.phase("r1", "c1"), CallPhase::Open { .. }),
        "failed read must release the call back to Open: {:?}",
        w.ledger.phase("r1", "c1")
    );
    w.schedules.fail_list_with(None);
    assert_eq!(
        done(w.call("c1", "schedule.list", "{}")),
        r#"{"schedules":[]}"#
    );
}

#[test]
fn the_default_timezone_is_fixed_from_the_pinned_revision_for_the_card_the_hash_and_the_store() {
    let policy = |rev: &str, zone: &str| {
        snapshot_with(rev)
            .approve(&["schedule.create"])
            .timezone(zone)
            .policy_revision("p-same") // keep policy revision stable while timezone changes
            .build()
    };
    let w = World::new();
    w.publish(policy("r1", "Asia/Seoul"));
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    let args = r#"{"cron":"0 9 * * *","instruction":"morning report"}"#;
    let nonce = awaiting(create(&w, "c1", args));
    let stored = w.ledger.stored_approval(&nonce);
    assert!(
        stored.card.0.contains("Asia/Seoul"),
        "card must display the effective timezone: {}",
        stored.card.0
    );
    w.publish(policy("r2", "UTC"));
    w.decide(&nonce, OWNER, true).unwrap();
    let id = id_of(&done(create(&w, "c1", args)));
    let (_, timing, _) = w.schedules.get(&id).unwrap();
    assert!(
        matches!(&timing, Timing::Cron { timezone, .. } if timezone == "Asia/Seoul"),
        "{timing:?}"
    );
}
