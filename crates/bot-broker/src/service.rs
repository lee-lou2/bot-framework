//! `Broker` 구현. 모델→도구 경계의 한 관문이다.
//!
//! 호출 순서: 세대 확인 → 선언·정책 조회 → 인자 봉인(검증·정규화·해시) → 호출 기록(멱등) →
//! (옛 시도가 남긴 호출이면 복구 판정) → 권한 → 속도·동시성 → 분류 → 승인 → 시작·실행·결과 기록.
//! 승인 소비와 시작은 원장이 한 트랜잭션으로 묶고, 브로커는 시작이 커밋된 뒤에만 실행기를 부른다.
//! 승인 대기 중에는 아무것도 실행하지 않고 `Awaiting`을 돌려준다. 승인 뒤 run이 같은 호출 ID로
//! 다시 들어오면 그때 현재 정책으로 다시 판정하고 승인을 소비해 실행한다.

mod authority;
mod call;
mod decide;
mod deliver;
mod exec;
mod gate;
mod manage;
pub(crate) mod pass;
pub mod preflight;
mod route;
pub(crate) mod schedule;
pub(crate) mod share;
mod site;
#[cfg(test)]
mod tests;
mod wake;

use crate::{
    ports::*,
    rate::{Limiter, RateConfig},
    sys::{Clock, Entropy},
};
use bot_core::{
    command::*,
    id::*,
    ledger::CallLedger,
    manage::ProfileManager,
    port::{AuditLog, Broker, EffectJournal, Error, MemoryStore, ModelCall, Reply},
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, ScheduledReplies, UserSchedules},
    trace::Tracer,
};
use std::{sync::Arc, time::Duration};

/// 브로커가 기대는 포트 묶음. 호스트가 조립한다.
pub struct Parts {
    pub catalog: Arc<dyn ProfileCatalog>,
    pub ledger: Arc<dyn CallLedger>,
    pub effects: Arc<dyn EffectJournal>,
    pub memory: Arc<dyn MemoryStore>,
    pub audit: Arc<dyn AuditLog>,
    pub runner: Arc<dyn ToolRunner>,
    pub sealer: Arc<dyn Sealer>,
    pub tracer: Arc<dyn Tracer>,
    pub clock: Arc<dyn Clock>,
    pub entropy: Arc<dyn Entropy>,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// 승인이 열려 있는 시간. 지나면 소비할 수 없다.
    pub approval_ttl: Duration,
    pub rate: RateConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            approval_ttl: Duration::from_secs(30 * 60),
            rate: RateConfig::default(),
        }
    }
}

pub struct BrokerService {
    parts: Parts,
    limiter: Arc<Limiter>,
    cfg: Config,
    /// 대화에서 만든 예약 저장소(내장 `schedule.*`와 도구의 후속 확인). 붙이지 않으면 모두 `Unavailable`로 닫힌다.
    schedules: Arc<dyn UserSchedules>,
    /// 예약 실행이 동결한 최종 회신 초안과 다른 수신자로의 전송 커밋(`ScheduleDelivery::release`). 붙이지 않으면 `Unavailable`로 닫힌다.
    replies: Arc<dyn ScheduledReplies>,
    /// 같은 프로필의 호스트 등록부(명시 목적지를 정규 전송 경로로 확정한다). 붙이지 않으면 다른 수신자는 지정할 수 없고 전송하지도 않는다.
    routes: Option<Arc<dyn ScheduleRoutes>>,
    /// 슈퍼 관리자 전용 런타임 스킬·MCP 관리기. 붙이지 않으면 `skill.*`·`mcp.*`는 `Unavailable`로 닫힌다.
    manager: Option<Arc<dyn ProfileManager>>,
}

impl BrokerService {
    pub fn new(parts: Parts, cfg: Config) -> Self {
        let limiter = Limiter::new(cfg.rate, Arc::clone(&parts.clock));
        Self {
            parts,
            limiter,
            cfg,
            schedules: Arc::new(schedule::NoSchedules),
            replies: Arc::new(deliver::NoReplies),
            routes: None,
            manager: None,
        }
    }

    /// 슈퍼 관리자 전용 스킬·MCP 관리기를 붙인다. 붙이지 않으면 `skill.*`·`mcp.*`는 `Unavailable`이다.
    pub fn with_manager(mut self, manager: Arc<dyn ProfileManager>) -> Self {
        self.manager = Some(manager);
        self
    }

    /// 대화에서 만든 예약 저장소를 붙인다. 붙이지 않으면 `schedule.create`·`schedule.list`·`schedule.cancel`은 `Unavailable`이고,
    /// 도구의 후속 확인 요청(`_meta["bot/wake"]`)은 확정할 수 없어 그 호출을 보류한다(거짓으로 예약됐다고 하지 않는다).
    pub fn with_schedules(mut self, schedules: Arc<dyn UserSchedules>) -> Self {
        self.schedules = schedules;
        self
    }

    /// 예약 회신 초안 저장소를 붙인다(워커 쪽 브로커). 붙이지 않으면 `ScheduleDelivery::release`는 초안을 읽지 못해 `Unavailable`이고
    /// 아무것도 보내지 않는다. 승인 결정만 받는 브로커(`decide`)에는 필요 없다.
    pub fn with_replies(mut self, replies: Arc<dyn ScheduledReplies>) -> Self {
        self.replies = replies;
        self
    }

    /// 같은 프로필의 호스트 등록부를 붙인다. 붙이지 않으면 `schedule.create`의 `destination`(다른 수신자)은 거절되고, 다른 수신자로 가는
    /// 회신(`schedule.deliver`)은 등록부를 확인할 수 없어 보내지 않는다(실패로 닫는다).
    pub fn with_routes(mut self, routes: Arc<dyn ScheduleRoutes>) -> Self {
        self.routes = Some(routes);
        self
    }
}

impl Broker for BrokerService {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        Box::pin(self.call_flow(call))
    }

    fn call_for_model(&self, call: ToolCall) -> Reply<'_, ModelCall> {
        Box::pin(async move {
            let error = match self.call(call.clone()).await {
                Ok(result) => return Ok(ModelCall::Returned(result)),
                // 리스와 판본은 워커가 다시 대조한다. 반납이나 재판정이 필요한 위치에 거절 표식을 만들지 않는다.
                Err(Error::Stale) => return Err(Error::Stale),
                Err(error) => error,
            };
            let Some(reason) = preflight::expected(&error) else {
                return Err(error);
            };
            match preflight::confirm(self.parts.ledger.as_ref(), Some(self), call, reason).await {
                Ok(true) => Ok(ModelCall::Refused(reason)),
                // 원래 오류는 호출자에게 넘기고, 사전 판정이 확인한 거절만 표식으로 만든다.
                Ok(false) => Err(error),
                Err(Error::Stale) => Err(Error::Stale),
                Err(_) => Err(Error::Unavailable),
            }
        })
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        Box::pin(self.decide_flow(nonce, actor, allow))
    }
}
