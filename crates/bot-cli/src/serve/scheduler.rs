//! scheduler 역할: 예약(cron)만 접수한다. 도구도 모델도 시작하지 않는다(접수한 run은 worker가 집는다).
//!
//! 정적 예약 정의는 활성 판본과 **같은 커밋**으로 게시된다(`serve::Publication.schedules`, 어느 역할이 게시하든 올라간다) —
//! 이 역할은 정의를 따로 맞추지 않고 저장소의 구간만 읽어 접수한다. 사용자 예약(대화에서 만든 at·cron)은 저장소의 `UserSchedules`에서
//! 읽고, 발화마다 현재 판본(멤버·바인딩·에이전트·권한)을 활성 프로필 목록에서 다시 본다.
use super::trace::Observe;
use bot_core::{
    port::ScheduleStore,
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, UserSchedules},
};
use bot_profile::Registry;
use bot_runtime::{Scheduler, SchedulerConfig, SchedulerDeps};
use bot_store::Store;
use std::sync::Arc;
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    pub observe: Arc<Observe>,
    pub stop: CancellationToken,
    /// 명시 목적지 등록부(worker·결정 브로커와 같은 것): 발화마다 지금 등록부로 다시 확인한다
    pub routes: Arc<dyn ScheduleRoutes>,
}

pub struct Prepared {
    scheduler: Scheduler,
    stop: CancellationToken,
}

pub fn prepare(deps: &Deps) -> Prepared {
    Prepared {
        scheduler: Scheduler::new(
            SchedulerDeps {
                schedule: deps.store.clone() as Arc<dyn ScheduleStore>,
                user: deps.store.clone() as Arc<dyn UserSchedules>,
                catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
                tracer: deps.observe.tracer(),
            },
            SchedulerConfig::default(),
        )
        .with_routes(deps.routes.clone()),
        stop: deps.stop.clone(),
    }
}

impl Prepared {
    /// 예약을 접수하기 시작한다. 활성 판본(과 예약 정의)을 확정한 뒤에 부른다.
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self { scheduler, stop } = self;
        tasks.spawn(async move {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                stop.cancelled().await;
                let _ = tx.send(true);
            });
            scheduler.run(rx).await;
            Ok(())
        });
    }
}
