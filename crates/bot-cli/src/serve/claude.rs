//! Host assembly for `claude-cli` model runs (broker relay front, Apple VM, and park tracking).
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
    active::ActiveCheck,
    id::{AgentId, Attempt, ProfileId, SecretName},
    ledger::CallLedger,
    port::{Broker, DelegationStore, RunStore, SecretStore},
    profile::ProfileCatalog,
    sandbox::ModelSandbox,
};
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

const EFFORT: &str = "medium";
const MAX_OUTPUT_TOKENS: u32 = 8192;
const RETRIES: u32 = 1;
const CONTEXT_WINDOW: u64 = 200_000;
const DEFAULT_PATH: &str = "/v1/messages";
const PLANNED_WAIT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct ClaudeParts {
    pub launch: Launch,
    pub upstream: Arc<dyn Upstream>,
    pub workspace: PathBuf,
    pub park: Option<Arc<HostPark>>,
}

mod launch;
use launch::Vm;
pub use launch::{Launch, operational};

pub struct Input<'a> {
    pub host: &'a HostFile,
    pub registry: Arc<Registry>,
    pub store: Arc<Store>,
    /// Accepts `Watched` only so every read model key is registered in the redactor before use.
    pub secrets: Arc<Watched>,
    pub broker: Arc<dyn Broker>,
    pub preflight: Arc<dyn Preflight>,
    pub runs: Arc<dyn RunStore>,
    pub active: Arc<dyn ActiveCheck>,
}

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

pub fn needed(host: &HostFile) -> bool {
    host.catalog.models.values().flatten().any(|alias| {
        host.providers
            .get(alias)
            .is_some_and(Provider::needs_model_vm)
    })
}

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

pub fn model_host(host: &HostFile) -> Result<ModelHost, String> {
    let mut providers = BTreeMap::new();
    for (alias, provider) in &host.providers {
        let Provider::ClaudeCli {
            model,
            endpoint,
            secret,
            effort: _,
        } = provider
        else {
            continue;
        };
        let (target, path) = upstream_target(endpoint)
            .ok_or_else(|| format!("cannot parse providers.{alias}.endpoint"))?;
        let secret = SecretName::parse(secret)
            .ok_or_else(|| format!("providers.{alias}.secret is not a valid secret name"))?;
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

/// Records park notifications from the model front; session revocation and worker heartbeat handle stopping the CLI.
#[derive(Default)]
pub struct HostPark {
    approval: AtomicU64,
    held: AtomicU64,
    child: AtomicU64,
    recent: Mutex<VecDeque<(Attempt, ParkReason)>>,
}

const RECENT_PARKS: usize = 32;

impl HostPark {
    pub fn recent(&self) -> Vec<(Attempt, ParkReason)> {
        self.recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

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
            "attempt parked"
        );
    }
}

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

pub struct Claude {
    pub runner: Arc<dyn ModelRunner>,
    pub relays: Arc<dyn Relays>,
    pub park: Arc<HostPark>,
    pub sessions: Arc<SessionTable>,
    pub sandbox: Arc<ModelSandbox>,
    vm: Option<Vm>,
    front: ModelFront,
}

impl Claude {
    pub fn addr(&self) -> SocketAddr {
        self.front.addr()
    }

    pub async fn stop(self) {
        self.front.stop().await;
        if let Some(vm) = &self.vm {
            vm.shutdown().await;
        }
    }
}

pub async fn start(parts: ClaudeParts, input: Input<'_>) -> Result<Claude, String> {
    let ledger = input.store.clone() as Arc<dyn CallLedger>;
    let model = Arc::new(ModelProxy::new(
        model_host(input.host)?,
        ledger.clone(),
        input.secrets,
        // Guard every upstream request and open stream against stale activation revisions.
        Arc::new(GuardedUpstream::new(parts.upstream, input.active)),
        Arc::new(SystemClock),
        ModelLimits::default(),
    ));
    let sessions = SessionTable::new(Arc::new(OsEntropy));
    let park = parts.park.clone().unwrap_or_default();
    // Start the VM network anchor first so the front binds strictly to the gateway address it guards.
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
            return Err(format!("failed to start model front ({bind}): {error}"));
        }
    };
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
        if let Provider::ClaudeCli { model, effort, .. } = provider {
            config = config.with_model(
                alias,
                CliModel {
                    id: model.clone(),
                    effort: effort
                        .map_or(EFFORT, bot_runtime::model::Effort::as_str)
                        .into(),
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
        for _ in 0..40 {
            park.parked(&attempt, ParkReason::Held);
        }
        assert_eq!(park.recent().len(), 32);
        assert_eq!(park.counts().1, 40);
    }
}
