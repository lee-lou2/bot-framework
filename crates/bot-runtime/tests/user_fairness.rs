//! 사용자 예약의 «앞줄 막힘»(실제 PG 저장소 + 실제 스케줄러, 등록부만 시험용):
//! 등록부 장애로 발화도 닫기도 못 하는(보류) 예약이 점검 쪽을 채워도, 그 뒤의 다른 소유자의 예약은 **한 점검 주기 안에** 발화한다.
//! - 점검 주기 = 스케줄러 `run` 루프처럼 «남았다(backlog)»는 동안 쉬지 않고 이어서 점검하는 한 바퀴. 쪽은 마지막으로 읽은 `(다음 확인 시각, 프로필, ID)` 다음부터 이어 읽는다(keyset).
//! - 보류 예약은 발생 시각·커서·다음 확인 시각을 그대로 두고(발생을 잃지 않는다), 한 주기에 한 번만 다시 본다(점검마다 같은 쪽을 다시 읽지 않는다).
//!
//! 예약 행은 SQL로 넣는다(만드는 쪽은 다른 시험이 본다). 모델 호출은 없다.

mod support;

use bot_broker::testing::{FakeCatalog, snapshot};
use bot_core::{
    id::{BindingId, ProfileId},
    port::Error,
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{Conversation, ConversationKind},
};
use bot_runtime::{Scheduler, SchedulerConfig, SchedulerDeps};
use std::sync::{Arc, Mutex};
use support::*;

/// 보류가 걸린 예약의 소유자 넷(소유자마다 25개)과 막히지 않는 예약의 소유자.
const STUCK_OWNERS: [&str; 4] = ["local:o1", "local:o2", "local:o3", "local:o4"];
const PER_OWNER: usize = 25;
const FREE: &str = "local:free";

/// 정해 둔 답을 주고 물은 횟수를 세는 등록부.
struct Routes {
    answer: Mutex<Result<DeliveryTarget, Error>>,
    asked: Mutex<usize>,
}

impl Routes {
    fn new(answer: Result<DeliveryTarget, Error>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: Mutex::new(0),
        })
    }
    fn now_says(&self, answer: Result<DeliveryTarget, Error>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn asked(&self) -> usize {
        *self.asked.lock().unwrap()
    }
}

impl ScheduleRoutes for Routes {
    fn resolve(&self, _: &ProfileId, _: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        *self.asked.lock().unwrap() += 1;
        match &*self.answer.lock().unwrap() {
            Ok(found) => Ok(found.clone()),
            Err(_) => Err(Error::Unavailable),
        }
    }
}

fn channel(id: &str) -> DeliveryTarget {
    DeliveryTarget {
        binding: BindingId("slack".into()),
        conversation: Conversation {
            kind: ConversationKind::Channel,
            id: id.into(),
            thread: Some("t1".into()),
        },
    }
}

/// 소유자들이 모두 멤버고 원래 대화 바인딩(desk)이 살아 있는 판본.
fn live_profile() -> Arc<bot_core::profile::Snapshot> {
    let mut builder = snapshot("personal", "r1")
        .role("owner", &["run", "calendar.read"])
        .policy_scopes(&["run", "calendar.read"])
        .slack_dm_binding("desk", "main")
        .member(FREE, &["owner"]);
    for owner in STUCK_OWNERS {
        builder = builder.member(owner, &["owner"]);
    }
    builder.build()
}

fn scheduler(w: &World, routes: &Arc<Routes>, batch: usize) -> Scheduler {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(live_profile());
    Scheduler::new(
        SchedulerDeps {
            schedule: w.pg.store.clone(),
            user: w.pg.store.clone(),
            catalog,
            tracer: Arc::new(bot_core::trace::NoopTracer),
        },
        SchedulerConfig {
            interval: std::time::Duration::from_millis(10),
            batch,
        },
    )
    .with_routes(routes.clone())
}

