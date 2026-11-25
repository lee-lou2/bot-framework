//! 도구를 실행하는 브로커(워커용). 도구 실행기와 읽기 전용 설치본은 `runner`가 만든다(srt 샌드박스 안의 MCP 서버를 호출마다 새로 띄워 실행하고,
//! 서버가 없는 호스트는 실행기 없이 설치 판본 조회만 한다). 이 브로커가 예약 회신 전송 입구와 CLI 확정 거절 사전 판정도 맡는다.
use super::{runner, unusable::unusable_tools, watched::Watched};
use crate::{host::HostFile, install::Installs, settings::Settings};
use bot_broker::{
    ports::ToolRunner,
    seal::JcsSealer,
    service::{BrokerService, Config, Parts, preflight::Preflight},
    sys::{OsEntropy, SystemClock},
};
use bot_core::{
    ledger::CallLedger,
    port::{AuditLog, Broker, EffectJournal, MemoryStore},
    profile::ProfileCatalog,
    schedule::{ScheduleDelivery, ScheduleRoutes, ScheduledReplies, UserSchedules},
    trace::Tracer,
};
use bot_gateway::active::ActiveCheck;
use bot_profile::{Registry, SchemaValidator};
use bot_store::Store;
use std::sync::Arc;

pub struct ToolDeps<'a> {
    pub host: &'a HostFile,
    pub settings: &'a Settings,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// 가림 장치를 거치는 보관소(`Observe.secrets`)만 받는다 — 도구 자격을 읽는 길이라 날 보관소는 넘길 수 없다
    pub secrets: Arc<Watched>,
    pub tracer: Arc<dyn Tracer>,
    /// 이 프로세스를 실행한 OS 사용자
    pub uid: u32,
    /// 명시 목적지 등록부(결정 브로커·scheduler와 같은 것)
    pub routes: Arc<dyn ScheduleRoutes>,
    /// 이 프로세스의 활성화 판본 확인: 원격 도구 서버로 나가는 모든 요청 직전에 다시 확인한다
    pub active: Arc<dyn ActiveCheck>,
}

/// 도구를 실행하는 브로커와, 쓰는 동안 설치본 사용 잠금을 쥐는 설치 목록. 같은 브로커가 예약 회신 전송 입구와 CLI 확정 거절 사전 판정도 맡는다.
pub struct Tools {
    pub broker: Arc<dyn Broker>,
    /// 다른 수신자로 가는 예약 회신의 전송 입구(`Worker::with_delivery`의 `release`)
    pub delivery: Arc<dyn ScheduleDelivery>,
    /// 모델 CLI의 호출이 실행 전에 확정적으로 거절되는지 가리는 사전 판정(`PlannedCalls::with_preflight`)
    pub preflight: Arc<dyn Preflight>,
    /// 도구 실행기: 종료 정리가 시작해 둔 호출의 프로세스를 `reap`으로 끝내고 확인하는 데 쓴다(`Quiescer`)
    pub runner: Arc<dyn ToolRunner>,
    _installs: Installs,
}

pub async fn build(deps: ToolDeps<'_>) -> Result<Tools, String> {
    let ToolDeps {
        host,
        settings,
        registry,
        store,
        secrets,
        tracer,
        uid,
        routes,
        active,
    } = deps;
    let ledger = store.clone() as Arc<dyn CallLedger>;
    let (runner, installs) = runner::build(
        host,
        settings,
        &secrets,
        store.clone() as Arc<dyn EffectJournal>,
        uid,
        active,
    )
    .await?;
    // 도구가 받는 자격이 빠진 도구는 쓸 수 없을 뿐 worker를 세우지 않는다: 이름만 경고한다. 그 도구의 호출은 브로커가 호출을 기록하기 전에
    // `secret_missing`으로 거절한다(`bot validate`는 이런 프로필을 엄격하게 실패시킨다)
    if !host.servers.is_empty() {
        for item in unusable_tools(&*secrets, &registry.active()).await {
            eprintln!("{}", item.warning());
        }
    }
    // 대화에서 만든 예약(`schedule.*`)과 예약 회신 초안은 활성화 판본에 묶은 핸들로 쓴다(회신 초안은 도구를 실행하는 워커 쪽 브로커만 쓴다)
    let schedules = store.clone() as Arc<dyn UserSchedules>;
    let replies = store.clone() as Arc<dyn ScheduledReplies>;
    let tool_runner = runner.clone();
    let broker = BrokerService::new(
        Parts {
            catalog: registry as Arc<dyn ProfileCatalog>,
            ledger,
            effects: store.clone() as Arc<dyn EffectJournal>,
            memory: store.clone() as Arc<dyn MemoryStore>,
            audit: store as Arc<dyn AuditLog>,
            runner,
            sealer: Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new()))),
            tracer,
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(schedules)
    .with_replies(replies)
    .with_routes(routes);
    let broker = Arc::new(broker);
    Ok(Tools {
        broker: broker.clone(),
        delivery: broker.clone(),
        preflight: broker,
        runner: tool_runner,
        _installs: installs,
    })
}
