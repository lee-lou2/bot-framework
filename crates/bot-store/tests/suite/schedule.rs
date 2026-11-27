use crate::support::*;
use bot_core::{command::*, id::*, port::*};
use std::time::{Duration, UNIX_EPOCH};

fn tool(name: &str) -> Target {
    Target::Tool {
        agent: AgentId("main".into()),
        tool: ToolId(name.into()),
        args: JsonPayload("{}".into()),
    }
}

fn at(ms: i64) -> std::time::SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

fn occ(trigger: &str, ms: i64) -> Occurrence {
    Occurrence {
        profile: ProfileId("personal".into()),
        agent: AgentId("main".into()),
        trigger: trigger.into(),
        scheduled_at: at(ms),
    }
}

async fn run_count(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_runs").await
}

#[tokio::test]
async fn sync_splits_segments_at_the_effective_time() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let set1 = schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("morning", "* * * * *", false)],
    );
    db.publish_schedule(set1.clone()).await.unwrap();
    db.publish_schedule(set1).await.unwrap(); // 같은 정의의 재게시는 아무것도 바꾸지 않는다
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_schedule_segments")
            .await,
        1
    );

    // 정의가 바뀌면 효력 시각에 구간이 나뉜다
    let mut changed = trigger_spec("morning", "0 9 * * *", false);
    changed.timezone = "Asia/Seoul".into();
    db.backdate_schedule().await;
    let set2 = schedule_set("personal", "r2", vec![changed]);
    db.publish_schedule(set2).await.unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_schedule_segments")
            .await,
        2
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_schedule_segments where until_ms is null")
            .await,
        1
    );
    let snapshot = db.store.snapshot().await.unwrap();
    assert_eq!(snapshot.triggers.len(), 1);
    let segments = &snapshot.triggers[0].segments;
    assert_eq!(segments.len(), 2, "커서 이후에 걸친 구간 둘");
    assert_eq!(
        (
            segments[0].spec.cron.as_str(),
            segments[0].revision.0.as_str()
        ),
        ("* * * * *", "r1")
    );
    assert_eq!(
        (
            segments[1].spec.cron.as_str(),
            segments[1].spec.timezone.as_str()
        ),
        ("0 9 * * *", "Asia/Seoul")
    );
    assert_eq!(segments[0].until, Some(segments[1].from), "구간은 이어진다");
    assert!(segments[1].until.is_none());

    // 정의에서 빠진 트리거는 그 시각에 닫힌다
    db.publish_schedule(schedule_set("personal", "r3", vec![]))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_schedule_segments where until_ms is null")
            .await,
        0
    );
    let bad = db
        .publish_schedule(schedule_set(
            "personal",
            "r4",
            vec![trigger_spec("x", "61 * * * *", false)],
        ))
        .await;
    assert!(matches!(bad, Err(Error::Invalid)));
}

