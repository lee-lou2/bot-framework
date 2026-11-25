//! worker 역할: 접수된 run을 집어 모델(`providers`)과 도구(브로커)로 실행한다. 데모 모델은 외부 키·VM 없이 결정적으로 실행하고,
//! 도구는 읽기 전용 설치본을 srt 샌드박스에서 호출마다 새로 띄워 실행한다(승인 규칙은 실제 도구와 같다). 워커는 하나의 프로세스에서
//! 브로커(`BrokerService`)를 함께 쓰므로 도구 실행기가 따로 필요하지 않다.
//!
//! 구판 보호: 이 프로세스의 활성화 판본이 저장소의 활성 판본과 다르면(모델 허용·상류 목적지·egress·설치 판본 같은 권한 설정이
//! 바뀜) 새 일을 집지 않고 도구를 시작하지 않는다(`guard`). 이미 집은 run은 끝까지 한다.
use super::{
    claude::{self, Claude, ClaudeParts},
    guard::{GuardedBroker, GuardedRuns, StoreHandback},
    models,
    quiesce::{Quiescer, StartedCalls},
    tools::{self, ToolDeps, Tools},
    trace::Observe,
    watched::Watched,
};
use crate::{host::HostFile, settings::Settings};
use bot_core::{
    ledger::CallLedger,
    port::{Broker, DelegationStore, MemoryStore, RunStore},
    profile::ProfileCatalog,
    schedule::{ScheduleRoutes, ScheduledReplies},
};
use bot_gateway::active::ActiveCheck;
use bot_profile::Registry;
use bot_runtime::{Delivery, Deps as RuntimeDeps, NoRelays, Relays, Worker, WorkerConfig};
use bot_store::Store;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, task::JoinSet};
use tokio_util::sync::CancellationToken;

/// 종료 신호 뒤 진행 중인 run이 정상 완료하거나 승인 대기로 넘어가도록 기다리는 시간(하트비트 유지).
const GRACE: Duration = Duration::from_secs(8);
/// 유예 뒤에도 남은 run의 정리(프로세스 종료·세션 회수 확인과 대기열 반납)에 쓰는 시간.
const CLEANUP: Duration = Duration::from_secs(10);
/// 전체 종료가 역할을 기다리는 기한. 유예 + 정리가 끝난 뒤 프런트를 닫고 닻·VM을 치우는 시간(`Claude::stop`: 프런트 → `vm.shutdown()`)까지 넉넉히 둔다.
/// 이 기한이 정리를 중간에 끊으면 안 된다(끊겨도 닻은 `Drop`이 끝내지만 반납·확인은 놓친다).
pub(super) const SHUTDOWN_WAIT: Duration = Duration::from_secs(40);
// 종료 대기가 유예 + 정리보다 짧으면 정리를 중간에 끊는다(docs/reliability.md «워커 종료와 리스 인계»)
const _: () = assert!(GRACE.as_secs() + CLEANUP.as_secs() < SHUTDOWN_WAIT.as_secs());

pub struct Deps {
    pub host: Arc<HostFile>,
    pub settings: Settings,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// 가림 장치를 거치는 보관소(`Observe.secrets`)만 받는다 — 날 보관소는 넘길 수 없다
    pub secrets: Arc<Watched>,
    pub observe: Arc<Observe>,
    /// 이 프로세스를 실행한 OS 사용자
    pub uid: u32,
    pub stop: CancellationToken,
    /// 이 프로세스의 활성화 판본이 저장소의 활성 판본과 같은지 확인한다
    pub active: Arc<dyn ActiveCheck>,
    /// 모델 CLI(`claude-cli`)를 쓰는 별칭이 있을 때 쓸 운영 어댑터 조각(모델 샌드박스·상류 전송). 없으면 `claude-cli`는 시작하지 않는다.
    pub claude: Option<ClaudeParts>,
    /// 운영 조각의 상류 전송만 바꾼다(시험 전용, 제품은 `None`)
    pub upstream: Option<Arc<dyn bot_broker::model::Upstream>>,
    /// 명시 목적지 등록부(결정 브로커·scheduler와 같은 것)
    pub routes: Arc<dyn ScheduleRoutes>,
}

