//! Tool-selected environment variables: execution starts only when names and values match across pinned and current revisions.

use super::*;

fn picking(revision: &str, value: Option<&str>) -> SnapshotBuilder {
    let mut list = tool(Risk::Read, &["calendar.read"]);
    list.env = ["CALENDAR_NAME".to_owned()].into();
    let builder = base(revision).tool("main", "agenda.list", list);
    match value {
        Some(value) => builder.agent_env("main", "CALENDAR_NAME", value),
        None => builder,
    }
}

fn world_with(snapshot: SnapshotBuilder) -> World {
    let w = World::new();
    w.publish(snapshot.build());
    w
}

#[test]
fn the_selected_value_comes_from_the_agent_environment_and_reaches_the_job() {
    let w = world_with(picking("r1", Some("personal")));
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(
        w.runner.jobs()[0].env,
        [("CALENDAR_NAME".to_owned(), "personal".to_owned())]
    );
    assert!(matches!(
        w.call("c2", "agenda.update", "{}"),
        Ok(_) | Err(_)
    ));
    assert!(
        w.runner
            .jobs()
            .iter()
            .filter(|job| job.tool.id.0 == "agenda.update")
            .all(|job| job.env.is_empty())
    );
}

#[test]
fn a_name_the_agent_does_not_have_never_starts_the_tool() {
    let w = world_with(picking("r1", None));
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(w.runner.jobs().is_empty());
    assert!(
        !w.ledger.has_call("r1", "c1"),
        "call record must not be created"
    );
}

#[test]
fn a_changed_value_refuses_the_older_run_but_an_unchanged_one_still_runs() {
    let w = world_with(picking("r1", Some("personal")));
    w.publish(picking("r2", Some("shared-team")).build());
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(w.runner.jobs().is_empty());
    // Unchanged value still runs across a revision bump (e.g. policy-only update).
    w.publish(picking("r3", Some("personal")).build());
    assert_eq!(done(w.call("c2", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(w.runner.jobs()[0].env[0].1, "personal");
}

#[test]
fn a_changed_set_of_names_is_a_changed_definition() {
    let w = world_with(picking("r1", Some("personal")));
    let mut renamed = tool(Risk::Read, &["calendar.read"]);
    renamed.env = ["CALENDAR_ID".to_owned()].into();
    w.publish(
        base("r2")
            .tool("main", "agenda.list", renamed)
            .agent_env("main", "CALENDAR_ID", "personal")
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_approval_card_goes_stale_when_the_selected_value_changed_before_the_click() {
    let mut drop = tool(Risk::Irreversible, &["calendar.delete"]);
    drop.env = ["CALENDAR_NAME".to_owned()].into();
    let w = world_with(
        base("r1")
            .tool("main", "agenda.delete", drop.clone())
            .agent_env("main", "CALENDAR_NAME", "personal"),
    );
    let nonce = awaiting(w.call("c1", "agenda.delete", "{\"event\":\"walk\"}"));
    w.publish(
        base("r2")
            .tool("main", "agenda.delete", drop)
            .agent_env("main", "CALENDAR_NAME", "other-calendar")
            .build(),
    );
    assert!(w.decide(&nonce, "local:owner", true).is_err());
    assert!(w.runner.jobs().is_empty());
}