#[tokio::test]
async fn accepts_each_occurrence_once_and_advances_the_cursor() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("morning", "* * * * *", false)],
    ))
    .await
    .unwrap();
    db.backdate_schedule().await;
    let now = db.now_ms().await;
    let cursor0 = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    let (t1, t2) = (cursor0 + 60_000, cursor0 + 120_000);
    let cmd = |ms| {
        occurrence_accept(
            "personal",
            "morning",
            ms,
            tool("agenda.list"),
            &["calendar.read"],
        )
    };

    let first = db
        .store
        .accept_occurrence(occ("morning", t1), cmd(t1))
        .await
        .unwrap();
    assert_eq!(
        db.store
            .accept_occurrence(occ("morning", t1), cmd(t1))
            .await
            .unwrap(),
        first,
        "같은 발생은 같은 run"
    );
    let second = db
        .store
        .accept_occurrence(occ("morning", t2), cmd(t2))
        .await
        .unwrap();
    assert_ne!(first, second);
    assert_eq!(run_count(&db).await, 2);
    assert_eq!(
        db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await,
        t2
    );
    assert_eq!(
        db.scalar::<String>(&format!(
            "select trigger || ':' || principal from bot_runs where id = '{}'",
            first.0
        ))
        .await,
        "morning:service:morning"
    );

    // 커서 뒤(이미 지난 것으로 처리된 구간)·아직 오지 않은 시각·정의가 덮지 않는 시각은 받지 않는다
    let behind = db
        .store
        .accept_occurrence(occ("morning", t1 - 60_000), cmd(t1 - 60_000))
        .await;
    assert!(matches!(behind, Err(Error::Stale)));
    let future = db
        .store
        .accept_occurrence(occ("morning", now + 600_000), cmd(now + 600_000))
        .await;
    assert!(matches!(future, Err(Error::Invalid)));
    let early = db
        .store
        .accept_occurrence(
            occ("morning", cursor0 - 7_200_000),
            cmd(cursor0 - 7_200_000),
        )
        .await;
    assert!(early.is_err());
    let unknown = db
        .store
        .accept_occurrence(
            occ("other", t2 + 60_000),
            occurrence_accept(
                "personal",
                "other",
                t2 + 60_000,
                tool("agenda.list"),
                &["calendar.read"],
            ),
        )
        .await;
    assert!(matches!(unknown, Err(Error::Missing)));

    // 저장된 정의와 다른 명령(다른 주체·권한·대상)은 받지 않는다
    let t3 = t2 + 60_000;
    let mut forged = cmd(t3);
    forged.event.principal = PrincipalId("local:owner".into());
    assert!(matches!(
        db.store.accept_occurrence(occ("morning", t3), forged).await,
        Err(Error::Denied)
    ));
    let broad = occurrence_accept(
        "personal",
        "morning",
        t3,
        tool("agenda.list"),
        &["calendar.read", "calendar.delete"],
    );
    assert!(matches!(
        db.store.accept_occurrence(occ("morning", t3), broad).await,
        Err(Error::Denied)
    ));
    let other_tool = occurrence_accept(
        "personal",
        "morning",
        t3,
        tool("agenda.delete"),
        &["calendar.read"],
    );
    assert!(matches!(
        db.store
            .accept_occurrence(occ("morning", t3), other_tool)
            .await,
        Err(Error::Denied)
    ));
    assert_eq!(
        run_count(&db).await,
        2,
        "거절된 명령은 아무것도 남기지 않는다"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_schedulers_converge_on_the_same_occurrences() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("morning", "* * * * *", false)],
    ))
    .await
    .unwrap();
    db.backdate_schedule().await;
    let cursor0 = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    let times: Vec<i64> = (1..=6).map(|n| cursor0 + n * 60_000).collect();
    let mut tasks = Vec::new();
    for n in 0..2 {
        let store = db.peer(&format!("scheduler-{n}")).await;
        let times = times.clone();
        tasks.push(tokio::spawn(async move {
            let mut runs = Vec::new();
            for ms in times {
                let cmd = occurrence_accept(
                    "personal",
                    "morning",
                    ms,
                    Target::Tool {
                        agent: AgentId("main".into()),
                        tool: ToolId("agenda.list".into()),
                        args: JsonPayload("{}".into()),
                    },
                    &["calendar.read"],
                );
                runs.push(
                    store
                        .accept_occurrence(occ("morning", ms), cmd)
                        .await
                        .unwrap(),
                );
            }
            runs
        }));
    }
    let (a, b) = (
        tasks.remove(0).await.unwrap(),
        tasks.remove(0).await.unwrap(),
    );
    assert_eq!(a, b, "두 스케줄러가 같은 발생에 같은 run을 얻는다");
    assert_eq!(run_count(&db).await, 6);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_occurrences")
            .await,
        6
    );
    assert_eq!(
        db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await,
        times[5]
    );
}

