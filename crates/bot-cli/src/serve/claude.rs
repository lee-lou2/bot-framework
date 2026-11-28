//! Host assembly for VM-hosted CLI model runs (`claude-cli`, `codex-cli`, `agy-cli`): broker relay front, Apple VM, and park tracking.
use super::guard::GuardedUpstream;
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
    auth::{AuthResolver, AuthSource, CredentialKind, HeaderMode},
    id::{AgentId, Attempt, ProfileId},
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
    /// Accepts `HostAuthResolver` built via `for_serve(Arc<Watched>)` so every read/minted credential enters the redactor.
    pub auth: Arc<dyn AuthResolver>,
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
            } else if trimmed.ends_with("/v1") || trimmed == "backend-api/codex" {
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
            secret: _,
            auth: _,
            max_tokens: _,
            effort: _,
            fallback: _,
        }) = provider
        else {
            continue;
        };
        let (target, path) = upstream_target(*kind, endpoint)
            .ok_or_else(|| format!("cannot parse providers.{alias}.endpoint"))?;
        let auth = provider
            .auth_source(alias)?
            .ok_or_else(|| format!("providers.{alias}: missing auth configuration"))?;
        let (header_mode, forward_headers) = match (kind, &auth) {
            (CliKind::Claude, AuthSource::Secret(_)) => (
                HeaderMode::ApiKey("x-api-key"),
                vec!["anthropic-version".into(), "anthropic-beta".into()],
            ),
            (
                CliKind::Claude,
                AuthSource::Credential {
                    expected_kind: CredentialKind::SubscriptionToken,
                    ..
                },
            ) => (
                HeaderMode::BearerMergeHeader {
                    header: "anthropic-beta",
                    token: "oauth-2025-04-20",
                },
                vec!["anthropic-version".into(), "anthropic-beta".into()],
            ),
            (CliKind::Claude, AuthSource::Credential { .. }) => (
                HeaderMode::Bearer,
                vec!["anthropic-version".into(), "anthropic-beta".into()],
            ),
            (CliKind::Codex, _) => (
                HeaderMode::Bearer,
                vec![
                    "openai-beta".into(),
                    "originator".into(),
                    "user-agent".into(),
                ],
            ),
            (CliKind::Agy, AuthSource::Secret(_)) => {
                (HeaderMode::ApiKey("x-goog-api-key"), Vec::new())
            }
            (CliKind::Agy, AuthSource::Credential { .. }) => (HeaderMode::Bearer, Vec::new()),
        };
        providers.insert(
            alias.clone(),
            UpstreamProvider {
                protocol: cli_protocol(*kind),
                endpoint: target,
                path,
                auth,
                header_mode,
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
    if input
        .host
        .allow_subscription_auth
        .values()
        .any(|vendors| !vendors.is_empty())
    {
        tracing::warn!(
            code = "subscription_auth_enabled",
            "experimental opt-in CLI subscription authentication is enabled for one or more profiles (not yet verified against live vendor upstreams)"
        );
    }
    let ledger = input.store.clone() as Arc<dyn CallLedger>;
    let model = Arc::new(ModelProxy::with_auth(
        model_host(input.host)?,
        ledger.clone(),
        input.auth,
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
            max_tokens,
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
                max_output_tokens: max_tokens.unwrap_or(MAX_OUTPUT_TOKENS),
                retries: RETRIES,
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
mod tests;
