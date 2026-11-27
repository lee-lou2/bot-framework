//! Tests for `Preflight::refusal` against the pinned revision without side effects.

use super::*;
use crate::{ports::Sealed, service::preflight::Preflight};
use bot_core::{cli::CliRefusal, profile::Tool, state::Risk};

struct PickySealer;

impl Sealer for PickySealer {
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
        if args.0.contains("bad") {
            return Err(Error::Invalid);
        }
        if args.0.contains("boom") {
            return Err(Error::Unavailable);
        }
        FakeSealer.seal(tool, args)
    }
}

fn attempt(w: &World, run: &str) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.into()),
        claim: Some(w.ledger.claim_epoch(run)),
        generation: Generation(1),
    }
}

fn ask(
    w: &World,
    broker: &BrokerService,
    tool: &str,
    args: &str,
) -> Result<Option<CliRefusal>, Error> {
    block_on(broker.refusal(
        attempt(w, "r1"),
        ToolId(tool.into()),
        JsonPayload(args.into()),
    ))
}

#[test]
fn a_tool_the_pinned_revision_does_not_show_the_agent_is_a_confirmed_unknown_tool() {
    let w = World::new();
    assert_eq!(
        ask(&w, &w.broker, "agenda.nothing", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    assert!(matches!(
        w.call("c1", "agenda.nothing", "{}"),
        Err(Error::Missing)
    ));
    w.publish(
        base("r1")
            .tool(
                "main/research",
                "research.only",
                tool(Risk::Read, &["calendar.read"]),
            )
            .build(),
    );
    assert_eq!(
        ask(&w, &w.broker, "research.only", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    assert_eq!(ask(&w, &w.broker, "agenda.list", "{}").unwrap(), None);
}

#[test]
fn only_the_pinned_revision_decides_a_tool_withdrawn_from_the_current_one_is_not_confirmed() {
    let w = World::new();
    w.publish(
        snapshot("personal", "r2")
            .role("owner", &ALL)
            .member("local:owner", &["owner"])
            .policy_scopes(&ALL)
            .build(),
    );
    assert_eq!(ask(&w, &w.broker, "agenda.list", "{}").unwrap(), None);
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn an_input_the_pinned_declaration_rejects_is_invalid_but_a_sealer_failure_is_not() {
    let w = World::new();
    let picky = w.bare_with(Arc::new(PickySealer));
    assert_eq!(
        ask(&w, &picky, "agenda.list", r#"{"x":"bad"}"#).unwrap(),
        Some(CliRefusal::Invalid)
    );
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c1", "agenda.list", r#"{"x":"bad"}"#),
        Err(Error::Invalid)
    ));
    assert_eq!(
        ask(&w, &picky, "agenda.list", r#"{"x":"ok"}"#).unwrap(),
        None
    );
    assert!(matches!(
        ask(&w, &picky, "agenda.list", r#"{"x":"boom"}"#),
        Err(Error::Unavailable)
    ));
}

#[test]
fn an_unverifiable_revision_or_a_lost_claim_or_a_down_ledger_confirms_nothing() {
    let w = World::new();
    let mut ctx = run_context("personal", "r5", 1, "local:owner", "main", &ALL);
    ctx.work.revision = Revision("gone".into());
    w.ledger.add_run(ctx);
    let lost = block_on(w.broker.refusal(
        attempt(&w, "r5"),
        ToolId("agenda.list".into()),
        JsonPayload("{}".into()),
    ));
    assert!(matches!(lost, Err(Error::Unavailable)), "{lost:?}");
    assert!(matches!(
        w.call_on(&w.broker, "r5", 1, "c1", "agenda.list", "{}"),
        Err(Error::Missing)
    ));
    let old = attempt(&w, "r1");
    w.ledger.bump_claim("r1");
    let result = block_on(w.broker.refusal(
        old,
        ToolId("agenda.nothing".into()),
        JsonPayload("{}".into()),
    ));
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    w.ledger.set_context_down(true);
    assert!(matches!(
        ask(&w, &w.broker, "agenda.nothing", "{}"),
        Err(Error::Unavailable)
    ));
}

#[test]
fn host_builtin_tools_are_judged_only_by_their_own_argument_checks_never_by_the_profile() {
    let w = World::new();
    for (id, args) in [
        ("memory.share", r#"{"id":"m-1"}"#),
        (
            "schedule.create",
            r#"{"at":"1970-01-13T12:00:00Z","instruction":"x"}"#,
        ),
        ("schedule.list", "{}"),
        ("schedule.cancel", r#"{"id":"s-1"}"#),
        ("delegate", r#"{"agent":"main/research","instruction":"x"}"#),
    ] {
        assert_eq!(ask(&w, &w.broker, id, args).unwrap(), None, "{id}");
    }
    for (id, args) in [
        ("memory.share", "{}"),
        ("memory.share", r#"{"id":"m-1","extra":1}"#),
        ("schedule.create", "{}"),
        (
            "schedule.create",
            r#"{"at":"1970-01-02T00:00:00Z","instruction":"x"}"#,
        ),
        ("schedule.list", r#"{"x":1}"#),
        ("schedule.cancel", "{}"),
    ] {
        assert_eq!(
            ask(&w, &w.broker, id, args).unwrap(),
            Some(CliRefusal::Invalid),
            "{id} {args}"
        );
    }
    assert_eq!(
        ask(&w, &w.broker, "schedule.deliver", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    assert_eq!(ask(&w, &w.broker, "delegate", "{}").unwrap(), None);
    assert!(
        w.ledger.call_ids("r1").is_empty(),
        "must not record any call"
    );
}

#[test]
fn asking_changes_nothing_and_always_gives_the_same_answer() {
    let w = World::new();
    for _ in 0..3 {
        assert_eq!(
            ask(&w, &w.broker, "agenda.nothing", "{}").unwrap(),
            Some(CliRefusal::UnknownTool)
        );
    }
    assert!(
        w.ledger.call_ids("r1").is_empty(),
        "must not record any call"
    );
    assert!(w.audit.entries().is_empty(), "must not write audit entries");
    assert!(w.runner.jobs().is_empty());
}