/// 보류될 예약 100개(소유자 넷 × 25)와, 그보다 늦게 도래했지만 등록부가 필요 없는(원래 대화로 답하는) 다른 소유자의 예약 하나.
/// 보류될 예약은 명시 목적지가 있고 모두 더 오래전에 도래해서 점검 쪽의 앞을 채운다.
async fn plant_blockers_and_one_free(w: &World) {
    let origin = w.accept("origin", "예약을 만든 대화").await;
    for (k, owner) in STUCK_OWNERS.iter().enumerate() {
        w.pg.sql(&format!(
            "with t as (select bot_now_ms() as now)
             insert into bot_user_schedules
               (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
                ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
             select 'personal', 's-{k}-' || lpad(i::text, 2, '0'), '{owner}', 'main', '{}', 'c-s-{k}-' || i, 'desk', 'direct', 'owner',
                    '{{run,calendar.read}}', 'slack', 'channel', 'T1/C9', 't1', now - 120000 - {k} * 1000 - i, '주간 보고를 보낸다',
                    now - 3600000, now - 120000 - {k} * 1000 - i
             from t, generate_series(1, {PER_OWNER}) as i",
            origin.0
        ))
        .await;
    }
    w.pg.sql(&format!(
        "with t as (select bot_now_ms() as now)
         insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, at_ms, instruction, cursor_ms, next_ms)
         select 'personal', 'z-free', '{FREE}', 'main', '{}', 'c-z-free', 'desk', 'direct', 'free',
                '{{run,calendar.read}}', now - 60000, '다섯째 소유자의 예약', now - 3600000, now - 60000
         from t",
        origin.0
    ))
    .await;
}

/// 점검 주기 하나: 스케줄러 `run` 루프처럼 «남았다»는 동안 쉬지 않고 이어서 점검한다. (발화 수, 점검 횟수)
async fn cycle(scheduler: &Scheduler) -> (usize, usize) {
    let (mut fired, mut ticks) = (0, 0);
    loop {
        let tick = scheduler.tick_users().await.unwrap();
        fired += tick.fired;
        ticks += 1;
        assert!(ticks <= 1000, "점검 주기가 끝나지 않는다");
        if !tick.backlog {
            return (fired, ticks);
        }
    }
}

async fn fired_ids(w: &World) -> Vec<String> {
    sqlx::query_scalar("select schedule from bot_user_occurrences order by schedule")
        .fetch_all(w.pg.store.pool())
        .await
        .unwrap()
}

/// 보류 예약의 발생 시각 · 커서 · 다음 확인 시각을 한 값으로(바뀌면 달라진다).
async fn stuck_fingerprint(w: &World) -> String {
    w.pg.scalar::<String>(
        "select coalesce(md5(string_agg(id || ':' || at_ms || ':' || cursor_ms || ':' || next_ms || ':' || state, ',' order by id)), '')
         from bot_user_schedules where id like 's-%'",
    )
    .await
}

#[tokio::test]
async fn held_schedules_filling_the_first_pages_do_not_block_another_owners_schedule_within_one_cycle()
 {
    for batch in [100, 1, 3] {
        let Some(w) = world().await else {
            return;
        };
        plant_blockers_and_one_free(&w).await;
        let held = stuck_fingerprint(&w).await;
        let runs = w.pg.scalar::<i64>("select count(*) from bot_runs").await;
        let routes = Routes::new(Err(Error::Unavailable));
        let scheduler = scheduler(&w, &routes, batch);

        let (fired, ticks) = cycle(&scheduler).await;
        assert_eq!(
            fired, 1,
            "배치 {batch}: 막히지 않는 예약이 첫 점검 주기 안에 발화한다"
        );
        assert_eq!(fired_ids(&w).await, ["z-free"], "배치 {batch}");
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_runs").await,
            runs + 1,
            "배치 {batch}: 새 run은 하나뿐이다"
        );
        // 보류 예약은 한 주기에 한 번씩만 본다(점검마다 같은 쪽을 다시 읽지 않는다) — 쪽 수만큼 점검이 이어졌다
        assert_eq!(
            routes.asked(),
            100,
            "배치 {batch}: 보류 예약을 주기당 한 번씩 확인"
        );
        // 이 바퀴의 끝 행을 읽은 쪽이 마지막이다(빈 쪽을 따로 읽지 않는다): 101개 → 올림(101 / 배치)번
        assert_eq!(
            ticks,
            101usize.div_ceil(batch),
            "배치 {batch}: 점검 횟수 = 쪽 수"
        );
        // 발생을 잃지 않는다: 발생 시각·커서·다음 확인 시각·상태 그대로, 닫지도 발화하지도 않았다
        assert_eq!(stuck_fingerprint(&w).await, held, "배치 {batch}");
        assert_eq!(
            w.pg.scalar::<i64>(
                "select count(*) from bot_user_schedules where state = 'active' and id like 's-%'"
            )
            .await,
            100,
            "배치 {batch}"
        );

        // 다음 주기도 첫 쪽부터 다시 본다(한 바퀴를 마치면 처음부터): 보류 예약을 한 번씩 더 확인할 뿐 새로 발화하는 것은 없다
        let (again, _) = cycle(&scheduler).await;
        assert_eq!(again, 0, "배치 {batch}");
        assert_eq!(routes.asked(), 200, "배치 {batch}: 두 번째 주기도 한 번씩");

        // 등록부가 돌아오면 보류 예약이 **원래 예정 시각**으로 모두 발화한다
        routes.now_says(Ok(channel("T1/C9")));
        let (recovered, _) = cycle(&scheduler).await;
        assert_eq!(recovered, 100, "배치 {batch}");
        assert_eq!(
            w.pg.scalar::<i64>(
                "select count(*) from bot_user_occurrences o join bot_user_schedules s on s.id = o.schedule and s.profile = o.profile
                 where o.at_ms = s.at_ms and s.id like 's-%'"
            )
            .await,
            100,
            "배치 {batch}: 원래 예정 시각의 발생이다"
        );
    }
}

