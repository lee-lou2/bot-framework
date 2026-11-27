//! Host assembly for VM-hosted CLI model runs (`claude-cli`, `codex-cli`, `agy-cli`): broker relay front, Apple VM, and park tracking.
use super::{guard::GuardedUpstream, watched::Watched};
use crate::{
    execution::{CliProvider, Provider, reachable_aliases},
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
    model::{ModelHost, ModelLimits, ModelProxy, Protocol, Provider as UpstreamProvider, Upstream},
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
    model::{
        AgyRunner, ClaudeCodeRunner, CliConfig, CliKind, CliModel, CodexRunner, ModelRunner,
        RelayChannel, RelayGrant,
    },
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
const CLAUDE_DEFAULT_PATH: &str = "/v1/messages";
const CODEX_DEFAULT_PATH: &str = "/v1/responses";
const AGY_DEFAULT_PATH: &str = "/v1beta";
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
    let roots = host.catalog.models.values().flatten().map(String::as_str);
    reachable_aliases(&host.providers, roots)
        .into_iter()
        .any(|alias| {
            host.providers
                .get(&alias)
                .is_some_and(Provider::needs_model_vm)
        })
}

fn cli_protocol(kind: CliKind) -> Protocol {
    match kind {
        CliKind::Claude => Protocol::Anthropic,
        CliKind::Codex => Protocol::OpenAiResponses,
        CliKind::Agy => Protocol::Gemini,
    }
}

