//! 재배포 뒤 옛 판본 복원 수락 시험(실제 프로세스 여럿·팀 시험 DB·srt·python3): `--activate`로 새 판본을 올린 뒤에도 옛 판본에 묶인 일이 그 판본으로 끝난다.
//! - 정적 cron의 예정 시각이 두 프로세스 사이에 지났다 → 그 구간의 판본으로 시각마다 한 번 발화하고 dead letter는 0건이다.
//! - 승인을 기다리던 호출 → 지침만 바뀌었으면 옛 카드로 승인해 같은 호출이 정확히 한 번 실행된다.
//! - 새 판본이 도구를 철회했다 → 옛 승인 클릭은 `stale`, 직접 도구 실행은 `denied`이며 도구 실행은 0회다.
//!
//! 팀 시험 DB·`srt`·`python3`가 없으면 건너뛴다. 번들의 누락·손상·합류는 `serve_restore_bundles.rs`.
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
    // 옛 판본이 시작해 구간을 올린다
    let first = c.start(&v1, false, &[]);
    let r1 = c.profile_revision();
    drop(first);
    // 두 프로세스 사이에 예정 시각이 몇 번 지났다(스케줄러는 DB 시각만 쓰므로 구간을 5분 앞당겨 기다림 없이 만든다)
    assert_eq!(
        c.q("update {s}.bot_schedule_segments set from_ms = from_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    assert_eq!(
        c.q("update {s}.bot_schedule_cursors set cursor_ms = cursor_ms - 300000 where trigger = 'morning'"),
        "UPDATE 1"
    );
    // 새 판본을 `--activate`로 올린다: 지난 시각은 옛 구간의 판본(r1)으로 접수돼 끝난다
    let _second = c.start(&v2, true, &["worker", "scheduler"]);
    let r2 = c.profile_revision();
    assert_ne!(r1, r2);
    let missed = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}'"
    );
    wait_until("지난 예정 시각을 접수하지 못했어요", || {
        c.q(&missed).parse::<u32>().unwrap_or(0) >= 4
    });
    let done = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}' and state = 'succeeded'"
    );
    wait_until(
        "옛 판본의 예약 실행이 끝나지 않았어요",
        || c.q(&done) == c.q(&missed),
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_runs where state = 'dead'"),
        "0",
        "dead letter 0건"
    );
    // 예정 시각마다 한 번: 발생 키가 유일하고 발생마다 실행이 하나다
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
    // 도구는 실제로 실행됐다(합성 일정 서버의 읽기 호출)
    assert_ne!(
        c.q(&format!(
            "select count(*) from {{s}}.bot_runs r where r.trigger = 'morning' and r.revision = '{r1}' and r.result like '%산책%'"
        )),
        "0",
        "옛 판본의 직접 도구 실행이 결과를 냈다"
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
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let card = wait_card("승인 카드가 열리지 않았어요", first.admin.as_ref().unwrap());
    let r1 = c.profile_revision();
    // 재배포: 지침만 바뀐 새 판본(승인 조건 그대로)을 올린다. 같은 호출 · 같은 카드가 새 프로세스에서 대기 중이다
    drop(first);
    let second = c.start(&v2, true, &["worker", "admin"]);
    assert_ne!(c.profile_revision(), r1);
    let admin = second.admin.as_ref().unwrap();
    let again = wait_card("재배포 뒤 승인 카드가 사라졌어요", admin);
    assert_eq!(again["nonce"], card["nonce"], "같은 카드");
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state in ('started', 'done')"),
        "0",
        "승인 전에는 실행되지 않았다"
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
        text(&done).contains("시뮬레이션했어요") && text(&done).contains("가상 영수증 sim-"),
        "{}",
        text(&done)
    );
    // 정확히 한 번: 호출은 한 번 시작해 끝났고 카드는 한 번 소비됐으며 실행은 옛 판본에 묶인 채 성공했다
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
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let card = wait_card("승인 카드가 열리지 않았어요", first.admin.as_ref().unwrap());
    drop(first);
    let second = c.start(&v2, true, &["worker", "admin"]);
    let admin = second.admin.as_ref().unwrap();
    // 새 판본이 도구를 철회했다: 옛 카드의 승인 클릭은 받아들여지지 않는다(stale)
    let seen = pending_card(admin);
    let target = if seen.is_null() { &card } else { &seen };
    let (status, body) = decide(admin, target, true);
    assert!(
        status == 409 && body.contains("stale"),
        "옛 승인 클릭은 stale이어야 한다: {status} {body}"
    );
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.delete' and state in ('started', 'done')"),
        "0",
        "도구 실행은 0회"
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
    wait_until("지난 예정 시각을 접수하지 못했어요", || {
        c.q(&missed).parse::<u32>().unwrap_or(0) >= 4
    });
    let ended = format!(
        "select count(*) from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}' and state in ('dead', 'succeeded')"
    );
    wait_until(
        "옛 판본의 예약 실행이 끝나지 않았어요",
        || c.q(&ended) == c.q(&missed),
    );
    assert_eq!(
        c.q(&format!(
            "select string_agg(distinct state || '/' || coalesce(error, ''), ',') from {{s}}.bot_runs where trigger = 'morning' and revision = '{r1}'"
        )),
        "dead/denied",
        "직접 도구 실행은 denied로 끝난다"
    );
    assert_eq!(
        c.q("select count(*) from {s}.bot_calls where tool = 'agenda.list' and state in ('started', 'done')"),
        "0",
        "도구 실행은 0회"
    );
}
