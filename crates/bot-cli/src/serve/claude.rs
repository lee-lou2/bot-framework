//! `claude-cli` 모델 실행의 호스트 조립: 브로커 프런트(모델 중계·MCP 중계)와 시도별 세션 발급(`Relays`), CLI 실행기, 대기 통지(`Park`).
//!
//! 모델 CLI는 비밀도 네트워크도 없는 샌드박스에서 돌고 프런트 한 곳에만 닿는다. 프런트가 서비스 키를 상류 요청 머리에만 싣는다
//! (`ModelProxy`). 호스트 `providers`의 `claude-cli` 항목(상류 `model`·HTTPS `endpoint`·프로필 자격 이름 `secret`)이 프록시의 제공자
//! 표가 되고, 프로필이 허용한 별칭만 그 제공자를 쓴다.
//!
//! **운영 어댑터가 아직 없는 두 조각**은 호출하는 쪽이 `ClaudeParts`로 준다 — 모델 샌드박스(Apple VM 어댑터)와 상류 HTTPS 전송이다.
//! 주지 않으면 `claude-cli`는 시작을 거절한다(srt 등으로 대신하지 않는다). 시험은 합성 샌드박스·가짜 상류를 넣는다.
use super::{guard::GuardedUpstream, watched::Watched};
use crate::{
    execution::Provider,
    host::HostFile,
    settings::SecretsKind,
    validate::{missing_named, model_secrets},
};
use bot_broker::{
    front::{
        FrontConfig, FrontParts, ModelFront, Session, SessionTable,
        cli::PlannedCalls,
        observe::CheckpointSink,
        park::{Park, ParkReason},
    },
    model::{ModelHost, ModelLimits, ModelProxy, Provider as UpstreamProvider, Upstream},
    resolve::HostPort,
    service::preflight::Preflight,
    sys::{OsEntropy, SystemClock},
};
use bot_core::{
    id::{AgentId, Attempt, ProfileId, SecretName},
    ledger::CallLedger,
    port::{Broker, DelegationStore, RunStore, SecretStore},
    profile::ProfileCatalog,
    sandbox::ModelSandbox,
};
use bot_gateway::active::ActiveCheck;
use bot_profile::Registry;
use bot_runtime::{
    Relays,
    model::{ClaudeCodeRunner, CliConfig, CliModel, ModelRunner, RelayChannel, RelayGrant},
};
use bot_store::Store;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// 호스트 설정에 필드가 생기기 전까지의 CLI 실행 기본값
const EFFORT: &str = "medium";
const MAX_OUTPUT_TOKENS: u32 = 8192;
const RETRIES: u32 = 1;
const CONTEXT_WINDOW: u64 = 200_000;
/// 상류 요청 경로의 기본값(`endpoint`에 경로가 없을 때)
const DEFAULT_PATH: &str = "/v1/messages";
/// 프런트가 모델 CLI의 도구 사용을 저장된 계획과 대조하며 기다리는 시간
const PLANNED_WAIT: Duration = Duration::from_secs(20);

/// 모델 실행의 조각과 이 호스트의 자리. 운영은 호스트 설정에서 `operational`이 만들고(Apple VM 어댑터 + HTTPS 상류 전송), 시험은 합성 샌드박스·가짜 상류를 넣는다.
#[derive(Clone)]
pub struct ClaudeParts {
    /// 모델 CLI를 띄우는 샌드박스를 얻는 방법(운영은 Apple VM만)
    pub launch: Launch,
    /// 상류(모델 제공자)로 보내는 전송
    pub upstream: Arc<dyn Upstream>,
    /// 호출별 입력·임시 폴더를 만드는 호스트 폴더(VM이면 어댑터의 마운트 루트와 같다)
    pub workspace: PathBuf,
    /// 대기 통지를 받을 호스트 쪽(없으면 새로 만든다). 호출하는 쪽이 같은 값을 쥐고 통지를 확인할 수 있다.
    pub park: Option<Arc<HostPark>>,
}

