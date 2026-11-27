//! 정적 예약(cron 트리거) 정의는 활성 판본과 같은 커밋으로 게시된다(실제 프로세스·팀 시험 DB): 어느 역할이 게시하든 올라가고,
//! 실패한 시작은 바꾸지 않으며, 새 판본에서 사라진 트리거는 그 시각에 닫히되 전환 전 발생은 따라잡는다.
use crate::process_support::*;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// 예제 프로필에서 `morning` 트리거만 뺀 복사본
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
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // gateway만 띄워도(예약 역할 없이) 첫 게시가 예약 정의를 같이 올린다
    let env = env_for(&examples(), &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();
    assert_eq!((open().as_str(), closed().as_str()), ("1", "0"));

    // 실패한 시작(소켓 사용 중)은 판본도 예약 정의도 바꾸지 않는다
    let env_changed = env_for(&changed, &url, &socket, &[]);
    let busy = bot(
        &refs(&env_changed),
        &["serve", "--role", "gateway", "--activate"],
    );
    assert_eq!(busy.status.code(), Some(1), "{}", text(&busy));
    assert_eq!(schema.active(), operating);
    assert_eq!((open().as_str(), closed().as_str()), ("1", "0"));

    // 트리거를 없앤 새 판본을 활성화하면 같은 커밋으로 그 트리거의 구간이 닫힌다
    drop(server);
    let _next = start_gateway(&refs(&env_changed), &["--activate"], &socket);
    wait_for("새 판본이 게시되지 않았어요", || {
        !schema.active().is_empty() && schema.active() != operating
    });
    assert_eq!((open().as_str(), closed().as_str()), ("0", "1"));
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_schedule_segments where trigger = 'morning' and until_ms > from_ms"
        ),
        "1",
        "닫은 구간은 비어 있지 않다"
    );
}

#[test]
fn the_first_activate_after_an_upgrade_replaces_an_older_revision_and_publishes_the_schedule() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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
    // 새 코드가 게시한 판본을 기억해 두고, 옛 코드가 같은 프로필로 게시한 상태(다른 판본·예약 정의 없음)로 되돌린다
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let current = schema.active();
    drop(server);
    assert_eq!(
        schema.query("update {s}.bot_active set revision = 'legacy-revision'"),
        "UPDATE 1"
    );
    schema.query("delete from {s}.bot_schedule_cursors");
    schema.query("delete from {s}.bot_schedule_segments");
    assert_eq!(open(), "0");
    // 같은 프로필이어도 판본이 달라 합류가 아니다: --activate 없이는 거절하고 아무것도 쓰지 않는다
    let refused = bot(&refs(&env), &["serve", "--role", "gateway,scheduler"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), "legacy-revision");
    assert_eq!(open(), "0");
    // --activate는 한 번 교체하고 같은 커밋으로 예약 정의를 올린다
    let _next = start_roles(&refs(&env), "gateway,scheduler", &["--activate"], &socket);
    wait_for("--activate가 판본을 교체하지 않았어요", || {
        schema.active() == current
    });
    assert_eq!(open(), "1");
}

#[test]
fn a_deleted_trigger_still_catches_up_the_occurrences_before_the_switch_and_none_after() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = workdir();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let changed = without_trigger(dir.path());
    // 원래 판본으로 시작해 구간을 올린다
    let env = env_for(&examples(), &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();
    drop(server);
    // 이 구간이 5분 전부터 돌았던 것처럼 시각을 앞당긴다(스케줄러는 DB 시각만 쓰므로 기다림 없이 따라잡기를 본다)
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

    // 트리거를 없앤 새 판본을 활성화한다: 구간이 닫히고 커서는 남는다
    let env_changed = env_for(&changed, &url, &socket, &[]);
    let _next = start_gateway(&refs(&env_changed), &["--activate"], &socket);
    wait_for("새 판본이 게시되지 않았어요", || {
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
        "닫힌 구간의 커서는 남는다"
    );

    // 예약 역할은 새 판본(트리거 없음)으로 합류해도 전환 전 발생을 따라잡는다
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
        "전환 전 발생을 따라잡지 못했어요: {}",
        caught_up()
    );
    // 전환 이후의 발생은 접수하지 않는다
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
        "삭제된 트리거는 다시 열리지 않는다"
    );
}
