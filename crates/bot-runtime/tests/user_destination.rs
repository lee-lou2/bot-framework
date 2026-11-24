//! 사용자 예약의 명시 목적지를 **발화할 때** 현재 운영자 등록부로 다시 확인한다(실제 PG 저장소+실제 스케줄러, 등록부만 시험용):
//! - 봉인한 그대로의 같은 경로 → 발화한다. 발생·run·목적지 행이 한 커밋이고 목적지는 예약 정의에서 그 run에 고정된다.
//! - 등록부에 없다·다른 경로로 재지정됐다·값이 잘못됐다(`Denied`·`Invalid`) → 새 run 0건, 예약을 닫고 `schedule.close`로 감사한다(발화 감사 0).
//! - 일시 오류·등록부 미연결 → 이번 발생을 건드리지 않는다(run 0, 닫지 않음, 커서·다음 확인 시각 그대로). 복구하면 **같은 발생 시각**으로 발화한다(발생을 버리지 않는다).
//! - 원래 대화로 답하는 예약은 등록부를 묻지 않는다. 한 예약의 보류가 같은 점검의 다른 예약을 막지 않는다.
//!
//! 사용자 예약 행은 SQL로 넣는다(만드는 쪽은 다른 시험이 본다). 모델 호출은 없다.

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

/// 정해 둔 답을 주고 물은 수신 ID를 기록하는 등록부.
struct Routes {
    answer: Mutex<Result<DeliveryTarget, Error>>,
    asked: Mutex<Vec<String>>,
}

impl Routes {
    fn new(answer: Result<DeliveryTarget, Error>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            asked: Mutex::new(Vec::new()),
        })
    }
    fn now_says(&self, answer: Result<DeliveryTarget, Error>) {
        *self.answer.lock().unwrap() = answer;
    }
    fn asked(&self) -> usize {
        self.asked.lock().unwrap().len()
    }
}

