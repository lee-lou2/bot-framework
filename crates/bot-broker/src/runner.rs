//! Sandboxed and remote MCP tool runner implementing `ToolRunner`.
//!
//! Each call spawns an isolated MCP server with only its declared profile secrets and an egress
//! proxy limited to declared hosts intersected with operator caps. Pre-send failures return
//! `NotSent`, uncertain post-send failures return `Unknown`, and vetted outputs violating shape,
//! size, or credential checks return `Blocked`.

use crate::{
    canon::canonicalize,
    effects::EffectChannel,
    egress::EgressPolicy,
    guard::blocks_output,
    json::parse,
    mcp::{self, McpError, Session, remote::Remote, wire::RemoteWire},
    ports::{Ran, ToolJob, ToolRunner},
    proxy::{Connector, LOCAL_HOST, LocalService, Proxy, Resolver},
    resolve::HostPort,
    sandbox::{Limits, Process, ProxyEndpoint, Sandbox, SandboxSpec},
    sys::Entropy,
    text::{hex, json_quote},
};
use bot_core::{
    auth::{AuthResolver, StaticAuthResolver},
    command::JsonPayload,
    id::{ProfileId, SecretName},
    port::{EffectJournal, Error, Reply, SecretStore},
    profile::Tool,
    state::ConversationKind,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, time::timeout};

/// Operator-registered server revisions, egress caps, private route overrides, and remote endpoints.
#[derive(Clone, Debug, Default)]
pub struct HostRegistry {
    pub servers: BTreeMap<String, String>,
    pub egress_caps: BTreeMap<ProfileId, Vec<HostPort>>,
    pub private_egress: BTreeMap<ProfileId, BTreeMap<(String, u16), std::net::SocketAddr>>,
    /// Remote Streamable HTTP servers (also listed in `servers` under `Remote::revision`); called directly without a sandbox.
    pub remote: BTreeMap<String, Remote>,
}

pub struct Deps {
    pub sandbox: Arc<dyn Sandbox>,
    pub secrets: Arc<dyn SecretStore>,
    pub effects: Arc<dyn EffectJournal>,
    pub resolver: Arc<dyn Resolver>,
    pub connector: Arc<dyn Connector>,
    pub entropy: Arc<dyn Entropy>,
}

pub struct McpRunner {
    deps: Deps,
    auth: Arc<dyn AuthResolver>,
    host: std::sync::RwLock<HostRegistry>,
    cfg: RunnerConfig,
    wire: Option<Arc<dyn RemoteWire>>,
}

mod config;
mod remote;

use config::Budget;
pub use config::{RunnerConfig, TIMEOUT_OVER_CAP};

fn kind_name(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

fn conversation_json(conv: &bot_core::state::Conversation) -> String {
    let thread = conv
        .thread
        .as_deref()
        .map_or_else(|| "null".to_owned(), json_quote);
    format!(
        "{{\"id\":{},\"kind\":{},\"thread\":{thread}}}",
        json_quote(&conv.id),
        json_quote(kind_name(&conv.kind)),
    )
}

/// Host-supplied `params._meta` injected into `tools/call`; models cannot override these keys.
fn meta(job: &ToolJob) -> String {
    let mut out = format!(
        "{{\"bot/call-id\":{},\"bot/conversation\":{}",
        json_quote(&job.call.0),
        conversation_json(&job.conversation),
    );
    if let Some(to) = &job.reply_to {
        out.push_str(&format!(",\"bot/destination\":{}", conversation_json(to)));
    }
    // External idempotency keys use [profile-id, run-id, call-id] (excluding generation/claim so handovers preserve logical identity).
    out.push_str(&format!(
        ",\"bot/generation\":{},\"bot/principal\":{},\"bot/profile-id\":{},\"bot/run-id\":{},\"bot/trigger\":{}}}",
        job.attempt.generation.0,
        json_quote(&job.principal.0),
        json_quote(&job.attempt.profile.0),
        json_quote(&job.attempt.run.0),
        json_quote(job.origin.as_str())
    ));
    out
}

const MAX_ENV_VALUE: usize = 8 << 10;

struct EffectLocal(EffectChannel);

impl LocalService for EffectLocal {
    fn handle(&self, frame: Vec<u8>) -> Reply<'_, Vec<u8>> {
        Box::pin(async move { Ok(self.0.handle(&frame).await.into_bytes()) })
    }
}

