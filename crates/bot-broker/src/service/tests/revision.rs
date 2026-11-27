//! Pre-return verification of store active revision, claim ownership, and cached tool/argument hashes.

use super::*;
use bot_core::ledger::CallPhase;

#[test]
fn a_revision_published_by_another_process_hides_the_result_even_when_this_catalog_is_stale() {
    let w = World::new();
    let ledger = w.ledger.clone();
    // Another process activates r2 before `still_current` runs while this process still caches r1.
    w.ledger
        .set_after_finish(move || ledger.set_active_revision("personal", "r2"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    assert!(
        matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)),
        "result must remain persisted in ledger"
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert_eq!(w.runner.jobs().len(), 1);
    w.ledger.set_active_revision("personal", "r1");
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
}

#[test]
fn a_result_is_not_returned_to_an_attempt_that_lost_its_claim_while_the_tool_ran() {
    let w = World::new();
    let ledger = w.ledger.clone();
    w.ledger.set_after_finish(move || {
        ledger.bump_claim("r1");
    });
    let result = w.call("c1", "agenda.list", "{}");
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
    let again = w.call("c1", "agenda.list", "{}");
    assert_eq!(done(again), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_profile_without_an_active_revision_in_the_store_gets_no_result() {
    let w = World::new();
    let ledger = w.ledger.clone();
    w.ledger
        .set_after_finish(move || ledger.set_active_revision("personal", ""));
    assert!(w.call("c1", "agenda.list", "{}").is_err());
    let w = World::new();
    w.ledger.set_active_revision("other-profile", "x");
    let ledger = w.ledger.clone();
    w.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "r1"));
    assert!(w.call("c1", "agenda.list", "{}").is_ok());
}

#[test]
fn a_cached_result_needs_the_same_tool_and_canonical_arguments_even_if_the_store_does_not_check() {
    let w = World::new();
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    w.ledger.set_lenient_begin(true);
    assert!(matches!(
        w.call("c1", "agenda.list", "{\"extra\":1}"),
        Err(Error::Conflict) | Err(Error::Invalid)
    ));
    assert!(matches!(
        w.call("c1", "agenda.update", "{}"),
        Err(Error::Conflict) | Err(Error::Invalid) | Err(Error::Denied)
    ));
    assert_eq!(w.runner.jobs().len(), 1, "must not re-execute");
}