mod launch;
use launch::Vm;
pub use launch::{Launch, operational};

/// 조립에 쓰는 이 프로세스의 포트들
pub struct Input<'a> {
    pub host: &'a HostFile,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// 가림 장치를 거치는 보관소(`Observe.secrets`)만 받는다 — 모델 서비스 키를 읽는 길이라 날 보관소는 넘길 수 없다
    pub secrets: Arc<Watched>,
    /// 프런트가 도구 호출·승인·위임을 맡길 브로커(구판 보호가 걸린 것)
    pub broker: Arc<dyn Broker>,
    /// 모델 CLI의 호출이 실행 전에 확정적으로 거절되는지 가리는 사전 판정(운영은 도구를 실행하는 브로커). 붙으면 확정 거절에 한해 프런트가 run
    /// 체크포인트에 표식을 남기고 재개가 모델이 본 단어로 그 자리를 마무리한다.
    pub preflight: Arc<dyn Preflight>,
    pub runs: Arc<dyn RunStore>,
    /// 이 프로세스의 활성화 판본이 저장소의 활성 판본과 같은지 확인한다. 호스트 설정(상류 목적지)이 바뀌어 구판이 되면 이미 집은 시도라도
    /// 모델 상류로 새 요청을 보내지 않고 열린 스트림도 끊는다.
    pub active: Arc<dyn ActiveCheck>,
}

/// 프로필이 허용한 `claude-cli` 별칭의 서비스 키가 보관소에 모두 있는지 본다(값은 읽어 보되 남기지 않는다). 없거나 읽지 못한 것의 안내
/// 문구들 — 비어 있으면 모두 있다. worker는 활성 판본을 게시하기 **전에** 부른다(키 없이 모든 모델 요청이 런타임에 실패하는 채로 열지 않는다).
pub async fn missing_keys(
    host: &HostFile,
    registry: &Registry,
    secrets: &dyn SecretStore,
    kind: &SecretsKind,
) -> Vec<String> {
    let mut problems = Vec::new();
    for (profile, snapshot) in registry.active().iter() {
        let names = model_secrets(snapshot, host);
        if !names.is_empty() {
            problems.extend(missing_named(secrets, profile, names, kind).await);
        }
    }
    problems
}

/// 호스트가 모델 CLI를 쓰는 별칭이 있는가: 프로필이 허용한 별칭 중 `claude-cli`로 실행하는 것
pub fn needed(host: &HostFile) -> bool {
    host.catalog.models.values().flatten().any(|alias| {
        host.providers
            .get(alias)
            .is_some_and(Provider::needs_model_vm)
    })
}

/// 상류 주소(`https://호스트[:포트][/경로]`)를 제공자의 목적지와 요청 경로로 나눈다. 포트를 생략하면 443, 경로를 생략하면 기본 경로다.
fn upstream_target(endpoint: &str) -> Option<(HostPort, String)> {
    let rest = endpoint.strip_prefix("https://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) if !path.is_empty() => (authority, format!("/{path}")),
        Some((authority, _)) => (authority, DEFAULT_PATH.to_owned()),
        None => (rest, DEFAULT_PATH.to_owned()),
    };
    let target = if authority.contains(':') {
        authority.to_owned()
    } else {
        format!("{authority}:443")
    };
    Some((HostPort::parse(&target)?, path))
}

