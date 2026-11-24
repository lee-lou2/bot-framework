//! 복원 대상은 이번에 게시·합류할 집합의 프로필 ID가 고정한 판본이다(실제 프로세스·팀 시험 DB·srt·python3).
//! - 아무 작업도 참조하지 않는 옛 판본은 DB의 활성 판본이라는 이유만으로 필수가 아니다: 옛 번들 없이도 새 판본으로 재배포된다.
//! - 집합에서 통째로 빠진 프로필(번들이 없는 옛 판본의 dead letter·미소진 구간이 남아 있다)은 다른 프로필의 재배포·합류를 막지 않고, 그 프로필의 작업은 되살아나지 않는다.
//! - 그 프로필을 다시 등록하면 그때의 살아 있는 참조로 다시 계산해 필요한 옛 번들이 없으면 게시 전에 실패하고 활성 집합은 그대로이며, 정확한 원본을 보충하면 통과한다.
//!
//! 팀 시험 DB·`srt`·`python3`가 없으면 건너뛴다.
mod process_support;
mod restore_support;
use process_support::*;
use restore_support::*;
use std::path::Path;
use std::time::Duration;

/// `morning` 트리거를 없앤 판본(`personal`의 옛 판본이 정적 구간으로 참조되지 않게 한다).
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
    // 이전 버전이 보관하지 않은 상태 + 아무 작업도 옛 판본을 참조하지 않는다(실행·카드·구간·예약 0)
    c.q("truncate {s}.bot_profile_bundles");
    assert_eq!(c.q("select count(*) from {s}.bot_runs"), "0");
    let _second = c.start(&v2, true, &[]);
    assert_ne!(c.schema.active(), active, "새 판본이 게시됐다");
    assert_ne!(c.profile_revision(), r1);
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles"),
        "1",
        "새 판본의 묶음만 보관됐다(옛 판본의 묶음을 만들어 내지 않는다)"
    );
}

#[test]
fn a_profile_that_left_the_set_never_blocks_the_others_and_coming_back_needs_its_exact_original() {
    let Some(c) = Cluster::new() else {
        return;
    };
    // 둘 다 있던 집합(S0)과, personal만 새 판본으로 올린 집합(A2), second를 새 판본으로 다시 등록한 집합(S1)
    let s0 = c.profiles_with_second("s0", without_trigger, "두 번째 프로필의 옛 지침.\n");
    let a2 = c.profiles("a2", without_trigger_and_changed);
    let s1 = c.profiles_with_second(
        "s1",
        without_trigger_and_changed,
        "두 번째 프로필의 새 지침.\n",
    );
    let first = c.start(&s0, false, &[]);
    let second_old = c.q("select revision from {s}.bot_active_profiles where profile = 'second'");
    assert_eq!(second_old.len(), 64);
    drop(first);
    // second가 쓰던 작업이 번들 없이 남아 있다(이전 버전이 보관하지 않았다): dead letter 하나와 닫혔지만 따라잡지 못한 정적 구간
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

    // 집합에서 빠진 프로필의 누락된 옛 번들은 personal의 재배포를 막지 않는다. 스케줄러·워커까지 띄워도 second의 실행은 0이다
    let mut running = c.start(&a2, true, &["worker", "scheduler"]);
    assert_eq!(
        c.q("select string_agg(profile, ',' order by profile) from {s}.bot_active_profiles"),
        "personal"
    );
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        second_rows(&c),
        before,
        "빠진 프로필의 dead letter는 되살아나지 않고 새 실행도 없다"
    );
    for table in ["bot_calls", "bot_approvals", "bot_outbox", "bot_deliveries"] {
        assert_eq!(
            c.q(&format!(
                "select count(*) from {{s}}.{table} where profile = 'second'"
            )),
            "0",
            "{table}: 모델·도구·승인·발송은 0"
        );
    }
    assert_eq!(
        c.q("select count(*) from {s}.bot_profile_bundles where profile = 'second'"),
        "0",
        "빠진 프로필의 묶음을 만들어 내거나 되살리지 않는다"
    );
    // 같은 집합에 합류한 워커·스케줄러(역할마다 따로 복원한다)도 끝나지 않고 돈다
    assert!(running.alive(), "빠진 프로필 때문에 합류한 역할이 멈췄어요");
    drop(running);
    let operating = c.schema.active();

    // second를 다시 등록한다(새 판본): 필요한 옛 번들이 없어 게시 전에 실패하고 활성 집합은 그대로다
    let env = c.env(&s1);
    let (code, out) = serve_that_must_stop(&refs(&env), &["--role", "gateway", "--activate"])
        .expect("필요한 옛 번들이 없으면 시작하지 않아야 한다");
    assert_eq!(code, Some(1), "{out}");
    assert!(
        out.contains(&format!(
            "프로필 second 판본 {second_old}: 보관한 묶음이 없어요"
        )),
        "{out}"
    );
    assert_eq!(c.schema.active(), operating, "현재 집합은 그대로다");
    assert_eq!(second_rows(&c), before);
    // 다른 유효한 폴더(second의 새 판본)는 자기 키로만 보관돼 누락을 채우지 못한다
    let new_folder = s1.join("second");
    let put = bot(
        &refs(&env),
        &["profile-bundle", "put", new_folder.to_str().unwrap()],
    );
    assert!(put.status.success(), "{}", text(&put));
    let (code, out) = serve_that_must_stop(&refs(&env), &["--role", "gateway", "--activate"])
        .expect("다른 판본은 누락을 채우지 못한다");
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains(&second_old), "{out}");
    // 정확한 원본(옛 지침의 폴더)을 보충하면 통과한다
    let original = s0.join("second");
    let put = bot(
        &refs(&env),
        &["profile-bundle", "put", original.to_str().unwrap()],
    );
    assert!(put.status.success(), "{}", text(&put));
    assert!(
        text(&put).contains(&format!("프로필 second 판본 {second_old}")),
        "{}",
        text(&put)
    );
    let _back = c.start(&s1, true, &[]);
    assert_eq!(
        c.q("select string_agg(profile, ',' order by profile) from {s}.bot_active_profiles"),
        "personal,second"
    );
    // 다시 등록해도 이미 끝난 작업은 되살아나지 않는다
    assert_eq!(second_rows(&c), before);
}
