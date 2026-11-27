//! Process-level tests for atomic publication of static cron trigger segments alongside active revisions, upgrade replacement, and pre-switch catch-up.
use crate::process_support::*;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

fn without_trigger(dir: &Path) -> PathBuf {
    let changed = dir.join("no-trigger");
    copy_dir(&examples(), &changed);
    std::fs::write(
        changed.join("personal/agents/main/triggers.yaml"),
        "version: 1\ntriggers: []\n",
    )
    .unwrap();
    changed
}

fn workdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap()
}

#[test]
fn any_role_publishes_the_schedule_with_the_revision_and_a_failed_start_changes_nothing() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let dir = workdir();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let changed = without_trigger(dir.path());
    let open = || {
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms is null",
        )
    };
    let closed = || {
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms is not null",
        )
    };
    // Even gateway alone publishes static schedule definitions with the initial revision.
    let env = env_for(&examples(), &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();
    assert_eq!((open().as_str(), closed().as_str()), ("1", "0"));

    // Failed start (socket busy) must change neither the revision nor schedule segments.
    let env_changed = env_for(&changed, &url, &socket, &[]);
    let busy = bot(
        &refs(&env_changed),
        &["serve", "--role", "gateway", "--activate"],
    );
    assert_eq!(busy.status.code(), Some(1), "{}", text(&busy));
    assert_eq!(schema.active(), operating);
    assert_eq!((open().as_str(), closed().as_str()), ("1", "0"));

    // Activating the trigger-less revision closes the segment in the same commit.
    drop(server);
    let _next = start_gateway(&refs(&env_changed), &["--activate"], &socket);
    wait_for("new revision was not published", || {
        !schema.active().is_empty() && schema.active() != operating
    });
    assert_eq!((open().as_str(), closed().as_str()), ("0", "1"));
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms > from_ms"
        ),
        "1",
        "closed segment must span a positive interval"
    );
}

#[test]
fn the_first_activate_after_an_upgrade_replaces_an_older_revision_and_publishes_the_schedule() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let dir = workdir();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(&examples(), &url, &socket, &[]);
    let open = || {
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms is null",
        )
    };
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let current = schema.active();
    drop(server);
    // Simulate a legacy DB state lacking schedule segments under an older activation revision.
    assert_eq!(
        schema.query("update {s}.bot_active set revision = 'legacy-revision'"),
        "UPDATE 1"
    );
    schema.query("delete from {s}.bot_schedule_cursors");
    schema.query("delete from {s}.bot_schedule_segments");
    assert_eq!(open(), "0");
    let refused = bot(&refs(&env), &["serve", "--role", "gateway,scheduler"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), "legacy-revision");
    assert_eq!(open(), "0");
    let _next = start_roles(&refs(&env), "gateway,scheduler", &["--activate"], &socket);
    wait_for("--activate did not replace the revision", || {
        schema.active() == current
    });
    assert_eq!(open(), "1");
}

#[test]
fn a_deleted_trigger_still_catches_up_the_occurrences_before_the_switch_and_none_after() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let dir = workdir();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let changed = without_trigger(dir.path());
    let env = env_for(&examples(), &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();
    drop(server);
    // Backdate segment and cursor by 5 minutes so scheduler catch-up runs immediately.
    assert_eq!(
        schema.query(
            "update {s}.bot_schedule_segments set from_ms = from_ms - 300000 where trigger = 'morning'"
        ),
        "UPDATE 1"
    );
    assert_eq!(
        schema.query(
            "update {s}.bot_schedule_cursors set cursor_ms = cursor_ms - 300000 where trigger = 'morning'"
        ),
        "UPDATE 1"
    );

    let env_changed = env_for(&changed, &url, &socket, &[]);
    let _next = start_gateway(&refs(&env_changed), &["--activate"], &socket);
    wait_for("new revision was not published", || {
        !schema.active().is_empty() && schema.active() != operating
    });
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms is not null"
        ),
        "1"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_schedule_cursors where trigger = 'morning'"),
        "1",
        "cursor must remain for closed segment catch-up"
    );

    let _scheduler = spawn_role(&refs(&env_changed), "scheduler");
    let caught_up = || {
        schema
            .query("select count(*) from {s}.bot_occurrences where trigger = 'morning'")
            .parse::<u32>()
            .unwrap_or(0)
    };
    for _ in 0..300 {
        if caught_up() >= 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        caught_up() >= 4,
        "did not catch up pre-switch occurrences: {}",
        caught_up()
    );
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_occurrences o where o.trigger = 'morning' and o.at_ms > \
             (select until_ms from {s}.bot_schedule_segments where trigger = 'morning')"
        ),
        "0"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms is null"
        ),
        "0",
        "deleted trigger must not reopen"
    );
}
