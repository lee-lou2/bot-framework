//! 구판 워커가 물러날 때(`Flow::Lost`)의 조건부 반납(실제 PG): 예약 발화 run을 집은 직후 활성 집합이 교체돼 회신 초안 조회가 `Stale`이 되면 워커는 물러나는데,
//! 그때 run을 `leased`로 남겨 두면 리스가 끝나길 기다려야 이어진다(그 만료는 활성 집합이 바뀌어 생긴 것이라 인계가 시도를 쓰지 않는다 — 리스 인계의
//! 활성 집합 비교). 물러나기 전에 저장소에 조건부 반납을 청하면 리스 만료를 기다리지 않고 바로 대기열로 돌아가 새 판본의 워커가 시도를 쓰지 않고 이어 간다.
//! 이미 인계된 claim과 현재 판본의 워커는 건드리지 않는다.

use crate::support::*;
use bot_core::{
    command::Finish,
    id::*,
    port::{Error, Reply, RunStore},
    profile::ActiveRevisions,
    schedule::{DeliveryDraft, ScheduledReplies},
};
use bot_runtime::{Delivery, Handback, Models, Outcome, SingleModel, model::DemoModel};
use bot_store::{Config, Store};
use std::{collections::BTreeMap, sync::Arc};

fn demo() -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(DemoModel)))
}

fn rev(value: &str) -> Revision {
    Revision(value.into())
}

/// 조건부 반납을 저장소의 구판 반납 함수에 잇는다(운영 호스트의 연결과 같다)
struct ToStore(Arc<Store>);

impl Handback for ToStore {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move { self.0.release_stale(&attempt).await })
    }
}

/// 초안을 읽기 직전에 다른 프로세스가 활성 집합을 교체한 상황(집은 직후 교체)을 흉내 낸다. 그다음의 읽기는 고정한 집합이 옛것이라 `Stale`이다.
struct ReplacedBeforeTheDraft {
    world: Arc<Store>,
    inner: Arc<Store>,
    done: std::sync::atomic::AtomicBool,
}

impl ScheduledReplies for ReplacedBeforeTheDraft {
    fn receipt(&self, attempt: Attempt) -> Reply<'_, Option<bot_core::command::JsonPayload>> {
        ScheduledReplies::receipt(&*self.inner, attempt)
    }

    fn draft(
        &self,
        attempt: Attempt,
        completion: Option<Finish>,
    ) -> Reply<'_, Option<DeliveryDraft>> {
        Box::pin(async move {
            if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                self.world
                    .replace(
                        Some(rev("set-1")),
                        rev("set-2"),
                        BTreeMap::from([(ProfileId("personal".into()), rev("r1"))]),
                        Vec::new(),
                        Default::default(),
                    )
                    .await?;
            }
            ScheduledReplies::draft(&*self.inner, attempt, completion).await
        })
    }

    fn target(&self, attempt: Attempt) -> Reply<'_, Option<bot_core::schedule::DeliveryTarget>> {
        ScheduledReplies::target(&*self.inner, attempt)
    }

    fn finish_unsent(
        &self,
        fence: bot_core::schedule::DeliveryFence,
        target: bot_core::schedule::DeliveryTarget,
        reason: bot_core::schedule::UnsentReason,
    ) -> Reply<'_, bot_core::command::JsonPayload> {
        ScheduledReplies::finish_unsent(&*self.inner, fence, target, reason)
    }

    fn enqueue(&self, approval: bot_core::ledger::StartCall) -> Reply<'_, ()> {
        ScheduledReplies::enqueue(&*self.inner, approval)
    }
}

/// 사용자 예약이 발화한 run처럼 보이게 한다(루트이고 출처가 예약이면 워커가 회신 초안을 먼저 본다).
async fn as_scheduled(w: &World, run: &RunId) {
    w.pg.sql(&format!(
        "update bot_runs set run_origin = 'schedule' where id = '{}'",
        run.0
    ))
    .await;
}

/// 마지막 시도 한도(1)로 활성 집합 `set`에 묶인 저장소 핸들. 인계(리스 만료)가 시도를 쓰면 곧바로 dead letter다.
fn handle(w: &World, worker: &str, set: &str) -> Arc<Store> {
    Arc::new(
        Store::from_pool(
            w.pg.store.pool().clone(),
            Config {
                worker: worker.into(),
                max_attempts: 1,
                ..Config::default()
            },
        )
        .for_activation(rev(set)),
    )
}