const EFFECT_URL: &str = "http://bot-effects.invalid/effect";

/// Verifies the server's advertised tool definition against a pinned `tools.lock.yaml` hash
/// (guarding against description poisoning) and canonical `inputSchema`.
fn check_definition(tool: &Tool, entry: Option<&Value>, declared: &str) -> Result<(), Ran> {
    if let Some(pin) = &tool.definition {
        match entry {
            None => return Err(Ran::Refused("tool_missing")),
            Some(found) if bot_core::mcp::entry_sha256(found).as_deref() != Some(pin.as_str()) => {
                return Err(Ran::Refused("tool_definition_changed"));
            }
            Some(_) => {}
        }
    }
    let schema = entry
        .and_then(|found| found.get("inputSchema"))
        .and_then(canonicalize);
    if schema.as_deref() == Some(declared) {
        Ok(())
    } else {
        Err(Ran::NotSent)
    }
}

/// Scans decoded keys, string values, and numeric tokens so JSON escapes cannot conceal credentials.
fn structured_blocked(value: &Value, credentials: &[Vec<u8>]) -> bool {
    match value {
        Value::String(text) => blocks_output(text, credentials),
        Value::Number(number) => blocks_output(&number.to_string(), credentials),
        Value::Array(items) => items
            .iter()
            .any(|item| structured_blocked(item, credentials)),
        Value::Object(map) => map.iter().any(|(name, item)| {
            blocks_output(name, credentials) || structured_blocked(item, credentials)
        }),
        _ => false,
    }
}

impl McpRunner {
    pub fn new(deps: Deps, host: HostRegistry, cfg: RunnerConfig) -> Self {
        let auth = Arc::new(StaticAuthResolver::new(deps.secrets.clone()));
        Self {
            deps,
            auth,
            host: std::sync::RwLock::new(host),
            cfg,
            wire: None,
        }
    }

    pub fn with_auth(mut self, auth: Arc<dyn AuthResolver>) -> Self {
        self.auth = auth;
        self
    }

    pub fn with_wire(mut self, wire: Arc<dyn RemoteWire>) -> Self {
        self.wire = Some(wire);
        self
    }

    pub fn update_host(&self, next: HostRegistry) {
        *self.host.write().unwrap_or_else(|e| e.into_inner()) = next;
    }