#[tokio::test]
async fn every_due_schedule_fires_in_one_cycle_when_the_due_set_spans_several_pages() {
    let Some(w) = world().await else {
        return;
    };
    plant_blockers_and_one_free(&w).await;
    // 등록부가 멀쩡하면 보류가 없다: 101개가 3개짜리 쪽 34개에 걸쳐 모두 발화한다
    let routes = Routes::new(Ok(channel("T1/C9")));
    let (fired, ticks) = cycle(&scheduler(&w, &routes, 3)).await;
    assert_eq!(fired, 101);
    assert_eq!(ticks, 34);
    assert_eq!(fired_ids(&w).await.len(), 101);
    // 다 처리했으면 다음 주기에는 할 일이 없다
    assert_eq!(cycle(&scheduler(&w, &routes, 3)).await, (0, 1));
}

/// 예약 하나를 SQL로 넣는다. `dest`가 있으면 명시 목적지(등록부가 필요 — 보류될 수 있다), 없으면 원래 대화로 답한다. `ago`ms 전에 도래했다.
async fn plant_row(w: &World, origin: &str, id: &str, owner: &str, dest: bool, ago: i64) {
    let (binding, kind, dest_id, thread) = if dest {
        ("'slack'", "'channel'", "'T1/C9'", "'t1'")
    } else {
        ("null", "null", "null", "null")
    };
    w.pg.sql(&format!(
        "with t as (select bot_now_ms() as now)
         insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
         select 'personal', '{id}', '{owner}', 'main', '{origin}', 'c-{id}', 'desk', 'direct', 'owner',
                '{{run,calendar.read}}', {binding}, {kind}, {dest_id}, {thread}, now - {ago}, '확인', now - 3600000, now - {ago}
         from t"
    ))
    .await;
}

async fn held_fired(w: &World) -> i64 {
    w.pg.scalar::<i64>("select count(*) from bot_user_occurrences where schedule like 'h-%'")
        .await
}

