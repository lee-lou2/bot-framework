//! Acceptance tests for historical revision restore after `--activate` redeploy (multi-process, PG, srt, python3).
//! Verifies missed static cron catch-up under the segment's revision, pending approval continuity across instruction-only
//! updates, and `stale`/`denied` rejection with zero executions when a tool is revoked in the new revision.
//! Bundle corruption/missing-bundle cases live in `serve_restore_bundles.rs`.
mod process_support;
mod restore_support;
use process_support::*;
use restore_support::*;
use std::time::Duration;

#[test]
fn cron_occurrences_missed_between_two_processes_run_with_their_segments_revision_and_leave_no_dead_letter()
 {
    let Some(c) = Cluster::new() else {
        return;
    };
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", instructions_changed),
    );
    let first = c.start(&v1, false, &[]);
    let r1 = c.profile_revision();
    drop(first);
    // Backdate the schedule segment and cursor by 5 minutes so missed occurrences fire immediately using DB time.
    assert_eq!(
        c.q("update {s}.bot_schedule_segments set from_ms = from_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    assert_eq!(
        c.q("update {s}.bot_schedule_cursors set cursor_ms = cursor_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    let _second = c.start(&v2, true, &["worker", "scheduler"]);
    let r2 = c.profile_revision();
    assert_ne!(r1, r2);
    let missed = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}'"
    );
    wait_until("missed occurrences were not accepted", || {
        c.q(&missed).parse::<u32>().unwrap_or(0) >= 4
    });
    let done = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}' and state = 'succeeded'"
    );
    wait_until("old-revision cron runs did not finish", || {
        c.q(&done) == c.q(&missed)
    });
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0",
        "expected zero dead letters"
    );
    assert_eq!(
        c.q("select count(*) - count(distinct at_ms) from {s}.bot_occurrences where trigger = 'morning'"),
        "0"
    );
    assert_eq!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_occurrences o join {{s}}.bot_runs r on r.profile = o.profile and r.id = o.run where o.trigger = 'morning' and r.revision = '{r1}'"
        )),
        c.q(&missed)
    );
    assert_ne!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_runs r where r.trigger = 'morning' and r.revision = '{r1}' and r.result like '%Walk%'"
        )),
        "0",
        "old-revision direct tool run must produce agenda output"
    );
}

#[test]
fn a_pending_approval_survives_a_redeploy_and_the_call_runs_exactly_once() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", instructions_changed),
    );
    let first = c.start(&v1, false, &["worker", "admin"]);
    let env1 = c.env(&v1);
    let start = bot(
        &refs(&env1),
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "redeploy-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let card = wait_card("approval card did not open", first.admin.as_ref().unwrap());
    let r1 = c.profile_revision();
    // Redeploy with instruction-only changes: approval policy is unchanged so the same card remains pending.
    drop(first);
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.profile_revision(), r1);
    let admin = second.admin.as_ref().unwrap();
    let again = wait_card("approval card disappeared after redeploy", admin);
    assert_eq!(again["nonce"], card["nonce"], "expected same approval card");
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state in ('started', 'done')"),
        "0",
        "must not execute before approval"
    );
    let (status, body) = decide(admin, &again, true);
    assert_eq!(status, 200, "{body}");
    let env2 = c.env(&v2);
    let done = bot(
        &refs(&env2),
        &["chat", "--attach", "--request-id", "redeploy-del"],
    );
    assert!(done.status.success(), "{}", text(&done));
    assert!(
        text(&done).contains("Simulated") && text(&done).contains("virtual receipt sim-"),
        "{}",
        text(&done)
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state = 'done'"),
        "1"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_approvals where state = 'consumed'"),
        "1"
    );
    assert_eq!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_runs where state = 'succeeded' and revision = '{r1}' and trigger is null"
        )),
        "1"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0"
    );
}

#[test]
fn a_tool_revoked_by_the_new_revision_makes_the_old_approval_stale_and_never_runs() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", tool_revoked("delete")),
    );
    let first = c.start(&v1, false, &["worker", "admin"]);
    let start = bot(
        &refs(&c.env(&v1)),
        &[
            "chat",
            "--no-wait",
            "--request-id",
            "revoked-del",
            "Delete the walk event",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let card = wait_card("approval card did not open", first.admin.as_ref().unwrap());
    drop(first);
    let second = c.start(&v2, true, &["worker", "admin"]);
    let admin = second.admin.as_ref().unwrap();
    // New revision revoked the tool, so approving the old card must fail as stale.
    let seen = pending_card(admin);
    let target = if seen.is_null() { &card } else { &seen };
    let (status, body) = decide(admin, target, true);
    assert!(
        status == 409 && body.contains("stale"),
        "expected stale 409 for revoked tool approval: {status} {body}"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state in ('started', 'done')"),
        "0",
        "tool must never execute"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_approvals where state = 'consumed'"),
        "0"
    );
}

#[test]
fn a_direct_tool_run_of_a_tool_the_new_revision_revoked_is_denied_and_never_runs() {
    let Some(c) = Cluster::new() else {
        return;
    };
    let (v1, v2) = (
        c.profiles("v1", |_| {}),
        c.profiles("v2", tool_revoked("list")),
    );
    let first = c.start(&v1, false, &[]);
    let r1 = c.profile_revision();
    drop(first);
    assert_eq!(
        c.q("update {s}.bot_schedule_segments set from_ms = from_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    assert_eq!(
        c.q("update {s}.bot_schedule_cursors set cursor_ms = cursor_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    let _second = c.start(&v2, true, &["worker", "scheduler"]);
    let missed = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}'"
    );
    wait_until("missed occurrences were not accepted", || {
        c.q(&missed).parse::<u32>().unwrap_or(0) >= 4
    });
    let ended = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}' and state in ('dead', 'succeeded')"
    );
    wait_until("old-revision cron runs did not finish", || {
        c.q(&ended) == c.q(&missed)
    });
    assert_eq!(
        c.q(&format!(
            "select string_agg(distinct state || '/' || coalesce(error, ''), ',') from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}'"
        )),
        "dead/denied",
        "direct tool run must fail with denied"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.list' and state in ('started', 'done')"),
        "0",
        "tool must never execute"
    );
}