#[tokio::test]
async fn beat_coalesces_pending_runs_and_keeps_the_last_signal() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("health", "* * * * *", true)],
    ))
    .await
    .unwrap();
    db.backdate_schedule().await;
    let cursor0 = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    let cmd = |ms| {
        occurrence_accept(
            "personal",
            "health",
            ms,
            tool("agenda.list"),
            &["calendar.read"],
        )
    };
    let t = |n: i64| cursor0 + n * 60_000;

    let a = db
        .store
        .accept_occurrence(occ("health", t(1)), cmd(t(1)))
        .await
        .unwrap();
    // 처리 전(pending)에 더 들어온 신호는 같은 run에 합쳐진다
    assert_eq!(
        db.store
            .accept_occurrence(occ("health", t(2)), cmd(t(2)))
            .await
            .unwrap(),
        a
    );
    assert_eq!(run_count(&db).await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_occurrences where coalesced")
            .await,
        1
    );

    // 처리 중에 들어온 신호는 마지막 갱신을 놓치지 않도록 다음 run 하나가 된다. 앞의 run이 끝나기 전에는 집히지 않는다
    let running = db.store.claim().await.unwrap().unwrap();
    assert_eq!(running.attempt.run, a);
    let b = db
        .store
        .accept_occurrence(occ("health", t(3)), cmd(t(3)))
        .await
        .unwrap();
    assert_ne!(b, a);
    assert!(
        db.store.claim().await.unwrap().is_none(),
        "같은 beat는 동시에 하나만 실행한다"
    );
    assert_eq!(
        db.store
            .accept_occurrence(occ("health", t(4)), cmd(t(4)))
            .await
            .unwrap(),
        b,
        "기다리는 run에 다시 합쳐진다"
    );
    assert_eq!(run_count(&db).await, 2);

    db.store.finish(running.attempt, done("ok")).await.unwrap();
    let next = db.store.claim().await.unwrap().unwrap();
    assert_eq!(next.attempt.run, b);
    assert_eq!(
        db.scalar::<i64>("select cursor_ms from bot_schedule_cursors")
            .await,
        t(4)
    );
}

#[tokio::test]
async fn expired_beat_and_unrelated_work_both_progress() {
    let Some(db) = TestDb::with(bot_store::Config {
        max_attempts: 2,
        ..Default::default()
    })
    .await
    else {
        return;
    };
    let peer = db.peer("other").await;
    let spec = trigger_spec("health", "* * * * *", true);
    db.publish_schedule(schedule_set("personal", "r1", vec![spec.clone()]))
        .await
        .unwrap();
    db.backdate_schedule().await;
    let cursor = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    let beat = |ms| {
        db.store.accept_occurrence(
            occ("health", ms),
            occurrence_accept(
                "personal",
                "health",
                ms,
                spec.target.clone(),
                &["calendar.read"],
            ),
        )
    };
    let first = beat(cursor + 60_000).await.unwrap();
    let original = db.store.claim().await.unwrap().unwrap();
    assert_eq!(original.attempt.run, first);
    let next = beat(cursor + 120_000).await.unwrap();
    // 후속 beat는 리스 만료보다 먼저 준비됐다. 시간 순서만 보면 선행 beat를 앞지른다.
    db.sql("update bot_runs set not_before_ms = 1 where state = 'ready'")
        .await;
    db.expire_lease(&first).await;
    let ordinary = db
        .store
        .accept(accept("work", "ordinary", "일반 질문"))
        .await
        .unwrap();

    let (a, b) = tokio::join!(db.store.claim(), peer.claim());
    let (a, b) = (a.unwrap().unwrap(), b.unwrap().unwrap());
    let (reclaimed, healthy) = if a.attempt.run == first {
        (a, b)
    } else {
        (b, a)
    };
    assert_eq!(reclaimed.attempt.run, first);
    assert_eq!(
        reclaimed.attempt.generation.0,
        original.attempt.generation.0 + 1
    );
    assert_eq!(healthy.attempt.run, ordinary);
    db.store
        .finish(healthy.attempt, done("완료"))
        .await
        .unwrap();
    assert!(peer.claim().await.unwrap().is_none());
    assert_eq!(
        db.scalar::<i64>(
            "select count(*) from bot_runs where serial_key is not null and state = 'leased'"
        )
        .await,
        1
    );

    // 선행 beat가 마지막 시도를 소진하면 같은 키의 후속 beat를 바로 점유할 수 있다.
    db.expire_lease(&first).await;
    assert_eq!(peer.claim().await.unwrap().unwrap().attempt.run, next);
    assert_eq!(state_of_run(&db, &first).await, "dead");
}