async fn attempts(w: &World, run: &RunId) -> i32 {
    w.pg.scalar::<i32>(&format!(
        "select attempts from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

fn replaced_before_the_draft(w: &World, old: &Arc<Store>) -> Arc<ReplacedBeforeTheDraft> {
    Arc::new(ReplacedBeforeTheDraft {
        world: w.pg.store.clone(),
        inner: old.clone(),
        done: Default::default(),
    })
}

#[tokio::test]
async fn a_worker_that_steps_back_after_the_set_was_replaced_hands_the_run_back_and_a_new_worker_finishes_it()
 {
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-1", "오늘 일정 알려줘").await;
    as_scheduled(&w, &run).await;
    // 옛 판본의 워커: 활성 집합 set-1에 묶였다
    let old = handle(&w, "old-worker", "set-1");
    let worker = w
        .worker(&old, demo(), config())
        .with_delivery(Delivery {
            replies: replaced_before_the_draft(&w, &old),
            release: w.broker.clone(),
        })
        .with_handback(Arc::new(ToStore(old.clone())));
    // 집은 직후 집합이 교체돼 회신 초안 조회가 Stale → 물러난다. 대기열로 돌아가고 시도는 그대로다
    assert_eq!(worker.run_once().await, Some(Outcome::Lost));
    assert_eq!(w.pg.state_of(&run).await, "ready");
    assert_eq!(attempts(&w, &run).await, 0);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'run.handover'")
            .await,
        1
    );
    // 새 판본의 워커가 이어받아 끝낸다(시도 소모 0)
    let new = handle(&w, "new-worker", "set-2");
    let resumed = w.worker(&new, demo(), config());
    assert_eq!(resumed.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.pg.state_of(&run).await, "succeeded");
    assert_eq!(attempts(&w, &run).await, 0);
}

#[tokio::test]
async fn without_the_handback_the_run_stays_leased_until_the_lease_ends_and_the_set_change_spares_the_attempt()
 {
    // 대조: 같은 상황에서 반납을 붙이지 않으면 run은 리스가 끝날 때까지 leased로 남는다. 끝난 뒤의 인계는 활성 집합이 바뀌어 생긴 만료라 시도를 쓰지 않는다
    // (리스 인계의 활성 집합 비교 — 예전에는 여기서 한도(1)에 닿아 dead letter가 됐다)
    let Some(w) = world().await else {
        return;
    };
    let run = w.accept("req-2", "오늘 일정 알려줘").await;
    as_scheduled(&w, &run).await;
    let old = handle(&w, "old-worker", "set-1");
    let worker = w.worker(&old, demo(), config()).with_delivery(Delivery {
        replies: replaced_before_the_draft(&w, &old),
        release: w.broker.clone(),
    });
    assert_eq!(worker.run_once().await, Some(Outcome::Lost));
    assert_eq!(
        w.pg.state_of(&run).await,
        "leased",
        "반납이 없으면 리스가 끝나길 기다린다"
    );
    // 리스가 끝난 뒤 새 판본의 워커가 인계한다: 직전 claim의 집합(set-1)과 이 워커의 집합(set-2)이 달라 시도를 쓰지 않는다
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let new = handle(&w, "new-worker", "set-2");
    let taken = RunStore::claim(&*new)
        .await
        .unwrap()
        .expect("시도를 쓰지 않고 인계한다");
    assert_eq!(taken.attempt.run, run);
    assert_eq!(attempts(&w, &run).await, 0);
}

#[tokio::test]
async fn a_run_already_taken_over_or_held_by_a_current_worker_is_left_alone() {
    let Some(w) = world().await else {
        return;
    };
    // 현재 판본의 워커가 집은 run은 반납하지 않는다(집합이 같다)
    let run = w.accept("req-3", "오늘 일정 알려줘").await;
    let current = handle(&w, "current-worker", "set-1");
    let claimed = RunStore::claim(&*current).await.unwrap().expect("집는다");
    assert!(matches!(
        current.release_stale(&claimed.attempt).await,
        Err(Error::Denied)
    ));
    assert_eq!(w.pg.state_of(&run).await, "leased");
    // 교체된 뒤에도 이미 다른 세대가 인계한 run의 옛 claim으로는 반납하지 못하고 새 소유자의 run은 그대로다
    w.pg.store
        .replace(
            Some(rev("set-1")),
            rev("set-2"),
            BTreeMap::from([(ProfileId("personal".into()), rev("r1"))]),
            Vec::new(),
            Default::default(),
        )
        .await
        .unwrap();
    w.pg.sql(&format!(
        "update bot_runs set lease_until_ms = bot_now_ms() - 1 where id = '{}'",
        run.0
    ))
    .await;
    let successor = Arc::new(
        Store::from_pool(w.pg.store.pool().clone(), Config::default()).for_activation(rev("set-2")),
    );
    let taken = RunStore::claim(&*successor).await.unwrap().expect("인계");
    assert!(taken.attempt.generation.0 > claimed.attempt.generation.0);
    assert!(matches!(
        current.release_stale(&claimed.attempt).await,
        Err(Error::Stale)
    ));
    assert_eq!(w.pg.state_of(&run).await, "leased");
}
