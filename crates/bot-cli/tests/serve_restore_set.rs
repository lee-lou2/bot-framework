//! Process-level tests for bundle restore scoping: unreferenced old revisions and removed profiles never block redeployment, while re-adding a removed profile requires its exact historical bundle.
mod process_support;
mod restore_support;
use process_support::*;
use restore_support::*;
use std::path::Path;
use std::time::Duration;

fn without_trigger(personal: &Path) {
    std::fs::write(
        personal.join("agents/main/triggers.yaml"),
        "version: 1\ntriggers: []\n",
    )
    .unwrap();
}

fn without_trigger_and_changed(personal: &Path) {
    without_trigger(personal);
    instructions_changed(personal);
}

fn disable_freeze(c: &Cluster) {
    let done = c
        .schema
        .exec("alter table {s}.bot_profile_bundles disable trigger bot_profile_bundles_frozen");
    assert!(done.starts_with("ALTER TABLE"), "{done}");
}

#[test]
fn an_old_revision_nothing_references_needs_no_bundle_to_redeploy() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let (v1, v2) = (
        c.profiles("v1", without_trigger),
        c.profiles("v2", without_trigger_and_changed),
    );
    let first = c.start(&v1, false, &[]);
    let (r1, active) = (c.profile_revision(), c.schema.active());
    drop(first);
    c.q("truncate {s}.bot_profile_bundles");
    assert_eq!(c.q("select count(*) from {s}.bot_runs"), "0");
    let _second = c.start(&v2, true, &[]);
    assert_ne!(
        c.schema.active(),
        active,
        "new revision should be published"
    );
    assert_ne!(c.profile_revision(), r1);
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles"),
        "1",
        "only the new revision bundle should be stored"
    );
}

#[test]
fn a_profile_that_left_the_set_never_blocks_the_others_and_coming_back_needs_its_exact_original() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let s0 = c.profiles_with_second(
        "s0",
        without_trigger,
        "Old instructions for second profile.\n",
    );
    let a2 = c.profiles("a2", without_trigger_and_changed);
    let s1 = c.profiles_with_second(
        "s1",
        without_trigger_and_changed,
        "New instructions for second profile.\n",
    );
    let first = c.start(&s0, false, &[]);
    let second_old = c.q("select revision from {s}.bot_active_profiles where profile = 'second'");
    assert_eq!(second_old.len(), 64);
    drop(first);
    // Simulate legacy state where `second` has a dead run and closed schedule segment without a stored bundle.
    disable_freeze(&c);
    c.q("delete from {s}.bot_profile_bundles where profile = 'second'");
    let done = c.schema.exec(&format!(
        "insert into {{s}}.bot_runs (profile, id, root, agent, principal, revision, binding, conv_kind, conv_id, target, input, state, error, finished_ms)
         values ('second', 'old-dead', 'old-dead', 'main', 'local:owner', '{second_old}', 'desk', 'cli', 'c1', '{{\"agent\":\"main\",\"instruction\":\"x\"}}'::jsonb, 'x', 'dead', 'unknown_revision', {{s}}.bot_now_ms())"
    ));
    assert!(done.starts_with("INSERT 0 1"), "{done}");
    let done = c.schema.exec(&format!(
        "insert into {{s}}.bot_schedule_segments (profile, agent, trigger, from_ms, until_ms, revision, cron, timezone, principal, target)
         values ('second', 'main', 'old-cron', 1, {{s}}.bot_now_ms(), '{second_old}', '* * * * *', 'UTC', 'service:x', '{{}}'::jsonb)"
    ));
    assert!(done.starts_with("INSERT 0 1"), "{done}");
    let done = c.schema.exec(
        "insert into {s}.bot_schedule_cursors (profile, agent, trigger, cursor_ms) values ('second', 'main', 'old-cron', {s}.bot_now_ms() - 300000)",
    );
    assert!(done.starts_with("INSERT 0 1"), "{done}");
    let second_rows = |c: &Cluster| {
        c.q("select string_agg(id || ':' || state || ':' || attempts, ',') from {s}.bot_runs where profile = 'second'")
    };
    let before = second_rows(&c);
    assert_eq!(before, "old-dead:dead:0");

    // Removing `second` allows `personal` to redeploy and run without reviving `second`'s work.
    let mut running = c.start(&a2, true, &["worker", "scheduler"]);
    assert_eq!(
        c.q("select string_agg(profile, ',' order by profile) from {s}.bot_active_profiles"),
        "personal"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        second_rows(&c),
        before,
        "dead run of removed profile must not revive or spawn new runs"
    );
    for table in ["bot_calls", "bot_approvals", "bot_outbox", "bot_deliveries"] {
        assert_eq!(
            c.q(&format!(
                "select count(*) from {{s}}.{table} where profile = 'second'"
            )),
            "0",
            "{table}: removed profile must have zero activity"
        );
    }
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles where profile = 'second'"),
        "0",
        "must not fabricate or restore bundle for removed profile"
    );
    assert!(
        running.alive(),
        "joined worker/scheduler stopped unexpectedly"
    );
    drop(running);
    let operating = c.schema.active();

    // Re-adding `second` fails pre-publish because its historical pinned bundle is missing.
    let env = c.env(&s1);
    let (code, out) = serve_that_must_stop(&refs(&env), &["--role", "gateway", "--activate"])
        .expect("must refuse start when required historical bundle is missing");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "profile second revision {second_old}: stored bundle is missing"
        )),
        "{out}"
    );
    assert_eq!(
        c.schema.active(),
        operating,
        "active set must remain unchanged"
    );
    assert_eq!(second_rows(&c), before);
    // Putting a newer directory stores under its own hash and does not satisfy the missing historical revision.
    let new_folder = s1.join("second");
    let put = bot(
        &refs(&env),
        &["profile-bundle", "put", new_folder.to_str().unwrap()],
    );
    assert!(put.status.success(), "{}", text(&put));
    let (code, out) = serve_that_must_stop(&refs(&env), &["--role", "gateway", "--activate"])
        .expect("different revision must not satisfy missing bundle");
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains(&second_old), "{out}");
    // Backfilling the exact original directory satisfies restore and allows activation.
    let original = s0.join("second");
    let put = bot(
        &refs(&env),
        &["profile-bundle", "put", original.to_str().unwrap()],
    );
    assert!(put.status.success(), "{}", text(&put));
    assert!(
        text(&put).contains(&format!("profile second revision {second_old}")),
        "{}",
        text(&put)
    );
    let _back = c.start(&s1, true, &[]);
    assert_eq!(
        c.q("select string_agg(profile, ',' order by profile) from {s}.bot_active_profiles"),
        "personal,second"
    );
    assert_eq!(second_rows(&c), before);
}