#[tokio::test]
async fn ordinary_triggers_never_coalesce() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("morning", "* * * * *", false)],
    ))
    .await
    .unwrap();
    db.backdate_schedule().await;
    let cursor0 = db
        .scalar::<i64>("select cursor_ms from bot_schedule_cursors")
        .await;
    for n in 1..=3 {
        let ms = cursor0 + n * 60_000;
        let cmd = occurrence_accept(
            "personal",
            "morning",
            ms,
            tool("agenda.list"),
            &["calendar.read"],
        );
        db.store
            .accept_occurrence(occ("morning", ms), cmd)
            .await
            .unwrap();
    }
    assert_eq!(run_count(&db).await, 3);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_occurrences where coalesced")
            .await,
        0
    );
}

#[tokio::test]
async fn occurrence_at_a_segment_boundary_belongs_to_the_old_segment() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.publish_schedule(schedule_set(
        "personal",
        "r1",
        vec![trigger_spec("morning", "* * * * *", false)],
    ))
    .await
    .unwrap();
    db.backdate_schedule().await;
    let mut next = trigger_spec("morning", "*/5 * * * *", false);
    next.scopes = scopes(&["calendar.read", "calendar.write"]);
    let set = schedule_set("personal", "r2", vec![next]);
    db.publish_schedule(set).await.unwrap();
    let boundary = db
        .scalar::<i64>("select until_ms from bot_schedule_segments where until_ms is not null")
        .await;
    assert!(
        boundary >= db.now_ms().await - 5_000,
        "효력 시각은 DB 시각 이상이다"
    );
    // 경계 시각은 (from, until] 규칙에 따라 옛 구간의 정의로만 접수된다
    db.sql("update bot_schedule_cursors set cursor_ms = cursor_ms - 7200000")
        .await;
    let old = occurrence_accept(
        "personal",
        "morning",
        boundary,
        tool("agenda.list"),
        &["calendar.read"],
    );
    // 옛 구간의 판본은 r1이고 새 구간의 판본은 r2다
    let accepted = db
        .store
        .accept_occurrence(occ("morning", boundary), old)
        .await;
    assert!(accepted.is_ok(), "{accepted:?}");
    let revision = db.scalar::<String>("select revision from bot_runs").await;
    assert_eq!(revision, "r1");
}

#[tokio::test]
async fn two_publications_in_one_db_millisecond_keep_only_the_latest_definition() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.sql("create or replace function bot_now_ms() returns bigint language sql volatile as 'select 1900000000000::bigint'").await;
    for revision in ["r1", "r2"] {
        db.publish_schedule(schedule_set(
            "personal",
            revision,
            vec![trigger_spec("morning", "* * * * *", false)],
        ))
        .await
        .unwrap();
    }
    let state = db.store.snapshot().await.unwrap();
    assert_eq!(state.triggers.len(), 1);
    assert_eq!(state.triggers[0].segments.len(), 1);
    assert_eq!(state.triggers[0].segments[0].revision.0, "r2");
    db.publish_schedule(schedule_set("personal", "r3", vec![]))
        .await
        .unwrap();
    assert!(db.store.snapshot().await.unwrap().triggers.is_empty());
}