fn upstream_target(kind: CliKind, endpoint: &str) -> Option<(HostPort, String)> {
    let rest = endpoint.strip_prefix("https://")?;
    let (authority, raw_path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    let path = match kind {
        CliKind::Claude => {
            if raw_path.is_empty() {
                CLAUDE_DEFAULT_PATH.to_owned()
            } else {
                format!("/{raw_path}")
            }
        }
        CliKind::Codex => {
            let trimmed = raw_path.trim_end_matches('/');
            if trimmed.is_empty() || trimmed == "v1" {
                CODEX_DEFAULT_PATH.to_owned()
            } else if trimmed.ends_with("/v1") {
                format!("/{trimmed}/responses")
            } else {
                format!("/{raw_path}")
            }
        }
        CliKind::Agy => {
            let trimmed = raw_path.trim_end_matches('/');
            let base = trimmed.strip_suffix("/models").unwrap_or(trimmed);
            if base.is_empty() {
                AGY_DEFAULT_PATH.to_owned()
            } else {
                format!("/{base}")
            }
        }
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
        let Provider::Cli(CliProvider {
            kind,
            model,
            endpoint,
            secret,
            effort: _,
            fallback: _,
        }) = provider
        else {
            continue;
        };
        let (target, path) = upstream_target(*kind, endpoint)
            .ok_or_else(|| format!("cannot parse providers.{alias}.endpoint"))?;
        let secret = SecretName::parse(secret)
            .ok_or_else(|| format!("providers.{alias}.secret is not a valid secret name"))?;
        let (header, scheme, forward_headers) = match kind {
            CliKind::Claude => (
                "x-api-key".into(),
                None,
                vec!["anthropic-version".into(), "anthropic-beta".into()],
            ),
            CliKind::Codex => (
                "authorization".into(),
                Some("Bearer".into()),
                vec!["openai-beta".into()],
            ),
            CliKind::Agy => ("x-goog-api-key".into(), None, Vec::new()),
        };
        providers.insert(
            alias.clone(),
            UpstreamProvider {
                protocol: cli_protocol(*kind),
                endpoint: target,
                path,
                secret,
                header,
                scheme,
                headers: Vec::new(),
                forward_headers,
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
            let reachable = reachable_aliases(&host.providers, aliases.iter().map(String::as_str));
            (
                ProfileId(profile.clone()),
                reachable
                    .into_iter()
                    .filter(|alias| providers.contains_key(alias))
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
    protocols: BTreeMap<String, Protocol>,
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
        let protocol = *self.protocols.get(alias)?;
        let token = self.sessions.issue(Session {
            attempt: attempt.clone(),
            agent: agent.clone(),
            alias: alias.to_owned(),
            protocol,
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
    pub runners: BTreeMap<CliKind, Arc<dyn ModelRunner>>,
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
    let mut configs: BTreeMap<CliKind, CliConfig> = BTreeMap::new();
    let mut protocols: BTreeMap<String, Protocol> = BTreeMap::new();
    for (alias, provider) in &input.host.providers {
        let Provider::Cli(CliProvider {
            kind,
            model,
            effort,
            ..
        }) = provider
        else {
            continue;
        };
        protocols.insert(alias.clone(), cli_protocol(*kind));
        let entry = configs.entry(*kind).or_insert_with(|| {
            let cfg = CliConfig::new(parts.workspace.clone());
            if confined_reads {
                cfg.with_confined_reads()
            } else {
                cfg
            }
        });
        entry.models.insert(
            alias.clone(),
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
    let mut runners: BTreeMap<CliKind, Arc<dyn ModelRunner>> = BTreeMap::new();
    for (kind, config) in configs {
        let runner: Arc<dyn ModelRunner> = match kind {
            CliKind::Claude => Arc::new(ClaudeCodeRunner::new(sandbox.clone(), config)),
            CliKind::Codex => Arc::new(CodexRunner::new(sandbox.clone(), config)),
            CliKind::Agy => Arc::new(AgyRunner::new(sandbox.clone(), config)),
        };
        runners.insert(kind, runner);
    }
    let relays = Arc::new(FrontRelays {
        sessions: sessions.clone(),
        addr: front.addr(),
        protocols,
    });
    Ok(Claude {
        runners,
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
        let (target, path) = upstream_target(CliKind::Claude, "https://api.example").unwrap();
        assert_eq!((target.host.as_str(), target.port), ("api.example", 443));
        assert_eq!(path, "/v1/messages");
        let (target, path) = upstream_target(
            CliKind::Claude,
            "https://api.example:8443/proxy/v1/messages",
        )
        .unwrap();
        assert_eq!((target.host.as_str(), target.port), ("api.example", 8443));
        assert_eq!(path, "/proxy/v1/messages");
        assert_eq!(
            upstream_target(CliKind::Claude, "https://api.example/")
                .unwrap()
                .1,
            CLAUDE_DEFAULT_PATH
        );
        assert_eq!(
            upstream_target(CliKind::Codex, "https://api.openai.com/v1")
                .unwrap()
                .1,
            "/v1/responses"
        );
        assert_eq!(
            upstream_target(CliKind::Codex, "https://api.openai.com")
                .unwrap()
                .1,
            "/v1/responses"
        );
        assert_eq!(
            upstream_target(CliKind::Agy, "https://generativelanguage.googleapis.com")
                .unwrap()
                .1,
            "/v1beta"
        );
        assert_eq!(
            upstream_target(
                CliKind::Agy,
                "https://generativelanguage.googleapis.com/v1beta/models"
            )
            .unwrap()
            .1,
            "/v1beta"
        );
        assert!(upstream_target(CliKind::Claude, "http://api.example").is_none());
        assert!(upstream_target(CliKind::Claude, "https://api.example:99999").is_none());
    }

    #[test]
    fn cli_aliases_become_protocol_pinned_upstream_providers_per_profile() {
        let host = host::parse(
            r#"{"version": 1, "models": {"personal": ["assistant", "codex", "agy", "plain"], "work": ["other"]},
                "providers": {
                    "assistant": {"kind": "claude-cli", "model": "model-large", "endpoint": "https://api.example", "secret": "model-key"},
                    "codex": {"kind": "codex-cli", "model": "gpt-5", "endpoint": "https://api.openai.com/v1", "secret": "openai-key"},
                    "agy": {"kind": "agy-cli", "model": "gemini-3.1-pro-preview", "endpoint": "https://generativelanguage.googleapis.com", "secret": "gemini-key"},
                    "plain": {"kind": "demo"},
                    "other": {"kind": "claude-cli", "model": "model-small", "endpoint": "https://api.example:8443/v1/messages", "secret": "other-key"}}}"#,
        )
        .unwrap();
        assert!(needed(&host));
        let table = model_host(&host).unwrap();
        assert_eq!(
            table.providers.keys().cloned().collect::<Vec<_>>(),
            ["agy", "assistant", "codex", "other"]
        );
        let assistant = &table.providers["assistant"];
        assert_eq!(assistant.protocol, Protocol::Anthropic);
        assert_eq!(assistant.models["assistant"], "model-large");
        assert_eq!(assistant.secret, SecretName::parse("model-key").unwrap());
        assert_eq!(assistant.header, "x-api-key");
        let codex = &table.providers["codex"];
        assert_eq!(codex.protocol, Protocol::OpenAiResponses);
        assert_eq!(codex.path, "/v1/responses");
        assert_eq!(codex.header, "authorization");
        assert_eq!(codex.scheme.as_deref(), Some("Bearer"));
        let agy = &table.providers["agy"];
        assert_eq!(agy.protocol, Protocol::Gemini);
        assert_eq!(agy.path, "/v1beta");
        assert_eq!(agy.header, "x-goog-api-key");
        assert_eq!(agy.scheme, None);
        let personal = &table.allow[&ProfileId("personal".into())];
        assert_eq!(
            personal.iter().cloned().collect::<Vec<_>>(),
            ["agy", "assistant", "codex"]
        );
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
