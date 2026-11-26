//! gateway 역할: 입구(CLI 소켓·HTTP·Slack)와 회신 발송을 한 프로세스에서 연다. 프로필에 바인딩이 있는 입구만 연다.
//!
//! 시작은 두 단계다. `prepare`가 모든 점검(입구 표·자격·Slack 설치)을 끝내고 리스너를 확보하되 아무것도 열지 않는다.
//! 호출자가 활성 판본·시드를 확정한 뒤에만 `Prepared::open`이 접수를 연다 — 점검이나 리스너 확보가 실패하면 저장소의
//! 운영 판본은 그대로다(떠 있는 게이트웨이가 소켓을 쓰는 중에 `--activate`가 실패해도 판본이 바뀌지 않는다).
//!
//! 이 프로세스는 평소 도구를 실행하지 않는다 — 단 민감 DM 트리거가 있는 프로필이 있으면 `PrivateGate`가 지정한 도구를 이 프로세스의 srt 샌드박스에서 실행한다.
//! 구판 프로세스 닫기(`ActiveSet`)는 붙어 있어 저장소의 활성 집합과 다르면 접수·결과·승인 결정을 일시 실패로 닫는다.
use super::{runner, watched::Watched};
use crate::{
    host::HostFile,
    install::Installs,
    settings::Settings,
    validate::{
        entrance_secrets, has_private_triggers, missing_named, weak_http_keys, weak_openai_keys,
    },
};
use bot_broker::{
    private::{PrivateGate, PrivateParts},
    rate::RateConfig,
    seal::JcsSealer,
    sys::SystemClock,
};
use bot_core::{
    api::ApiRuns,
    cards::CardOutbox,
    id::Revision,
    intake::{Outbox, PrivateCall, PrivateDecision, PrivateIntake},
    ledger::{CallLedger, PrivateLedger},
    port::{Broker, EffectJournal, Error, Reply, RunAdmin, RunStore},
    profile::{ActiveRevisions, Adapter, ProfileBundles, ProfileCatalog},
    slack::BotThreads,
    trace::Tracer,
};
use bot_gateway::{
    active::{ActiveCheck, ActiveSet},
    cli::{CliGateway, socket},
    delivery::Deliverer,
    http::HttpGateway,
    ingress::Ingress,
    slack::{SlackGateway, api::SlackApi, cards::CardDeliverer},
};
use bot_profile::{Registry, SchemaValidator};
use bot_store::Store;
use std::sync::Arc;
use tokio::{net::TcpListener, net::UnixListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

pub struct Deps {
    pub settings: Settings,
    pub host: Arc<HostFile>,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// 가림 장치를 거치는 보관소(`Observe.secrets`)만 받는다 — 날 보관소는 넘길 수 없다
    pub secrets: Arc<Watched>,
    pub uid: u32,
    pub stop: CancellationToken,
    /// 시작할 때 검증한 활성화 판본(프로필 집합 + 입구 매핑). 저장소의 활성 판본과 견주는 기준이다.
    pub set_revision: Revision,
    /// 승인 결정을 맡길 브로커(설치 판본을 알면 실제 브로커, 모르면 결정을 맡기지 않는 것)
    pub decisions: Arc<dyn Broker>,
    /// 접수·회신·승인 스팬을 보낼 곳(호스트에 설정한 수집기, 없으면 아무것도 내보내지 않는다)
    pub tracer: Arc<dyn Tracer>,
    /// 가짜 Slack 서버 주소(시험 전용, 제품은 `None`)
    pub slack_base: Option<String>,
    /// 이 프로세스의 활성화 판본 확인(민감 DM 접수가 쓰는 원격 도구 서버 요청 직전에 다시 확인한다)
    pub active: Arc<dyn ActiveCheck>,
}

/// 민감 트리거가 없는 호스트의 자리. 민감 접수가 올 수 없으므로 받지 않는다(ack하지 않아 Slack이 다시 보낸다).
struct UnwiredPrivate;

impl PrivateIntake for UnwiredPrivate {
    fn receive(&self, _: PrivateCall) -> Reply<'_, PrivateDecision> {
        Box::pin(std::future::ready(Err(Error::Unavailable)))
    }
}