#[tokio::test]
async fn a_recovered_held_schedule_fires_within_a_bounded_number_of_checks_even_while_new_schedules_keep_arriving()
 {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept("origin", "예약을 만든 대화").await.0;
    // 보류될 예약 셋(목적지가 있다)과 막히지 않는 예약 하나. 바퀴를 시작할 때 이 넷이 «이번 바퀴가 읽을 것»이다.
    for (i, id) in ["h-1", "h-2", "h-3"].into_iter().enumerate() {
        plant_row(&w, &origin, id, STUCK_OWNERS[0], true, 120_000 - i as i64).await;
    }
    plant_row(&w, &origin, "z-free", FREE, false, 60_000).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    // 첫 점검: 첫 쪽(h-1)을 보고 보류한다. 바퀴는 아직 끝나지 않았다
    let first = scheduler.tick_users().await.unwrap();
    assert!(first.backlog && first.fired == 0);
    // 등록부가 돌아왔다. 이제부터 점검마다 새 예약이 한 건씩 들어와 쪽이 늘 가득 찬다(바퀴가 끝나지 않으면 h-1은 영영 못 본다)
    routes.now_says(Ok(channel("T1/C9")));
    let mut ticks = 1;
    for n in 0.. {
        assert!(
            ticks <= 12,
            "유입 때문에 복구된 예약이 발화하지 못한다: {} / 3",
            held_fired(&w).await
        );
        plant_row(&w, &origin, &format!("n-{n:02}"), FREE, false, 1_000).await;
        scheduler.tick_users().await.unwrap();
        ticks += 1;
        if held_fired(&w).await == 3 {
            break;
        }
    }
    // 고정된 끝(z-free)까지 읽으면 바퀴가 끝나고 다음 바퀴가 처음(h-1)부터 본다: 넷 + 한 바퀴 안의 처음 쪽
    assert!(ticks <= 7, "점검 {ticks}번");
    // 새로 들어온 예약은 지금까지 읽은 바퀴에서 빠졌다가 다음 바퀴에 발화한다(잃지 않는다)
    let planted = ticks - 1;
    let mut more = 0;
    while w
        .pg
        .scalar::<i64>("select count(*) from bot_user_occurrences where schedule like 'n-%'")
        .await
        < planted
    {
        scheduler.tick_users().await.unwrap();
        more += 1;
        assert!(more < 40, "새 예약이 발화하지 못한다");
    }
}

/// 30분 밀린 매분 반복 예약 한 행(원래 대화로 답한다 — 등록부가 필요 없다).
async fn plant_recurring(w: &World, origin: &str, id: &str, owner: &str) {
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, cron, timezone, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{owner}', 'main', '{origin}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', '* * * * *', 'UTC', '매분 확인', bot_now_ms() - 1801000, bot_now_ms() - 1800000)"
    ))
    .await;
}

