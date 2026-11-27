//! Tests for pre-record secret admission (`call.refuse secret_missing`) and ordering invariants.

use super::*;
use crate::ports::Sealed;
use bot_core::profile::Tool;

pub(super) fn needy(class: Risk) -> Tool {
    let mut t = tool(class, &["calendar.read"]);
    t.secrets.insert(
        "MAIL_TOKEN".into(),
        SecretName::parse("mail-token").unwrap(),
    );
    t
}

fn secret_world(class: Risk) -> World {
    let w = World::new();
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(class))
            .build(),
    );
    w
}

fn refused(w: &World) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.refuse".into(), "secret_missing".into()))
}

#[test]
fn a_missing_secret_refuses_a_new_call_before_it_is_recorded_or_any_approval_is_asked() {
    let w = secret_world(Risk::Irreversible);
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(refused(&w));
    assert!(!w.ledger.has_call("r1", "c1"), "must not record call");
    assert!(
        w.ledger.audit_log().is_empty(),
        "must not open approval or start call"
    );
    assert!(w.runner.jobs().is_empty());
    let checks = w.runner.secret_checks();
    assert_eq!(
        checks,
        vec![(ProfileId("personal".into()), vec!["mail-token".to_owned()])]
    );
    w.runner.restore_secret("mail-token");
    awaiting(w.call("c1", "agenda.mail", "{}"));
    assert!(w.ledger.has_call("r1", "c1"));
}

#[test]
fn an_unreadable_store_is_not_a_missing_secret_and_other_tools_are_never_checked() {
    let w = secret_world(Risk::Read);
    w.runner.fail_secret_checks_with(Some(Error::Unavailable));
    done(w.call("c1", "agenda.mail", "{}"));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(!refused(&w));
    let before = w.runner.secret_checks().len();
    done(w.call("c2", "agenda.list", "{}"));
    assert_eq!(w.runner.secret_checks().len(), before);
}

#[test]
fn only_new_calls_are_checked_a_recorded_call_continues_as_recorded() {
    let w = secret_world(Risk::Read);
    let first = done(w.call("c1", "agenda.mail", "{}"));
    w.runner.miss_secret("mail-token");
    assert_eq!(done(w.call("c1", "agenda.mail", "{}")), first);
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(matches!(
        w.call("c2", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(!w.ledger.has_call("r1", "c2"));
    assert!(refused(&w));
}

#[test]
fn the_argument_check_comes_first_so_a_bad_input_stays_invalid() {
    struct Picky;
    impl Sealer for Picky {
        fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
            if args.0.contains("bad") {
                return Err(Error::Invalid);
            }
            FakeSealer.seal(tool, args)
        }
    }
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token");
    let picky = w.bare_with(Arc::new(Picky));
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c1", "agenda.mail", r#"{"x":"bad"}"#),
        Err(Error::Invalid)
    ));
    assert!(!refused(&w));
    assert!(w.runner.secret_checks().is_empty());
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c2", "agenda.mail", r#"{"x":"ok"}"#),
        Err(Error::Denied)
    ));
    assert!(refused(&w));
}

fn denied(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.deny".into(), word.into()))
}

#[test]
fn a_name_the_call_may_never_use_is_refused_without_the_store_being_read_even_once() {
    let w = World::new();
    let mut reserved = tool(Risk::Read, &["calendar.read"]);
    reserved
        .secrets
        .insert("TOKEN".into(), SecretName::parse("private-intake").unwrap());
    w.publish(
        base("r1")
            .secret("private-intake")
            .tool("main", "agenda.mail", reserved)
            .build(),
    );
    w.runner.miss_secret("private-intake");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_reserved") && !refused(&w));
    assert!(
        w.runner.secret_checks().is_empty(),
        "must not query secret store"
    );
    assert!(w.runner.jobs().is_empty());
    let w = World::new();
    w.publish(
        base("r1")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_not_allowed") && !refused(&w));
    assert!(
        w.runner.secret_checks().is_empty(),
        "must not query secret store"
    );
    // Forbidden secret takes priority over missing allowed secret, reading neither.
    let w = World::new();
    let mut both = needy(Risk::Read);
    both.secrets
        .insert("OTHER".into(), SecretName::parse("not-listed").unwrap());
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", both)
            .build(),
    );
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(denied(&w, "secret_not_allowed") && !refused(&w));
    assert!(
        w.runner.secret_checks().is_empty(),
        "must not query secret store"
    );
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(refused(&w) && !denied(&w, "secret_not_allowed"));
    assert_eq!(w.runner.secret_checks().len(), 1);
}

#[test]
fn a_requester_whose_role_was_revoked_never_makes_the_store_be_read() {
    let w = secret_world(Risk::Read);
    w.runner.miss_secret("mail-token");
    w.publish(
        base("r2")
            .role("owner", &["run"])
            .policy_scopes(&["run"])
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(w.ledger.has_call("r1", "c1"));
    assert!(!refused(&w));
    assert!(
        w.runner.secret_checks().is_empty(),
        "must not query secret store"
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_call_over_the_rate_limit_never_makes_the_store_be_read_and_a_missing_secret_still_counts_against_it()
 {
    let w = World::with(cfg_with_rate(60, 2, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    for call in ["c1", "c2"] {
        assert!(matches!(
            w.call(call, "agenda.mail", "{}"),
            Err(Error::Denied)
        ));
        assert!(!w.ledger.has_call("r1", call));
    }
    assert_eq!(w.runner.secret_checks().len(), 2);
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::RateLimited)
    ));
    assert_eq!(
        w.runner.secret_checks().len(),
        2,
        "secret store checks unchanged"
    );
    assert!(!w.ledger.has_call("r1", "c3"));
    w.clock.advance(Duration::from_secs(60));
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert_eq!(w.runner.secret_checks().len(), 3);
}

#[test]
fn a_new_call_that_passes_the_checks_keeps_one_rate_slot_for_the_whole_call() {
    let w = World::with(cfg_with_rate(60, 2, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    done(w.call("c1", "agenda.mail", "{}"));
    done(w.call("c2", "agenda.mail", "{}"));
    assert!(matches!(
        w.call("c3", "agenda.mail", "{}"),
        Err(Error::RateLimited)
    ));
    assert_eq!(w.runner.jobs().len(), 2);
}