/// 민감 DM 접수(`PrivateGate`): 민감 트리거가 있는 프로필이 하나라도 있으면 전용 원장(저장소)·도구 실행기(이 프로세스의 srt 샌드박스)·프로필별
/// 지문 키(`private-intake`)로 연다. 본문은 이 프로세스의 메모리와 지정한 도구에만 가고 저장소에는 접수 키와 HMAC 지문만 남는다.
/// 없으면 도구도 srt도 필요 없다. 도구 설치본의 사용 잠금은 접수 입구와 **같은 수명**으로 쥔다(`Held`).
async fn private_intake(deps: &Deps) -> Result<Arc<dyn PrivateIntake>, String> {
    if !deps
        .registry
        .active()
        .values()
        .any(|snapshot| has_private_triggers(snapshot))
    {
        return Ok(Arc::new(UnwiredPrivate));
    }
    let (runner, installs) = runner::build(
        &deps.host,
        &deps.settings,
        &deps.secrets,
        deps.store.clone() as Arc<dyn EffectJournal>,
        deps.uid,
        deps.active.clone(),
    )
    .await?;
    let gate = PrivateGate::new(
        PrivateParts {
            catalog: deps.registry.clone() as Arc<dyn ProfileCatalog>,
            ledger: deps.store.clone() as Arc<dyn PrivateLedger>,
            secrets: deps.secrets.clone(),
            sealer: Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new()))),
            runner,
            tracer: deps.tracer.clone(),
            clock: Arc::new(SystemClock),
        },
        RateConfig::default(),
    );
    Ok(Arc::new(Held {
        gate,
        _installs: installs,
    }))
}

/// 민감 접수 입구와 그 도구 설치본의 사용 잠금. 접수 입구(`Ingress`)를 쥔 처리가 하나라도 남아 있는 동안(진행 중인 마지막 호출까지) 잠금이 풀리지 않는다 —
/// 어긋난 설치본을 복구하려는 다른 기동이 실행 중인 도구의 파일을 지우지 못한다.
struct Held {
    gate: PrivateGate,
    _installs: Installs,
}

impl PrivateIntake for Held {
    fn receive(&self, call: PrivateCall) -> Reply<'_, PrivateDecision> {
        self.gate.receive(call)
    }
}

/// 점검을 마치고 리스너를 확보한 gateway. `open` 전에는 아무 요청도 받지 않는다.
pub struct Prepared {
    ingress: Arc<Ingress>,
    store: Arc<Store>,
    secrets: Arc<Watched>,
    stop: CancellationToken,
    slack: Option<Arc<SlackGateway>>,
    cli: Option<UnixListener>,
    http: Option<TcpListener>,
}