impl ScheduleRoutes for Routes {
    fn resolve(&self, _: &ProfileId, target: &DeliveryTarget) -> Result<DeliveryTarget, Error> {
        self.asked
            .lock()
            .unwrap()
            .push(target.conversation.id.clone());
        match &*self.answer.lock().unwrap() {
            Ok(found) => Ok(found.clone()),
            Err(Error::Denied) => Err(Error::Denied),
            Err(Error::Invalid) => Err(Error::Invalid),
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

/// 소유자의 `desk` 대화에서 만든 예약이 가리키는 판본(멤버·바인딩·에이전트·권한이 모두 지금도 살아 있다).
fn live_profile() -> Arc<bot_core::profile::Snapshot> {
    snapshot("personal", "r1")
        .role("owner", &["run", "calendar.read"])
        .member(OWNER, &["owner"])
        .policy_scopes(&["run", "calendar.read"])
        .slack_dm_binding("desk", "main")
        .build()
}

/// 실제 스케줄러(등록부는 고른 것). 카탈로그는 위 판본이다.
fn scheduler(w: &World, routes: Option<Arc<Routes>>) -> Scheduler {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(live_profile());
    let scheduler = Scheduler::new(
        SchedulerDeps {
            schedule: w.pg.store.clone(),
            user: w.pg.store.clone(),
            catalog,
            tracer: Arc::new(bot_core::trace::NoopTracer),
        },
        SchedulerConfig {
            interval: std::time::Duration::from_millis(10),
            batch: 100,
        },
    );
    match routes {
        Some(routes) => scheduler.with_routes(routes),
        None => scheduler,
    }
}

/// 한 번 하는 예약 하나를 DB에 둔다: 두 분 전이 예정이라 지금 발화해야 한다. `destination`은 봉인한 수신자(없으면 원래 대화).
async fn plant(w: &World, id: &str, destination: Option<&str>) {
    let origin = w.accept(&format!("origin-{id}"), "예약을 만든 대화").await;
    let (binding, kind, dest) = match destination {
        Some(dest) => ("'slack'", "'channel'", format!("'{dest}'")),
        None => ("null", "null", "null".to_owned()),
    };
    let thread = if destination.is_some() {
        "'t1'"
    } else {
        "null"
    };
    w.pg.sql(&format!(
        "insert into bot_user_schedules
           (profile, id, owner, agent, origin_run, origin_call, origin_binding, origin_kind, origin_id,
            ceiling, dest_binding, dest_kind, dest_id, dest_thread, at_ms, instruction, cursor_ms, next_ms)
         values ('personal', '{id}', '{OWNER}', 'main', '{}', 'c-{id}', 'desk', 'direct', 'owner',
            '{{run,calendar.read}}', {binding}, {kind}, {dest}, {thread}, bot_now_ms() - 120000, '주간 보고를 보낸다',
            bot_now_ms() - 3600000, bot_now_ms() - 120000)",
        origin.0
    ))
    .await;
}

async fn runs(w: &World) -> i64 {
    w.pg.scalar::<i64>("select count(*) from bot_runs").await
}

async fn audit(w: &World, action: &str) -> i64 {
    w.pg.scalar::<i64>(&format!(
        "select count(*) from bot_audit where action = '{action}'"
    ))
    .await
}

#[tokio::test]
async fn a_destination_that_is_still_the_registered_one_fires_with_the_destination_fixed_on_the_new_run()
 {
    let Some(w) = world().await else {
        return;
    };
    plant(&w, "s1", Some("T1/C9")).await;
    let before = runs(&w).await;
    let routes = Routes::new(Ok(channel("T1/C9")));
    let tick = scheduler(&w, Some(routes.clone()))
        .tick_users()
        .await
        .unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    assert_eq!(routes.asked(), 1, "발화 전에 한 번 묻는다");
    // 발생·run·목적지 행이 같은 커밋이다
    assert_eq!(runs(&w).await, before + 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_user_occurrences")
            .await,
        1
    );
    assert_eq!(
        w.pg.scalar::<String>(
            "select dest_binding || '/' || dest_kind || '/' || dest_id || '/' || dest_thread || '/' || state from bot_deliveries"
        )
        .await,
        "slack/channel/T1/C9/t1/pending"
    );
    assert_eq!(audit(&w, "schedule.fire").await, 1);
    // 한 번짜리의 마지막 발생이라 정상 종료로 닫힌다(감사 한 줄): 수신 경로 때문에 닫은 것이 아니다
    assert_eq!(
        w.pg.scalar::<String>("select verdict from bot_audit where action = 'schedule.close'")
            .await,
        "finished"
    );
}

#[tokio::test]
async fn a_destination_that_left_or_moved_closes_the_schedule_and_fires_nothing() {
    for (answer, why) in [
        (Err(Error::Denied), "등록부에서 빠졌다"),
        (Err(Error::Invalid), "값이 잘못됐다"),
        (Ok(channel("T2/C9")), "다른 팀으로 재지정됐다"),
        (Ok(channel("T1/C1")), "다른 채널로 재지정됐다"),
    ] {
        let Some(w) = world().await else {
            return;
        };
        plant(&w, "s1", Some("T1/C9")).await;
        let before = runs(&w).await;
        let tick = scheduler(&w, Some(Routes::new(answer)))
            .tick_users()
            .await
            .unwrap();
        assert_eq!((tick.fired, tick.closed), (0, 1), "{why}");
        // 새 run 0, 목적지 행 0, 발생 0, 예약은 닫히고 고정 낱말로 감사한다
        assert_eq!(runs(&w).await, before, "{why}");
        for table in ["bot_deliveries", "bot_user_occurrences"] {
            assert_eq!(
                w.pg.scalar::<i64>(&format!("select count(*) from {table}"))
                    .await,
                0,
                "{why}: {table}"
            );
        }
        assert_eq!(
            w.pg.scalar::<String>("select state from bot_user_schedules")
                .await,
            "closed",
            "{why}"
        );
        assert_eq!(audit(&w, "schedule.close").await, 1, "{why}");
        assert_eq!(audit(&w, "schedule.fire").await, 0, "{why}");
        assert_eq!(
            w.pg.scalar::<String>("select verdict from bot_audit where action = 'schedule.close'")
                .await,
            "unrouted",
            "{why}"
        );
        // 닫힌 예약은 다시 점검해도 아무것도 하지 않는다
        let again = scheduler(&w, Some(Routes::new(Ok(channel("T1/C9")))))
            .tick_users()
            .await
            .unwrap();
        assert_eq!((again.fired, again.closed), (0, 0), "{why}");
        assert_eq!(runs(&w).await, before, "{why}");
    }
}

#[tokio::test]
async fn an_unconfirmed_registry_holds_the_occurrence_untouched_and_the_same_occurrence_fires_after_it_recovers()
 {
    let Some(w) = world().await else {
        return;
    };
    plant(&w, "s1", Some("T1/C9")).await;
    let before = runs(&w).await;
    let at =
        w.pg.scalar::<i64>("select at_ms from bot_user_schedules")
            .await;
    let (cursor, next) = (
        w.pg.scalar::<i64>("select cursor_ms from bot_user_schedules")
            .await,
        w.pg.scalar::<i64>("select next_ms from bot_user_schedules")
            .await,
    );
    // 등록부가 일시적으로 답하지 못하거나 호스트가 붙이지 않았다: 몇 번을 점검해도 건드리지 않는다
    let routes = Routes::new(Err(Error::Unavailable));
    for scheduler in [
        scheduler(&w, Some(routes.clone())),
        scheduler(&w, None),
        scheduler(&w, Some(routes.clone())),
    ] {
        let tick = scheduler.tick_users().await.unwrap();
        assert_eq!((tick.fired, tick.closed, tick.waiting), (0, 0, 0));
    }
    assert_eq!(runs(&w).await, before);
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules")
            .await,
        "active"
    );
    assert_eq!(
        (
            w.pg.scalar::<i64>("select cursor_ms from bot_user_schedules")
                .await,
            w.pg.scalar::<i64>("select next_ms from bot_user_schedules")
                .await
        ),
        (cursor, next),
        "커서와 다음 확인 시각이 그대로다"
    );
    for action in ["schedule.fire", "schedule.close"] {
        assert_eq!(audit(&w, action).await, 0, "{action}");
    }
    // 등록부가 돌아오면 같은 발생(원래 예정 시각)으로 발화한다: 발생을 버리지 않았다
    routes.now_says(Ok(channel("T1/C9")));
    let tick = scheduler(&w, Some(routes)).tick_users().await.unwrap();
    assert_eq!(tick.fired, 1);
    assert_eq!(
        w.pg.scalar::<i64>("select at_ms from bot_user_occurrences")
            .await,
        at,
        "원래 예정 시각의 발생이다"
    );
    assert_eq!(runs(&w).await, before + 1);
}

#[tokio::test]
async fn a_schedule_for_the_original_conversation_never_asks_the_registry_and_a_hold_does_not_block_the_others()
 {
    let Some(w) = world().await else {
        return;
    };
    // 목적지가 있는 예약(보류될 것)과 원래 대화로 답하는 예약이 같은 점검에 함께 나온다
    plant(&w, "s1-stuck", Some("T1/C9")).await;
    plant(&w, "s2-plain", None).await;
    let before = runs(&w).await;
    let routes = Routes::new(Err(Error::Unavailable));
    let tick = scheduler(&w, Some(routes.clone()))
        .tick_users()
        .await
        .unwrap();
    assert_eq!((tick.fired, tick.closed), (1, 0));
    assert_eq!(routes.asked(), 1, "목적지가 있는 예약만 묻는다");
    assert_eq!(runs(&w).await, before + 1);
    assert_eq!(
        w.pg.scalar::<String>("select schedule from bot_user_occurrences")
            .await,
        "s2-plain"
    );
    assert_eq!(
        w.pg.scalar::<String>("select state from bot_user_schedules where id = 's1-stuck'")
            .await,
        "active"
    );
    // 원래 대화 예약만 있어도 등록부는 묻지 않는다(등록부가 없어도 발화한다)
    plant(&w, "s3-plain", None).await;
    let none = scheduler(&w, None).tick_users().await.unwrap();
    assert_eq!(none.fired, 1);
}