    pub(super) fn host(&self) -> std::sync::RwLockReadGuard<'_, HostRegistry> {
        self.host.read().unwrap_or_else(|e| e.into_inner())
    }

    async fn execute(&self, job: ToolJob) -> Ran {
        let started = tokio::time::Instant::now();
        let tool = &job.tool.tool;
        let budget = match self.cfg.budget(tool) {
            Ok(budget) => budget,
            Err(why) => return Ran::Refused(why),
        };
        let (image, remote, egress_cap, private_routes) = {
            let host = self.host();
            let Some(image) = host.servers.get(&tool.server).cloned() else {
                return Ran::NotSent;
            };
            let remote = host.remote.get(&tool.server).cloned();
            let egress_cap = host.egress_caps.get(&job.attempt.profile).cloned();
            let private_routes = host.private_egress.get(&job.attempt.profile).cloned();
            (image, remote, egress_cap, private_routes)
        };
        if tool
            .revision
            .0
            .rsplit_once('@')
            .map(|(_, approved)| approved)
            != Some(image.as_str())
        {
            return Ran::NotSent;
        }
        if let Some(remote) = remote {
            return self.execute_remote(&job, &remote, budget).await;
        }
        if self.deps.sandbox.admit(&tool.server, &image).is_err() {
            return Ran::NotSent;
        }
        let (mut env, mut credentials) = (Vec::new(), Vec::new());
        if !tool.secrets.is_empty() {
            let Ok(store) = self
                .deps
                .secrets
                .for_profile(job.attempt.profile.clone())
                .await
            else {
                return Ran::NotSent;
            };
            for (var, name) in &tool.secrets {
                if bot_core::profile::forbidden_env_name(var)
                    || crate::private::reserved_secret(name)
                {
                    return Ran::NotSent;
                }
                let Ok(value) = store.resolve(name.clone()).await else {
                    return Ran::NotSent;
                };
                let Ok(text) = String::from_utf8(value.0.clone()) else {
                    return Ran::NotSent;
                };
                if text.contains('\0') {
                    return Ran::NotSent;
                }
                credentials.push(value.0);
                env.push((var.clone(), text));
            }
        }
        // Require an exact 1:1 match with `tool.env` and block reserved or secret-shadowing names.
        let given: BTreeSet<&String> = job.env.iter().map(|(name, _)| name).collect();
        if given.len() != job.env.len()
            || given.len() != tool.env.len()
            || !tool.env.iter().all(|name| given.contains(name))
        {
            return Ran::NotSent;
        }
        for (name, value) in &job.env {
            let clash = env.iter().any(|(taken, _)| taken == name);
            if bot_core::profile::forbidden_env_name(name)
                || clash
                || value.contains('\0')
                || value.len() > MAX_ENV_VALUE
            {
                return Ran::NotSent;
            }
            env.push((name.clone(), value.clone()));
        }
        let via_proxy = tool.effects && self.deps.sandbox.effects_via_proxy();
        let policy = EgressPolicy::new(&job.tool.egress, egress_cap.as_deref());
        let (mut proxy, mut endpoint, mut host_env) = (None, None, Vec::new());
        let mut hosts: Vec<String> = policy
            .allowed()
            .iter()
            .map(|h| format!("{}:{}", h.host, h.port))
            .collect();
        let mut effect_channel = None;
        if !policy.is_empty() || via_proxy {
            let mut raw = [0u8; 16];
            if !self.deps.entropy.fill(&mut raw) {
                return Ran::NotSent;
            }
            let password = hex(&raw);
            let mut proxy_cfg = self.cfg.proxy(policy, password.clone(), budget);
            if let Some(routes) = private_routes {
                proxy_cfg.private_routes = routes;
            }
            // Include the per-call proxy password in output and effect-report credential screening.
            credentials.push(password.clone().into_bytes());
            if via_proxy {
                let chan = EffectChannel::new(
                    Arc::clone(&self.deps.effects),
                    job.attempt.clone(),
                    job.call.clone(),
                    job.tool.id.clone(),
                )
                .with_credentials(credentials.clone());
                proxy_cfg.local = Some(Arc::new(EffectLocal(chan)));
                hosts.push(format!("{LOCAL_HOST}:80"));
                host_env.push(("BOT_EFFECT_URL".to_owned(), EFFECT_URL.to_owned()));
                effect_channel = Some(());
            }
            let started_proxy = Proxy::start(
                proxy_cfg,
                Arc::clone(&self.deps.resolver),
                Arc::clone(&self.deps.connector),
            )
            .await;
            let Ok(started_proxy) = started_proxy else {
                return Ran::NotSent;
            };
            let url = format!("http://bot:{password}@{}", started_proxy.addr());
            for name in [
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            ] {
                env.push((name.to_owned(), url.clone()));
            }
            endpoint = Some(ProxyEndpoint {
                addr: started_proxy.addr(),
                url,
            });
            proxy = Some(started_proxy);
        }
        let spec = SandboxSpec {
            owner: job.owner.clone(),
            server: tool.server.clone(),
            image,
            env,
            host_env,
            egress: endpoint.take(),
            egress_hosts: hosts,
            effects: tool.effects && effect_channel.is_none(),
            limits: Limits { wall: budget.wall },
        };
        // Bound spawn, handshake, call, and response under one wall-clock budget.
        let ran = match timeout(budget.wall, self.deps.sandbox.spawn(spec)).await {
            Ok(Ok(process)) => {
                let Process {
                    stdio,
                    effects,
                    handle,
                } = process;
                let sent = AtomicBool::new(false);
                let remaining = budget.wall.saturating_sub(started.elapsed());
                let ran = match timeout(
                    remaining,
                    self.drive(&job, stdio, effects, &credentials, &sent, budget.call),
                )
                .await
                {
                    Ok(ran) => ran,
                    Err(_) if sent.load(Ordering::SeqCst) => Ran::Unknown("wall_timeout"),
                    Err(_) => Ran::NotSent,
                };
                let confirmed = matches!(
                    timeout(self.cfg.terminate_timeout, handle.terminate()).await,
                    Ok(Ok(()))
                );
                match ran {
                    // Non-execution cannot be proven if the spawned process group is not confirmed gone.
                    Ran::NotSent | Ran::Rejected if !confirmed => {
                        Ran::Unknown("terminate_unconfirmed")
                    }
                    other => other,
                }
            }
            Ok(Err(_)) => Ran::NotSent,
            // Spawn timed out: a child may still exist, so reap by owner before claiming `NotSent`.
            Err(_) => match timeout(
                self.cfg.terminate_timeout,
                self.deps.sandbox.reap(&job.owner),
            )
            .await
            {
                Ok(Ok(())) => Ran::NotSent,
                _ => Ran::Unknown("terminate_unconfirmed"),
            },
        };
        if let Some(proxy) = proxy.take() {
            proxy.stop().await;
        }
        ran
    }

    async fn drive(
        &self,
        job: &ToolJob,
        stdio: crate::sandbox::Pipe,
        effects: Option<crate::sandbox::Pipe>,
        credentials: &[Vec<u8>],
        sent: &AtomicBool,
        call_limit: Duration,
    ) -> Ran {
        let mut session = Session::new(stdio);
        let limit = self.cfg.handshake_timeout;
        if session.initialize(limit).await.is_err() {
            return Ran::NotSent;
        }
        let Some(declared) = parse(&job.tool.tool.input.0).and_then(|v| canonicalize(&v)) else {
            return Ran::NotSent;
        };
        let entry = match session.find_tool(&job.tool.name, limit).await {
            Ok(entry) => entry,
            Err(_) => return Ran::NotSent,
        };
        if let Err(ran) = check_definition(&job.tool.tool, entry.as_ref(), &declared) {
            return ran;
        }
        let channel = match (job.tool.tool.effects, effects) {
            (true, Some(pipe)) => {
                let (stop, signal) = watch::channel(false);
                let chan = EffectChannel::new(
                    Arc::clone(&self.deps.effects),
                    job.attempt.clone(),
                    job.call.clone(),
                    job.tool.id.clone(),
                )
                .with_credentials(credentials.to_vec());
                Some((
                    stop,
                    tokio::spawn(chan.serve(pipe.read, pipe.write, signal)),
                ))
            }
            (true, None) if !self.deps.sandbox.effects_via_proxy() => return Ran::NotSent,
            _ => None,
        };
        sent.store(true, Ordering::SeqCst);
        let result = match session
            .send_call(&job.tool.name, &job.args, &meta(job))
            .await
        {
            Ok(id) => session.await_result(id, call_limit).await,
            Err(_) => Err(McpError::Io),
        };
        if let Some((stop, task)) = channel {
            let _ = stop.send(true);
            let _ = timeout(Duration::from_secs(5), task).await;
        }
        match result {
            Ok(value) => self.vet(&value, credentials),
            // JSON-RPC invalid request / method not found / invalid params prove pre-execution rejection.
            Err(McpError::Server(-32602..=-32600)) => Ran::Rejected,
            Err(McpError::Server(_)) => Ran::Unknown("server_error"),
            Err(McpError::Timeout) => Ran::Unknown("timeout"),
            Err(McpError::Closed) => Ran::Unknown("closed"),
            Err(McpError::Io) => Ran::Unknown("io"),
            Err(_) => Ran::Unknown("protocol"),
        }
    }

    fn vet(&self, value: &Value, credentials: &[Vec<u8>]) -> Ran {
        let output = match mcp::parse_result(value) {
            Ok(output) => output,
            Err(code) => return Ran::Blocked(code),
        };
        let mut size = 0usize;
        for text in &output.texts {
            size += text.len();
            if blocks_output(text, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        if let Some(structured) = &output.structured {
            let Some(text) = canonicalize(structured) else {
                return Ran::Blocked("result_shape");
            };
            size += text.len();
            if structured_blocked(structured, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        // Wake and pass markers can carry model-visible text, so vet them identically.
        for marker in [&output.wake, &output.pass].into_iter().flatten() {
            let Some(text) = canonicalize(marker) else {
                return Ran::Blocked("result_shape");
            };
            size += text.len();
            if structured_blocked(marker, credentials) {
                return Ran::Blocked("result_credential");
            }
        }
        if size > self.cfg.max_result_bytes {
            return Ran::Blocked("result_size");
        }
        match mcp::payload(&output) {
            Some(payload) if payload.len() <= self.cfg.max_result_bytes => {
                Ran::Done(JsonPayload(payload))
            }
            Some(_) => Ran::Blocked("result_size"),
            None => Ran::Blocked("result_shape"),
        }
    }
}

impl ToolRunner for McpRunner {
    fn installed(&self, server: &str) -> Option<String> {
        self.host().servers.get(server).cloned()
    }

    fn admit(&self, server: &str) -> Result<(), Error> {
        let (image, is_remote) = {
            let host = self.host();
            let image = host.servers.get(server).cloned().ok_or(Error::Denied)?;
            (image, host.remote.contains_key(server))
        };
        if is_remote {
            return if self.wire.is_some() {
                Ok(())
            } else {
                Err(Error::Denied)
            };
        }
        self.deps.sandbox.admit(server, &image)
    }

    fn server_secrets(&self, server: &str) -> Vec<SecretName> {
        self.host()
            .remote
            .get(server)
            .and_then(|remote| remote.bearer.clone())
            .into_iter()
            .collect()
    }

    fn reap_proves_end(&self, server: &str) -> bool {
        !self.host().remote.contains_key(server)
    }

    fn admit_secrets(&self, profile: &ProfileId, names: &[SecretName]) -> Reply<'_, ()> {
        let (profile, names) = (profile.clone(), names.to_vec());
        Box::pin(async move {
            if names.is_empty() {
                return Ok(());
            }
            let store = self.deps.secrets.for_profile(profile).await?;
            for name in names {
                drop(store.resolve(name).await?);
            }
            Ok(())
        })
    }

    fn has_server_auth(&self, server: &str) -> bool {
        self.host()
            .remote
            .get(server)
            .is_some_and(|remote| remote.oauth.is_some())
    }

    fn admit_server_auth(&self, profile: &ProfileId, server: &str) -> Reply<'_, ()> {
        let (profile, server) = (profile.clone(), server.to_owned());
        Box::pin(async move { self.admit_remote_auth(profile, server).await })
    }

    fn run(&self, job: ToolJob) -> Reply<'_, Ran> {
        Box::pin(async move { Ok(self.execute(job).await) })
    }

    fn reap(&self, owner: String) -> Reply<'_, ()> {
        Box::pin(async move { self.deps.sandbox.reap(&owner).await })
    }
}

#[cfg(test)]
mod tests;