/// 점검하고 리스너를 확보한다. 하나라도 실패하면 Err — 아직 아무것도 열리지 않았고 저장소도 건드리지 않았다.
pub async fn prepare(deps: &Deps) -> Result<Prepared, String> {
    let private = private_intake(deps).await?;
    let ingress = Arc::new(
        Ingress::new(
            deps.registry.clone() as Arc<dyn ProfileCatalog>,
            Arc::new(deps.host.gateway.clone()),
            deps.store.clone() as Arc<dyn RunStore>,
            private,
        )
        .with_tracer(deps.tracer.clone())
        .with_active(Arc::new(ActiveSet::new(
            deps.store.clone() as Arc<dyn ActiveRevisions>,
            deps.set_revision.clone(),
        ))),
    );
    let routes = ingress.routes().map_err(|e| e.to_string())?;
    let (mut cli, mut http, mut slack) = (false, false, false);
    let mut problems = Vec::new();
    let mut snapshots = Vec::new();
    for snapshot in routes.profiles() {
        for binding in snapshot.bindings.values() {
            match binding.source {
                Adapter::Cli { .. } => cli = true,
                Adapter::Http { .. } | Adapter::OpenAi { .. } => http = true,
                Adapter::SlackChannel { .. } | Adapter::SlackDm { .. } => slack = true,
            }
        }
        snapshots.push(snapshot.clone());
    }
    drop(routes);
    // 입구가 쓰는 자격은 열기 전에 모두 있어야 하고(없으면 이름과 환경 변수명을 알린다) HTTP 키는 하한 이상이어야 한다
    problems.extend(weak_openai_keys(&*deps.secrets, &snapshots, &deps.host.gateway).await);
    for snapshot in snapshots {
        let names = entrance_secrets(&snapshot, &deps.host.gateway);
        problems.extend(
            missing_named(&*deps.secrets, &snapshot.id, names, &deps.settings.secrets).await,
        );
        problems.extend(weak_http_keys(&*deps.secrets, &snapshot, &deps.host.gateway).await);
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    // Slack 설치 검사(토큰·팀·앱 중복)도 열기 전에 끝낸다
    let slack_gateway = if slack {
        let gateway = Arc::new(
            SlackGateway::new(
                ingress.clone(),
                deps.decisions.clone(),
                deps.store.clone() as Arc<dyn CallLedger>,
                deps.secrets.clone(),
                SlackApi::new(deps.slack_base.as_deref()),
            )
            // 봇이 참여한 스레드의 증거(Slack `thread_reply` 이벤트용)는 같은 저장소에 둔다
            .with_threads(deps.store.clone() as Arc<dyn BotThreads>)
            // Slack 진행 상태(👀/✅ · `chat.startStream`)와 스레드 중단(버튼 · "중단해줘")
            .with_ux(
                deps.store.clone() as Arc<dyn RunStore>,
                deps.store.clone() as Arc<dyn RunAdmin>,
            ),
        );
        gateway.plan().await.map_err(|e| e.to_string())?;
        Some(gateway)
    } else {
        None
    };
    let cli_listener = if cli {
        let path = socket::socket_path(
            deps.settings
                .socket
                .as_deref()
                .or(deps.host.gateway.cli.socket.as_deref()),
            &|name| std::env::var(name).ok(),
            deps.uid,
        )?;
        let listener = socket::bind(&path, deps.uid)
            .map_err(|e| format!("CLI 소켓을 열지 못했어요({}): {e}", path.display()))?;
        eprintln!("게이트웨이: CLI 소켓 {}", path.display());
        Some(listener)
    } else {
        None
    };
    let http_listener = if http {
        let addr = deps.host.gateway.listen();
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("HTTP 입구를 열지 못했어요({addr}): {e}"))?;
        eprintln!("게이트웨이: HTTP {addr}");
        Some(listener)
    } else {
        None
    };
    if !(cli || http || slack) {
        eprintln!("게이트웨이: 연결된 입구가 없어요(프로필에 바인딩이 없어요)");
    }
    Ok(Prepared {
        ingress,
        store: deps.store.clone(),
        secrets: deps.secrets.clone(),
        stop: deps.stop.clone(),
        slack: slack_gateway,
        cli: cli_listener,
        http: http_listener,
    })
}

impl Prepared {
    /// 접수를 연다. 활성 판본·시드를 확정한 뒤에 부른다. 이 안에서는 실패할 일이 없다.
    pub fn open(self, tasks: &mut JoinSet<Result<(), String>>) {
        let Self {
            ingress,
            store,
            secrets,
            stop,
            slack,
            cli,
            http,
        } = self;
        if let Some(listener) = cli {
            let gateway = Arc::new(CliGateway::new(
                ingress.clone(),
                store.clone() as Arc<dyn RunStore>,
            ));
            let stop = stop.clone();
            tasks.spawn(async move {
                gateway.serve(listener, stop).await;
                Ok(())
            });
        }
        if let Some(listener) = http {
            let gateway = Arc::new(
                HttpGateway::new(ingress.clone(), store.clone() as Arc<dyn RunStore>, secrets)
                    .with_openai(
                        store.clone() as Arc<dyn ApiRuns>,
                        store.clone() as Arc<dyn ProfileBundles>,
                    ),
            );
            let stop = stop.clone();
            tasks.spawn(async move {
                gateway
                    .serve(listener, stop)
                    .await
                    .map_err(|e| format!("HTTP 입구가 멈췄어요: {e}"))
            });
        }
        if let Some(gateway) = slack {
            let deliverer = Deliverer::new(
                store.clone() as Arc<dyn Outbox>,
                ingress.clone(),
                Arc::new(gateway.sender()),
            );
            // 채널에서 시작한 예약의 승인 카드는 소유자 DM으로 전달한다(같은 설치·같은 저장소 핸들)
            let cards = CardDeliverer::new(store as Arc<dyn CardOutbox>, ingress, gateway.sender());
            tasks.spawn({
                let stop = stop.clone();
                async move { gateway.serve(stop).await.map_err(|e| e.to_string()) }
            });
            tasks.spawn({
                let stop = stop.clone();
                async move {
                    cards.run(stop).await;
                    Ok(())
                }
            });
            tasks.spawn(async move {
                deliverer.run(stop).await;
                Ok(())
            });
        }
    }
}