/// 프록시의 제공자 표: 별칭마다 제공자 하나(이름은 별칭, 모델 별칭 → 상류 모델), 프로필은 자기가 허용한 별칭의 제공자만 쓴다.
pub fn model_host(host: &HostFile) -> Result<ModelHost, String> {
    let mut providers = BTreeMap::new();
    for (alias, provider) in &host.providers {
        let Provider::ClaudeCli {
            model,
            endpoint,
            secret,
        } = provider
        else {
            continue;
        };
        let (target, path) = upstream_target(endpoint)
            .ok_or_else(|| format!("providers.{alias}.endpoint를 해석할 수 없어요"))?;
        let secret = SecretName::parse(secret)
            .ok_or_else(|| format!("providers.{alias}.secret이 자격 이름 규칙에 맞지 않아요"))?;
        providers.insert(
            alias.clone(),
            UpstreamProvider {
                endpoint: target,
                path,
                secret,
                header: "x-api-key".into(),
                scheme: None,
                headers: Vec::new(),
                forward_headers: vec!["anthropic-version".into(), "anthropic-beta".into()],
                models: BTreeMap::from([(alias.clone(), model.clone())]),
                body_fields: Vec::new(),
            },
        );
    }
    let allow: BTreeMap<ProfileId, BTreeSet<String>> = host
        .catalog
        .models
        .iter()
        .map(|(profile, aliases)| {
            (
                ProfileId(profile.clone()),
                aliases
                    .iter()
                    .filter(|alias| providers.contains_key(*alias))
                    .cloned()
                    .collect(),
            )
        })
        .collect();
    Ok(ModelHost { providers, allow })
}

/// 시도가 대기(승인·보류·자식)에 들어갔다는 통지를 받는 호스트 쪽. 프런트가 이미 그 시도의 세션을 거뒀고(CLI의 모델 호출은 거절된다)
/// 원장이 run을 대기로 보냈으므로 워커의 하트비트가 CLI를 끝낸다 — 여기서 따로 끝낼 것은 없다. 흔적(추적 한 줄)과 계측 카운터·최근
/// 통지 몇 건만 남긴다.
#[derive(Default)]
pub struct HostPark {
    approval: AtomicU64,
    held: AtomicU64,
    child: AtomicU64,
    recent: Mutex<VecDeque<(Attempt, ParkReason)>>,
}

/// 기억하는 최근 통지 수
const RECENT_PARKS: usize = 32;

impl HostPark {
    /// 최근 통지(오래된 것부터, 최대 32건): 어느 시도가 왜 대기에 들어갔는가
    pub fn recent(&self) -> Vec<(Attempt, ParkReason)> {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 사유별 통지 수: (승인 대기, 보류, 자식 대기)
    pub fn counts(&self) -> (u64, u64, u64) {
        (
            self.approval.load(Ordering::SeqCst),
            self.held.load(Ordering::SeqCst),
            self.child.load(Ordering::SeqCst),
        )
    }
}

impl Park for HostPark {
    fn parked(&self, attempt: &Attempt, reason: ParkReason) {
        let counter = match reason {
            ParkReason::Approval => &self.approval,
            ParkReason::Held => &self.held,
            ParkReason::Child => &self.child,
        };
        counter.fetch_add(1, Ordering::SeqCst);
        {
            let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
            if recent.len() == RECENT_PARKS {
                recent.pop_front();
            }
            recent.push_back((attempt.clone(), reason));
        }
        tracing::info!(
            profile = %attempt.profile.0,
            run = %attempt.run.0,
            reason = ?reason,
            "시도가 대기에 들어갔어요"
        );
    }
}

/// 이 시도에 세션 토큰을 발급해 프런트 연결을 여는 `Relays`. 돌려준 열쇠를 놓으면 이 시도가 발급받은 토큰만 폐기한다.
struct FrontRelays {
    sessions: Arc<SessionTable>,
    addr: SocketAddr,
}

struct Revoke {
    sessions: Arc<SessionTable>,
    attempt: Attempt,
}

impl Drop for Revoke {
    fn drop(&mut self) {
        self.sessions.revoke_attempt(&self.attempt);
    }
}

impl Relays for FrontRelays {
    fn open(&self, attempt: &Attempt, agent: &AgentId, alias: &str) -> Option<RelayGrant> {
        let token = self.sessions.issue(Session {
            attempt: attempt.clone(),
            agent: agent.clone(),
            alias: alias.to_owned(),
        })?;
        Some(RelayGrant::new(
            RelayChannel {
                name: "relay".into(),
                base_url: format!("http://{}", self.addr),
                token,
            },
            Revoke {
                sessions: self.sessions.clone(),
                attempt: attempt.clone(),
            },
        ))
    }
}

/// 시작한 프런트와 그 위의 실행기·세션 발급·대기 통지. 버리면 프런트도 닫힌다.
pub struct Claude {
    pub runner: Arc<dyn ModelRunner>,
    pub relays: Arc<dyn Relays>,
    pub park: Arc<HostPark>,
    /// 발급한 세션 토큰의 표와 모델 샌드박스: 종료 정리가 이 시도의 세션을 거두고 VM이 끝났음을 확인하는 데 쓴다(`Quiescer`)
    pub sessions: Arc<SessionTable>,
    pub sandbox: Arc<ModelSandbox>,
    /// Apple VM 어댑터(운영): 끝낼 때 닻과 이번 실행의 VM을 치우고 인스턴스 소유권을 놓는다
    vm: Option<Vm>,
    front: ModelFront,
}

impl Claude {
    pub fn addr(&self) -> SocketAddr {
        self.front.addr()
    }