/// 점검을 마친 워커. `open` 전에는 아무 일도 집지 않는다.
pub struct Prepared {
    worker: Worker,
    stop: CancellationToken,
    tools: Tools,
    /// 모델 CLI의 브로커 프런트(쓰는 별칭이 있을 때)
    claude: Option<Claude>,
}

/// 모델 실행기와 도구 브로커를 만든다(srt·설치본 점검 포함). 하나라도 어긋나면 Err — 아직 아무 일도 집지 않았고 판본도 그대로다.
pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    // 모델 서비스 키: 프로필이 허용한 claude-cli 별칭의 자격이 보관소에 없으면 아무것도 만들기 전에 멈춘다
    let missing = claude::missing_keys(
        &deps.host,
        &deps.registry,
        deps.secrets.as_ref(),
        &deps.settings.secrets,
    )
    .await;
    if !missing.is_empty() {
        return Err(format!(
            "모델 서비스 키가 보관소에 없어 worker를 시작하지 않아요:\n{}",
            missing.join("\n")
        ));
    }
    let tools = tools::build(ToolDeps {
        host: &deps.host,
        settings: &deps.settings,
        registry: deps.registry.clone(),
        store: deps.store.clone(),
        secrets: deps.secrets.clone(),
        tracer: deps.observe.tracer(),
        uid: deps.uid,
        routes: deps.routes.clone(),
        active: deps.active.clone(),
    })
    .await?;
    // 구판 보호가 걸린 브로커: 워커의 Step 방식과 모델 CLI의 브로커 프런트가 같은 브로커를 쓴다
    let broker: Arc<dyn Broker> = Arc::new(GuardedBroker::new(
        tools.broker.clone(),
        deps.active.clone(),
    ));
    // 모델 CLI를 쓰는 별칭이 있으면 프런트를 연다. 조각은 호출하는 쪽이 줬으면(시험) 그것을, 아니면 호스트 설정으로 운영 조각(Apple VM 어댑터 +
    // HTTPS 상류 전송)을 만든다 — `sandbox.apple`이 없으면 시작하지 않는다(다른 샌드박스로 대신하지 않는다).
    let claude = match claude::needed(&deps.host).then(|| match deps.claude.clone() {
        Some(parts) => Ok(parts),
        None => claude::operational(&deps.host, deps.settings.installs.as_deref(), deps.uid).map(
            |mut parts| {
                if let Some(upstream) = &deps.upstream {
                    parts.upstream = upstream.clone();
                }
                parts
            },
        ),
    }) {
        Some(parts) => Some(
            claude::start(
                parts?,
                claude::Input {
                    host: &deps.host,
                    registry: deps.registry.clone(),
                    store: deps.store.clone(),
                    secrets: deps.secrets.clone(),
                    broker: broker.clone(),
                    preflight: tools.preflight.clone(),
                    runs: deps.store.clone() as Arc<dyn RunStore>,
                    active: deps.active.clone(),
                },
            )
            .await?,
        ),
        None => None,
    };
    // API 직접 호출 별칭(`anthropic-api`·`openai-api`)은 VM 없이 워커가 상류를 부른다: 키는 가림 장치를 거치는 보관소로만 읽고, 매 요청 직전에 이 프로세스의 활성화 판본을 확인한다
    let api = models::ApiParts {
        secrets: deps.secrets.clone(),
        gate: Arc::new(models::ActiveGate(deps.active.clone())),
    };
    // 이 뒤의 실패는 이미 띄운 모델 프런트·VM을 치우고 돌아간다(닻 VM이 남지 않게)
    let models = match models::build(
        &deps.host,
        claude.as_ref().map(|c| c.runner.clone()),
        Some(&api),
    ) {
        Ok(models) => models,
        Err(error) => {
            if let Some(claude) = claude {
                claude.stop().await;
            }
            return Err(error);
        }
    };
    let relays: Arc<dyn Relays> = match &claude {
        Some(claude) => claude.relays.clone(),
        None => Arc::new(NoRelays),
    };
    let worker = Worker::new(
        RuntimeDeps {
            // 구판이 되면 새 일을 집지 않고, 실패 보고는 저장소가 판단해 시도 횟수를 쓰지 않고 대기열로 돌려준다
            runs: Arc::new(
                GuardedRuns::new(deps.store.clone() as Arc<dyn RunStore>, deps.active.clone())
                    .with_handover(deps.store.clone()),
            ),
            memory: deps.store.clone() as Arc<dyn MemoryStore>,
            delegation: deps.store.clone() as Arc<dyn DelegationStore>,
            broker,
            // CLI 세션을 되살릴 때 호출 결과를 읽는다(읽기 쓰임)
            ledger: deps.store.clone() as Arc<dyn CallLedger>,
            catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
            models: Arc::new(models),
            relays,
            tracer: deps.observe.tracer(),
            gateway: None,
        },
        WorkerConfig {
            // 유예 + 정리 시간은 serve의 종료 대기(`SHUTDOWN_WAIT`)보다 짧아야 한다
            grace: GRACE,
            cleanup: CLEANUP,
            // 중단 요청을 빠르게 감지하도록 heartbeat를 3초로 둔다(리스 30초 대비 충분한 여유)
            heartbeat: std::time::Duration::from_secs(3),
            ..WorkerConfig::default()
        },
    )
    // 다른 수신자로 가는 예약 회신: 초안은 저장소(활성화 판본에 묶은 핸들)가 동결하고 전송은 브로커의 승인 절차로 끝낸다
    .with_delivery(Delivery {
        replies: deps.store.clone() as Arc<dyn ScheduledReplies>,
        release: tools.delivery.clone(),
    })
    // 물러나는 run은 저장소가 «구판이고 아직 소유자»일 때만 대기열로 돌려준다(리스가 끝나길 기다리다 인계 때 시도를 쓰지 않게)
    .with_handback(Arc::new(StoreHandback(deps.store.clone())))
    // 종료 신호 뒤 남은 run은 모델 VM·도구 프로세스가 끝났고 세션이 거두어졌음을 확인한 뒤에만 대기열로 돌려준다
    .with_quiesce(Arc::new(Quiescer::new(
        deps.store.clone() as Arc<dyn StartedCalls>,
        tools.runner.clone(),
        claude
            .as_ref()
            .map(|claude| (claude.sessions.clone(), claude.sandbox.clone())),
    )));
    Ok(Prepared {
        worker,
        stop: deps.stop.clone(),
        tools,
        claude,
    })
}

impl Prepared {
    /// 열지 못하고 버린다(활성 판본 확정이 실패했다): 이미 띄운 모델 프런트와 VM(닻 포함)을 치운다. 아무 일도 집지 않았다.
    pub async fn discard(self) {
        if let Some(claude) = self.claude {
            claude.stop().await;
        }
    }

    /// 일을 집기 시작한다. 활성 판본 확정 뒤에 부른다.
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            worker,
            stop,
            tools,
            claude,
        } = self;
        tasks.spawn(async move {
            let (tx, rx) = watch::channel(false);
            tokio::spawn(async move {
                stop.cancelled().await;
                let _ = tx.send(true);
            });
            worker.run(rx).await;
            // 끝나면 모델 프런트를 닫고 설치본 사용 잠금을 놓는다
            if let Some(claude) = claude {
                claude.stop().await;
            }
            drop(tools);
            Ok(())
        });
    }
}