async fn occurrences_of(w: &World, id: &str) -> Vec<i64> {
    sqlx::query_scalar(&format!(
        "select at_ms from bot_user_occurrences where schedule = '{id}' order by at_ms"
    ))
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

/// 한 점검을 돌리고 이번 점검에서 새로 접수한 발생의 예약 ID(접수 순서)와 `backlog`를 돌려준다.
async fn one_tick(w: &World, scheduler: &Scheduler, seen: &mut usize) -> (Vec<String>, bool) {
    let tick = scheduler.tick_users().await.unwrap();
    let all: Vec<String> = sqlx::query_scalar(
        "select schedule from bot_user_occurrences order by created_ms, at_ms, schedule",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap();
    let fresh = all[*seen..].to_vec();
    *seen = all.len();
    (fresh, tick.backlog)
}

/// 밀린 정상 발생이 남았는가: 활성 예약 중 확인할 시각이 지난 것의 ID(보류 예약 제외). 매분 예약이 시험 도중 새 분에 들어서도 흔들리지 않게 2초 여유를 둔다.
async fn overdue_healthy(w: &World) -> Vec<String> {
    sqlx::query_scalar(
        "select id from bot_user_schedules where state = 'active' and next_ms <= bot_now_ms() - 2000 and dest_binding is null order by id",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap()
}

#[tokio::test]
async fn an_overdue_recurring_schedule_gets_one_turn_per_pass_and_the_others_fire_in_the_first_ticks()
 {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept("origin", "예약을 만든 대화").await.0;
    // 키가 가장 오래된 30분 밀린 매분 반복 예약, 등록부가 필요한 보류 예약, 다른 소유자의 한 번짜리. 배치 1.
    // 반복 예약은 발화할 때마다 다음 확인 시각이 앞으로 가지만 아직 다른 둘보다 오래됐다 — 같은 바퀴의 뒤쪽 쪽에 다시 나타난다.
    plant_recurring(&w, &origin, "r-minutely", STUCK_OWNERS[0]).await;
    plant_row(&w, &origin, "h-held", STUCK_OWNERS[0], true, 120_000).await;
    plant_row(&w, &origin, "z-free", FREE, false, 60_000).await;
    let runs = w.pg.scalar::<i64>("select count(*) from bot_runs").await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    let mut seen = 0;

    // 공정성은 점검 순서로 본다. 첫 바퀴(등록부 장애): 1) 반복 예약 발화 2) 다시 나타난 반복 예약은 건너뜀 3) 보류 예약은 건드리지 않음 4) 다른 소유자 예약 발화.
    // 바퀴가 끝나는 4번째 점검도 «밀린 발생이 남았다»를 전한다(쉬지 않는다).
    let mut order = Vec::new();
    for _ in 0..4 {
        let (fresh, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        order.push((fresh, backlog));
    }
    assert_eq!(
        order,
        [
            (vec!["r-minutely".to_owned()], true),
            (vec![], true),
            (vec![], true),
            (vec!["z-free".to_owned()], true),
        ]
    );
    assert!(occurrences_of(&w, "h-held").await.is_empty());
    assert_eq!(routes.asked(), 1, "보류 예약은 바퀴마다 한 번");

    // 등록부가 복구된 뒤 첫 바퀴: 반복 예약의 다음 발생 → 건너뜀 → 복구된 예약(3번째 점검 안에 발화한다)
    routes.now_says(Ok(channel("T1/C9")));
    let mut after = Vec::new();
    for _ in 0..3 {
        after.push(one_tick(&w, &scheduler, &mut seen).await.0);
    }
    assert_eq!(
        after,
        [
            vec!["r-minutely".to_owned()],
            vec![],
            vec!["h-held".to_owned()]
        ]
    );

    // 따라잡기는 쉬기 직전의 DB로 본다: 남은 발생을 쉬지 않고 이어서 모두 접수하고, 쉬는 순간 밀린 정상 발생이 0건이다
    let mut ticks = 0;
    loop {
        let (_, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        ticks += 1;
        assert!(ticks <= 100, "따라잡지 못한다");
        if !backlog {
            break;
        }
    }
    assert_eq!(
        overdue_healthy(&w).await,
        Vec::<String>::new(),
        "쉬기 직전 밀린 정상 발생은 0건이다"
    );
    let all = occurrences_of(&w, "r-minutely").await;
    assert!(
        all.len() >= 29,
        "밀린 서른 번 안팎을 모두 접수했다: {}",
        all.len()
    );
    assert!(
        all.windows(2).all(|pair| pair[1] - pair[0] == 60_000),
        "오래된 순으로 한 분씩, 빠지거나 겹치지 않는다"
    );
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_runs").await,
        runs + all.len() as i64 + 2,
        "발생마다 run 하나(반복 예약) + 보류였다가 복구된 예약 + 다른 소유자 예약"
    );
}

#[tokio::test]
async fn a_held_row_behind_an_overdue_recurring_schedule_does_not_cut_the_catch_up_short() {
    let Some(w) = world().await else {
        return;
    };
    let origin = w.accept("origin", "예약을 만든 대화").await.0;
    // 30분 밀린 매분 반복 예약 + 마지막에 등록부 보류 예약(끝까지 풀리지 않는다), 배치 1: 바퀴마다 반복 예약은 한 번씩만 발화하지만 «남음»이 쪽을 넘어도 사라지지 않아 쉬지 않고 이어 간다
    plant_recurring(&w, &origin, "r-minutely", STUCK_OWNERS[0]).await;
    plant_row(&w, &origin, "z-held", STUCK_OWNERS[0], true, 60_000).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let scheduler = scheduler(&w, &routes, 1);
    let mut seen = 0;
    let mut rests = 0;
    for tick in 1.. {
        assert!(
            tick <= 300,
            "따라잡지 못한다: {}",
            occurrences_of(&w, "r-minutely").await.len()
        );
        let (_, backlog) = one_tick(&w, &scheduler, &mut seen).await;
        if !backlog {
            rests = tick;
            break;
        }
    }
    // 쉬기 직전: 밀린 정상 발생 0건(보류 예약만 남았다), 반복 예약은 서른 번 안팎을 오래된 순으로 모두 접수했고 보류 예약은 그대로다
    assert_eq!(
        overdue_healthy(&w).await,
        Vec::<String>::new(),
        "{rests}번째 점검에서 쉰다"
    );
    let all = occurrences_of(&w, "r-minutely").await;
    assert!(
        all.len() >= 29,
        "{}번째 점검에서 쉬었는데 {}번만 접수했다",
        rests,
        all.len()
    );
    assert!(all.windows(2).all(|pair| pair[1] - pair[0] == 60_000));
    assert!(occurrences_of(&w, "z-held").await.is_empty());
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules where id = 'z-held'")
            .await,
        "active",
        "보류 예약은 닫히지도 발화하지도 않았다"
    );
}