    /// 새 연결을 받지 않고 진행 중인 연결도 끝낸 뒤, VM 어댑터면 닻과 VM을 치운다.
    pub async fn stop(self) {
        self.front.stop().await;
        if let Some(vm) = &self.vm {
            vm.shutdown().await;
        }
    }
}

/// 프런트를 열고 CLI 실행기를 만든다. 주소를 잡지 못하거나 제공자 표를 만들 수 없으면 Err(아직 아무 일도 집지 않았다).
pub async fn start(parts: ClaudeParts, input: Input<'_>) -> Result<Claude, String> {
    let ledger = input.store.clone() as Arc<dyn CallLedger>;
    let model = Arc::new(ModelProxy::new(
        model_host(input.host)?,
        ledger.clone(),
        input.secrets,
        // 상류로 나가는 모든 요청 직전에 호스트 활성화 판본을 확인하고 열린 스트림도 주기적으로 다시 확인한다
        Arc::new(GuardedUpstream::new(parts.upstream, input.active)),
        Arc::new(SystemClock),
        ModelLimits::default(),
    ));
    let sessions = SessionTable::new(Arc::new(OsEntropy));
    let park = parts.park.clone().unwrap_or_default();
    // VM이면 여기서 닻을 먼저 띄운다(프런트는 닻이 지키는 게이트웨이 주소에만 열 수 있다). 이 뒤의 실패는 모두 닻을 치우고 돌아간다.
    let (bind, confined_reads, pending) = parts.launch.prepare().await?;
    let front = match ModelFront::start(
        bind,
        FrontParts {
            sessions: sessions.clone(),
            model,
            broker: input.broker,
            catalog: input.registry as Arc<dyn ProfileCatalog>,
            calls: Arc::new(
                PlannedCalls::new(ledger.clone(), PLANNED_WAIT).with_preflight(input.preflight),
            ),
            turns: Arc::new(CheckpointSink::new(input.runs)),
            ledger,
            delegation: input.store as Arc<dyn DelegationStore>,
            park: park.clone(),
        },
        FrontConfig::default(),
    )
    .await
    {
        Ok(front) => front,
        Err(error) => {
            pending.abort().await;
            return Err(format!("모델 프런트를 열지 못했어요({bind}): {error}"));
        }
    };
    // 프런트가 실제로 연 포트가 릴레이 주소의 단일 원본이다
    let (sandbox, vm) = match pending.open(front.addr().port()).await {
        Ok(opened) => opened,
        Err(error) => {
            front.stop().await;
            return Err(error);
        }
    };
    let mut config = CliConfig::new(parts.workspace);
    if confined_reads {
        config = config.with_confined_reads();
    }
    for (alias, provider) in &input.host.providers {
        if let Provider::ClaudeCli { model, .. } = provider {
            config = config.with_model(
                alias,
                CliModel {
                    id: model.clone(),
                    effort: EFFORT.into(),
                    max_output_tokens: MAX_OUTPUT_TOKENS,
                    retries: RETRIES,
                    context_window: CONTEXT_WINDOW,
                },
            );
        }
    }
    let relays = Arc::new(FrontRelays {
        sessions: sessions.clone(),
        addr: front.addr(),
    });
    Ok(Claude {
        runner: Arc::new(ClaudeCodeRunner::new(sandbox.clone(), config)),
        relays,
        park,
        sessions,
        sandbox,
        vm,
        front,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host;

    fn parsed(extra: &str) -> HostFile {
        host::parse(&format!(
            r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{extra}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn the_upstream_endpoint_is_split_into_target_and_path() {
        let (target, path) = upstream_target("https://api.example").unwrap();
        assert_eq!((target.host.as_str(), target.port), ("api.example", 443));
        assert_eq!(path, "/v1/messages");
        let (target, path) = upstream_target("https://api.example:8443/proxy/v1/messages").unwrap();
        assert_eq!((target.host.as_str(), target.port), ("api.example", 8443));
        assert_eq!(path, "/proxy/v1/messages");
        assert_eq!(
            upstream_target("https://api.example/").unwrap().1,
            DEFAULT_PATH
        );
        assert!(upstream_target("http://api.example").is_none());
        assert!(upstream_target("https://api.example:99999").is_none());
    }

    #[test]
    fn only_claude_aliases_become_upstream_providers_and_profiles_keep_their_own_allow_list() {
        let host = parsed(
            r#", "providers": {
                "assistant": {"kind": "claude-cli", "model": "model-large", "endpoint": "https://api.example", "secret": "model-key"},
                "plain": {"kind": "demo"},
                "other": {"kind": "claude-cli", "model": "model-small", "endpoint": "https://api.example:8443/v1/messages", "secret": "other-key"}}"#,
        );
        assert!(needed(&host));
        let table = model_host(&host).unwrap();
        assert_eq!(
            table.providers.keys().cloned().collect::<Vec<_>>(),
            ["assistant", "other"]
        );
        let assistant = &table.providers["assistant"];
        assert_eq!(assistant.models["assistant"], "model-large");
        assert_eq!(assistant.secret, SecretName::parse("model-key").unwrap());
        assert_eq!(assistant.header, "x-api-key");
        // 프로필이 허용한 별칭 중 claude-cli인 것만 쓸 수 있다(데모 별칭 `plain`은 프록시가 모른다)
        let personal = &table.allow[&ProfileId("personal".into())];
        assert_eq!(personal.iter().cloned().collect::<Vec<_>>(), ["assistant"]);
        let work = &table.allow[&ProfileId("work".into())];
        assert_eq!(work.iter().cloned().collect::<Vec<_>>(), ["other"]);
    }

    #[test]
    fn a_host_without_claude_aliases_does_not_need_the_front() {
        let demo =
            parsed(r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#);
        assert!(!needed(&demo));
        assert!(model_host(&demo).unwrap().providers.is_empty());
        // 프로필이 허용하지 않은 claude-cli 별칭은 필요로 치지 않는다
        let unused = parsed(
            r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"},
                "spare": {"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "k"}}"#,
        );
        assert!(!needed(&unused));
    }

    #[test]
    fn the_park_notice_counts_by_reason() {
        let park = HostPark::default();
        let attempt = Attempt {
            profile: ProfileId("personal".into()),
            run: bot_core::id::RunId("run-1".into()),
            claim: None,
            generation: bot_core::id::Generation(1),
        };
        park.parked(&attempt, ParkReason::Approval);
        park.parked(&attempt, ParkReason::Child);
        park.parked(&attempt, ParkReason::Child);
        assert_eq!(park.counts(), (1, 0, 2));
        assert_eq!(
            park.recent().iter().map(|(_, r)| *r).collect::<Vec<_>>(),
            [ParkReason::Approval, ParkReason::Child, ParkReason::Child]
        );
        // 최근 통지는 32건까지만 기억한다
        for _ in 0..40 {
            park.parked(&attempt, ParkReason::Held);
        }
        assert_eq!(park.recent().len(), 32);
        assert_eq!(park.counts().1, 40);
    }
}
