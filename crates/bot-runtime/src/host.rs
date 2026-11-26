//! 워커·스케줄러가 기대하는 포트 묶음과 설정. 호스트(bot-cli)가 조립한다. 저장소·브로커·프로필·게이트웨이는 모두 `bot_core`의 포트다.

use crate::model::{ModelRunner, RelayGrant};
use bot_core::{
    id::{AgentId, Attempt, ProfileId},
    ledger::CallLedger,
    port::{Broker, DelegationStore, Gateway, MemoryStore, Reply, RunStore, ScheduleStore},
    profile::ProfileCatalog,
    schedule::UserSchedules,
    trace::Tracer,
};
use std::{sync::Arc, time::Duration};

/// 모델 별칭 → 실행기. 운영자가 프로필마다 허용한 제공자 목록에서만 고른다(없으면 `None`이고 run은 `unknown_model`로 끝난다).
pub trait Models: Send + Sync {
    fn runner(&self, profile: &ProfileId, alias: &str) -> Option<Arc<dyn ModelRunner>>;
}

/// Loop 방식 실행기(CLI)가 모델·도구를 쓸 브로커 프런트 연결. 구현은 호스트가 브로커의 세션 표(`SessionTable::issue`)로 한다.
pub trait Relays: Send + Sync {
    /// 이 시도(현재 claim)에 세션 토큰을 발급해 연결을 연다. 돌려준 `RelayGrant`를 놓으면(CLI 실행이 끝나면) **이 시도가 발급받은** 토큰만
    /// 폐기한다 — 이미 새 claim이 발급받은 토큰을 끊지 않는다. 발급할 수 없으면 `None`이고 실행기는 실행하지 않는다.
    fn open(&self, attempt: &Attempt, agent: &AgentId, alias: &str) -> Option<RelayGrant>;
}

/// 브로커 프런트가 없는 호스트(Step 방식 실행기만 쓰는 구성).
pub struct NoRelays;

impl Relays for NoRelays {
    fn open(&self, _: &Attempt, _: &AgentId, _: &str) -> Option<RelayGrant> {
        None
    }
}

/// 물러나는(`Lost`) run을 저장소에 **조건부로** 돌려주는 입구(선택). 호스트가 «이 프로세스의 활성 집합이 더는 현재가 아니고 이 시도가 아직 리스·세대의
/// 소유자»일 때만 대기열로 돌리는 저장소 함수에 잇는다. 이미 인계된 claim(다른 세대)이나 현재 판본의 워커에는 아무 일도 하지 않으므로 워커는 결과를 신경 쓰지
/// 않는다(오류도 무시). 없으면 물러나기만 해 리스가 끝날 때까지 `leased`로 남는다.
pub trait Handback: Send + Sync {
    fn release(&self, attempt: Attempt) -> Reply<'_, ()>;
}

/// 종료 정리 확인(선택): 종료 유예가 지난 뒤 남은 run을 대기열로 돌려주기 **전에** 이 시도(claim)가 띄운 모델·도구 프로세스가 모두 끝났음을 확인하고
/// 그 세션 토큰을 거둔다. 확인하면 `Ok`다. 확인하지 못하면(오류·시간 초과) 워커는 반납하지 않고 리스 인계 규칙에 맡긴다. 없으면 워커는 종료 반납을 하지 않는다.
pub trait Quiesce: Send + Sync {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()>;
}

/// 한 가지 실행기를 모든 프로필·별칭에 쓰는 단순 구성(시험·데모·단일 제공자).
pub struct SingleModel(pub Arc<dyn ModelRunner>);

impl Models for SingleModel {
    fn runner(&self, _: &ProfileId, _: &str) -> Option<Arc<dyn ModelRunner>> {
        Some(self.0.clone())
    }
}

/// 워커가 쓰는 포트들.
#[derive(Clone)]
pub struct Deps {
    pub runs: Arc<dyn RunStore>,
    pub memory: Arc<dyn MemoryStore>,
    pub delegation: Arc<dyn DelegationStore>,
    pub broker: Arc<dyn Broker>,
    /// CLI 세션을 되살릴 때 호출 결과·프런트가 확인한 응답·위임 결과를 읽는다(읽기 전용 쓰임).
    pub ledger: Arc<dyn CallLedger>,
    pub catalog: Arc<dyn ProfileCatalog>,
    pub models: Arc<dyn Models>,
    pub relays: Arc<dyn Relays>,
    pub tracer: Arc<dyn Tracer>,
    /// 결과를 커밋한 뒤 회신을 서둘러 내보내도록 게이트웨이를 깨운다(없으면 게이트웨이가 outbox를 스스로 점유한다).
    pub gateway: Option<Arc<dyn Gateway>>,
}

/// 스케줄러가 쓰는 포트들.
#[derive(Clone)]
pub struct SchedulerDeps {
    pub schedule: Arc<dyn ScheduleStore>,
    /// 사용자 예약(대화에서 만든 at·cron). 발화마다 현재 판본(멤버·바인딩·에이전트·권한)을 `catalog`에서 다시 본다.
    pub user: Arc<dyn UserSchedules>,
    pub catalog: Arc<dyn ProfileCatalog>,
    pub tracer: Arc<dyn Tracer>,
}

#[derive(Clone, Debug)]
pub struct WorkerConfig {
    /// 동시에 실행하는 run 수
    pub concurrency: usize,
    /// 집을 run이 없을 때 다음에 보기까지
    pub poll: Duration,
    /// heartbeat 간격. 저장소의 리스(기본 30초)의 1/3 안팎으로 둔다.
    pub heartbeat: Duration,
    /// 연속으로 heartbeat를 못 보내면(저장소 불통) 리스를 잃은 것으로 보고 멈춘다.
    pub heartbeat_misses: u32,
    pub limits: crate::model::Limits,
    /// 한 run이 부를 수 있는 모델 차례 수 상한(무한 도구 순환 방지)
    pub max_turns: usize,
    /// 맥락에 넣는 같은 대화의 이전 차례 수
    pub history_turns: u32,
    /// 종료 신호 뒤 진행 중인 run을 정상 완료하거나 승인 대기로 넘어가도록 기다리는 시간(하트비트는 유지한다).
    pub grace: Duration,
    /// 유예가 지난 뒤에도 남은 run의 정리(프로세스 종료·세션 회수 확인과 `release_for_shutdown` 반납)에 쓰는 시간. 이 안에 끝나지 않은 run은 반납하지 않고
    /// 놓는다(리스가 끝나면 다른 워커가 인계 규칙으로 이어받는다). 호스트는 전체 종료 기한을 `grace + cleanup`보다 길게 잡아 정리를 끊지 않는다.
    pub cleanup: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            poll: Duration::from_millis(500),
            heartbeat: Duration::from_secs(10),
            heartbeat_misses: 3,
            limits: crate::model::Limits::default(),
            max_turns: 24,
            history_turns: 20,
            grace: Duration::from_secs(20),
            cleanup: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SchedulerConfig {
    /// 스냅샷을 읽는 간격
    pub interval: Duration,
    /// 한 번에 한 트리거에서 접수하는 발생 수(따라잡기 배치 크기)
    pub batch: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(1),
            batch: 100,
        }
    }
}
