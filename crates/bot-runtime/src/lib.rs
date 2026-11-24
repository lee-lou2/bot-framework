//! 워커·예약·위임과 모델 격리. 이 파일은 모듈 선언과 재노출만 둔다.
//! 수락 시험: fences_expired_workers, catches_up_missed_schedule(tests/).
pub mod context;
pub mod cron;
pub mod host;
pub mod model;
pub mod scheduler;
mod scope;
pub mod worker;

pub use host::{
    Deps, Handback, Models, NoRelays, Quiesce, Relays, SchedulerConfig, SchedulerDeps, SingleModel,
    WorkerConfig,
};
pub use scheduler::{Scheduler, Tick, UserTick, schedule_set};
pub use worker::{Delivery, Outcome, Worker};
