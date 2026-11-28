//! Central request admission pipeline: entrance lookup, rate limits, membership, trigger selection, and durable intake.
//!
//! Private DM triggers branch before the durable queue and never fall through to normal chat on failure.
use crate::{
    active::{ActiveCheck, Release},
    entrance::{Owner, Real},
    host::{CliAccount, HostConfig, HostError, resolve_uid},
    inbound::{Event, Inbound, Respond, kind_name},
    limits::{Limits, RateLimiter},
    observe::{self, Intake},
    routes::{Active, BoundTrigger, Route, Routes, RoutesError},
    trigger,
};
use bot_core::{
    command::{Accept, AcceptExtras, Envelope, HttpRoute, JsonPayload, Target},
    id::{PrincipalId, ProfileId, RequestId, Revision, RunId},
    intake::{PrivateCall, PrivateDecision, PrivateIntake, RequestKey},
    port::{Error, RunStore},
    profile::{ProfileCatalog, ResponseMode, Snapshot, TriggerSource, TriggerTarget},
    slack::SlackSource,
    trace::{NoopTracer, Tracer, now_nano},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug, Eq, PartialEq)]
pub enum Admission {
    Accepted {
        run: RunId,
        respond: Respond,
    },
    /// Consumed by a private DM trigger without writing to the durable queue.
    Private,
    /// Unregistered entrance; acknowledge without creating work or replying.
    Ignored,
    Rejected(Rejection),
}

#[derive(Debug, Eq, PartialEq)]
pub enum Rejection {
    Unregistered,
    Forbidden,
    RateLimited(Duration),
    TooLarge,
    NoRoute,
    Invalid,
    Conflict,
    Unavailable,
}

impl Admission {
    /// Returns true only when redelivery cannot change the outcome; transient rejections stay unacknowledged.
    pub fn settled(&self) -> bool {
        !matches!(
            self,
            Admission::Rejected(Rejection::Unavailable | Rejection::RateLimited(_))
        )
    }
}

type UidResolver = dyn Fn(&CliAccount) -> Result<u32, HostError> + Send + Sync;

pub struct Ingress {
    catalog: Arc<dyn ProfileCatalog>,
    host: Arc<HostConfig>,
    store: Arc<dyn RunStore>,
    private: Arc<dyn PrivateIntake>,
    limits: Limits,
    entrance_limiter: RateLimiter,
    principal_limiter: RateLimiter,
    uid_of: Box<UidResolver>,
    routes: Mutex<Cached>,
    tracer: Arc<dyn Tracer>,
    active: Option<Arc<dyn ActiveCheck>>,
}

#[derive(Default)]
struct Cached {
    current: Option<Arc<Routes>>,
    failed: Option<(Active, RoutesError)>,
}

#[derive(Default)]
struct Seen {
    trigger: Option<String>,
    private: bool,
    /// Matched trigger definition `(method, path)`, never the raw request path.
    route: Option<(String, String)>,
}

struct Plan {
    target: Target,
    respond: Respond,
}

impl Ingress {
    pub fn new(
        catalog: Arc<dyn ProfileCatalog>,
        host: Arc<HostConfig>,
        store: Arc<dyn RunStore>,
        private: Arc<dyn PrivateIntake>,
    ) -> Self {
        let limits = host.limits();
        Self {
            catalog,
            host,
            store,
            private,
            entrance_limiter: RateLimiter::new(limits.per_entrance_per_min),
            principal_limiter: RateLimiter::new(limits.per_principal_per_min),
            limits,
            uid_of: Box::new(resolve_uid),
            routes: Mutex::new(Cached::default()),
            tracer: Arc::new(NoopTracer),
            active: None,
        }
    }

    pub fn with_active(mut self, active: Arc<dyn ActiveCheck>) -> Self {
        self.active = Some(active);
        self
    }

    pub fn with_tracer(mut self, tracer: Arc<dyn Tracer>) -> Self {
        self.tracer = tracer;
        self
    }

    pub fn tracer(&self) -> &Arc<dyn Tracer> {
        &self.tracer
    }

    pub fn with_uid_resolver(
        mut self,
        resolver: impl Fn(&CliAccount) -> Result<u32, HostError> + Send + Sync + 'static,
    ) -> Self {
        self.uid_of = Box::new(resolver);
        self
    }

    pub fn pinned(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        self.catalog.lookup(profile, revision)
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn host(&self) -> &Arc<HostConfig> {
        &self.host
    }

    /// Rebuilds the cached route table when the active set changes; keeps the previous valid table on build failure.
    pub fn routes(&self) -> Result<Arc<Routes>, RoutesError> {
        let mut cache = self.routes.lock().unwrap_or_else(|e| e.into_inner());
        let active = self.catalog.active();
        if let Some(current) = &cache.current
            && current.is_current(&active)
        {
            return Ok(current.clone());
        }
        if let Some((failed, error)) = &cache.failed
            && Arc::ptr_eq(failed, &active)
        {
            return cache.current.clone().ok_or_else(|| error.clone());
        }
        match Routes::build(active.clone(), &self.host, &*self.uid_of) {
            Ok(built) => {
                let built = Arc::new(built);
                cache.current = Some(built.clone());
                cache.failed = None;
                Ok(built)
            }
            Err(error) => {
                tracing::error!(%error, "failed to build route table from active profile set — keeping previous table");
                cache.failed = Some((active, error.clone()));
                cache.current.clone().ok_or(error)
            }
        }
    }

    pub async fn admit(&self, inbound: Inbound) -> Admission {
        let Ok(routes) = self.routes() else {
            return Admission::Rejected(Rejection::Unavailable);
        };
        self.admit_with(&routes, inbound).await
    }

    pub async fn verify_active(&self) -> Result<(), Error> {
        match &self.active {
            Some(check) => {
                let verified = check.verify().await;
                if let Err(error) = &verified {
                    tracing::warn!(
                        ?error,
                        "active set check failed — closing intake until restarted with the active revision"
                    );
                }
                verified
            }
            None => Ok(()),
        }
    }

    /// Re-verifies current binding membership and active revision immediately before releasing a result or status.
    pub async fn release(&self, real: &Real, owner: &Owner, principal: &PrincipalId) -> Release {
        if !self.still_member(real, owner, principal) {
            return Release::Forbidden;
        }
        match self.verify_active().await {
            Ok(()) => Release::Allowed,
            Err(_) => Release::Unavailable,
        }
    }

    pub fn still_member(&self, real: &Real, owner: &Owner, principal: &PrincipalId) -> bool {
        let Ok(routes) = self.routes() else {
            return false;
        };
        routes.find(real).is_some_and(|route| {
            route.snapshot.id == owner.profile
                && *route.binding_id == owner.binding
                && route.snapshot.members.contains_key(principal)
        })
    }

    /// Admits using the caller's pre-selected route table so auth and intake observe the same profile revision.
    pub async fn admit_with(&self, routes: &Routes, inbound: Inbound) -> Admission {
        self.admit_sourced(routes, inbound, None).await
    }

    /// Like `admit_with`, attaching authenticated Slack event evidence (also enables `bot/pass` chains).
    pub async fn admit_sourced(
        &self,
        routes: &Routes,
        inbound: Inbound,
        source: Option<SlackSource>,
    ) -> Admission {
        let started = now_nano();
        let Some(route) = routes.find(&inbound.real) else {
            return Admission::Ignored;
        };
        let profile = route.snapshot.id.clone();
        let binding = route.binding_id.0.clone();
        let request = inbound.request.0.clone();
        let principal = inbound.principal.0.clone();
        let conversation = kind_name(&inbound.conversation.kind);
        let event = inbound.event.clone();
        let adapter = match inbound.real {
            Real::Cli { .. } => "cli",
            Real::SlackChannel { .. } | Real::SlackDm { .. } => "slack",
            Real::TelegramDm { .. } => "telegram",
            Real::Http { .. } | Real::HttpModel { .. } => "http",
        };
        let mut seen = Seen::default();
        let admission = self.admit_inner(routes, inbound, &mut seen, source).await;
        observe::record_intake(
            &*self.tracer,
            &Intake {
                profile: &profile,
                binding: &binding,
                request: &request,
                principal: &principal,
                conversation,
                trigger: seen.trigger.as_deref(),
                private: seen.private,
                route: seen.route.as_ref().map(|(m, p)| (m.as_str(), p.as_str())),
                adapter,
                event: &event,
                started,
            },
            &admission,
        );
        admission
    }

    async fn admit_inner(
        &self,
        routes: &Routes,
        inbound: Inbound,
        seen: &mut Seen,
        source: Option<SlackSource>,
    ) -> Admission {
        let Some(route) = routes.find(&inbound.real) else {
            return Admission::Ignored;
        };
        // Reject stale processes before rate or permission checks so callers retry against the active revision.
        if self.verify_active().await.is_err() {
            return Admission::Rejected(Rejection::Unavailable);
        }
        let snapshot = route.snapshot;
        let profile = &snapshot.id;
        let body_text = inbound.body.to_string();
        if body_text.len() > self.limits.body_bytes
            || inbound.attachments.len() > self.limits.attachments
        {
            return Admission::Rejected(Rejection::TooLarge);
        }
        // Charge entrance limit before membership checks using bounded registered binding keys.
        let entrance_key = format!("{}/{}", profile.0, route.binding_id.0);
        if let Err(wait) = self.entrance_limiter.check(&entrance_key) {
            return Admission::Rejected(Rejection::RateLimited(wait));
        }
        let Some(roles) = snapshot.members.get(&inbound.principal) else {
            tracing::debug!(profile = %profile.0, binding = %route.binding_id.0, "rejected unregistered principal");
            return Admission::Rejected(Rejection::Unregistered);
        };
        let principal_key = format!("{}/{}", profile.0, inbound.principal.0);
        if let Err(wait) = self.principal_limiter.check(&principal_key) {
            return Admission::Rejected(Rejection::RateLimited(wait));
        }
        let scopes = requester_scopes(snapshot, roles);
        let envelope = trigger::envelope_json(route.binding_id, &inbound);
        let chosen = trigger::select(route.triggers, route.binding_id, &inbound.event, &envelope);
        if let Some(bound) = chosen {
            seen.trigger = Some(bound.trigger.id.clone());
            seen.private = bound.trigger.private;
            if let TriggerSource::Http { method, path, .. } = &bound.trigger.on {
                seen.route = Some((method.clone(), path.clone()));
            }
        }
        let mut key = RequestKey {
            profile: profile.clone(),
            binding: route.binding_id.clone(),
            principal: inbound.principal.clone(),
            request: inbound.request.clone(),
        };
        let plan = match chosen {
            Some(bound) if bound.trigger.private => {
                match self
                    .private(
                        &route,
                        bound,
                        key.clone(),
                        &inbound,
                        &envelope,
                        source.as_ref(),
                    )
                    .await
                {
                    Ok(PrivateDecision::Consumed) => return Admission::Private,
                    Err(rejection) => return Admission::Rejected(rejection),
                    Ok(PrivateDecision::Pass) if !bound.trigger.private_pass => {
                        return Admission::Rejected(Rejection::Forbidden);
                    }
                    Ok(PrivateDecision::Pass) => {
                        // The private DM intake already committed `key.request` as a private row in `bot_requests`;
                        // suffix the pass-through request ID while `slack_key` still enforces Slack deduplication.
                        key.request = RequestId(format!("{}:pass", key.request.0));
                        let text = inbound
                            .body
                            .get("text")
                            .and_then(Value::as_str)
                            .filter(|text| !text.trim().is_empty());
                        let Some(text) = text else {
                            return Admission::Rejected(Rejection::Invalid);
                        };
                        Plan {
                            target: Target::Agent {
                                agent: route.binding.agent.clone(),
                                instruction: text.to_owned(),
                            },
                            respond: Respond::Accepted,
                        }
                    }
                }
            }
            Some(bound) => match plan_of(bound, &envelope) {
                Some(plan) => plan,
                None => return Admission::Rejected(Rejection::Invalid),
            },
            None => match &inbound.event {
                Event::Http { .. } | Event::Reaction => {
                    return Admission::Rejected(Rejection::NoRoute);
                }
                Event::Message | Event::ThreadReply => {
                    let text = inbound
                        .body
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty());
                    let Some(text) = text else {
                        return Admission::Rejected(Rejection::Invalid);
                    };
                    Plan {
                        target: Target::Agent {
                            agent: route.binding.agent.clone(),
                            instruction: text.to_owned(),
                        },
                        respond: Respond::Accepted,
                    }
                }
            },
        };
        let chain = source
            .as_ref()
            .and_then(|_| chain::pass_chain(&route, &inbound, &envelope, &plan, &scopes));
        let extras = AcceptExtras { source, chain };
        self.accept(&route, key, inbound, body_text, plan, scopes, extras)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn accept(
        &self,
        route: &Route<'_>,
        key: RequestKey,
        inbound: Inbound,
        body_text: String,
        plan: Plan,
        scopes: BTreeSet<String>,
        extras: AcceptExtras,
    ) -> Admission {
        // Agent runs require `run`; direct tool trigger scopes are enforced per tool by the broker.
        if matches!(plan.target, Target::Agent { .. }) && !scopes.contains("run") {
            return Admission::Rejected(Rejection::Forbidden);
        }
        let http = match &inbound.event {
            Event::Http { method, path } => Some(HttpRoute {
                method: method.clone(),
                path: path.clone(),
                response: plan.respond.into(),
            }),
            Event::Message | Event::ThreadReply | Event::Reaction => None,
        };
        let command = Accept {
            reply_transport: crate::routes::reply_transport(&route.binding.source),
            approver: approver_of(&inbound.principal),
            revision: route.snapshot.revision.clone(),
            target: plan.target,
            scopes,
            event: Envelope {
                profile: key.profile,
                request: key.request,
                binding: key.binding,
                principal: key.principal,
                conversation: inbound.conversation,
                http,
                attachments: inbound.attachments,
                body: JsonPayload(body_text),
            },
        };
        match self.store.accept_with(command, extras).await {
            Ok(run) => Admission::Accepted {
                run,
                respond: plan.respond,
            },
            Err(error) => Admission::Rejected(rejection_of(error)),
        }
    }

    async fn private(
        &self,
        route: &Route<'_>,
        bound: &BoundTrigger,
        key: RequestKey,
        inbound: &Inbound,
        envelope: &Value,
        source: Option<&SlackSource>,
    ) -> Result<PrivateDecision, Rejection> {
        let TriggerTarget::Tool { tool, .. } = &bound.trigger.target else {
            return Err(Rejection::Invalid);
        };
        let Some(args) = trigger::tool_args(&bound.trigger, envelope) else {
            return Err(Rejection::Invalid);
        };
        let source_id = source.map(|s| (s.event_key(), &s.event_id));
        let bytes = serde_json::to_vec(&(envelope, source_id)).map_err(|_| Rejection::Invalid)?;
        let call = PrivateCall {
            key,
            revision: route.snapshot.revision.clone(),
            conversation: inbound.conversation.clone(),
            agent: bound.agent.clone(),
            trigger: bound.trigger.id.clone(),
            tool: tool.clone(),
            args: JsonPayload(args),
            envelope_sha256: Sha256::digest(bytes).into(),
        };
        self.private.receive(call).await.map_err(private_rejection)
    }
}

fn plan_of(bound: &BoundTrigger, envelope: &Value) -> Option<Plan> {
    let target = match &bound.trigger.target {
        TriggerTarget::Instruction(instruction) => Target::Agent {
            agent: bound.agent.clone(),
            instruction: instruction.clone(),
        },
        TriggerTarget::Tool { tool, .. } => Target::Tool {
            agent: bound.agent.clone(),
            tool: tool.clone(),
            args: JsonPayload(trigger::tool_args(&bound.trigger, envelope)?),
        },
    };
    let respond = match &bound.trigger.on {
        TriggerSource::Http {
            response: ResponseMode::Sync,
            ..
        } => Respond::Sync,
        _ => Respond::Accepted,
    };
    Some(Plan { target, respond })
}

fn requester_scopes(snapshot: &Snapshot, roles: &BTreeSet<String>) -> BTreeSet<String> {
    roles
        .iter()
        .filter_map(|role| snapshot.roles.get(role))
        .flatten()
        .filter(|scope| snapshot.policy.scopes.contains(*scope))
        .cloned()
        .collect()
}

/// Service principals (HTTP tokens) cannot approve actions.
fn approver_of(principal: &PrincipalId) -> Option<PrincipalId> {
    principal.is_human().then(|| principal.clone())
}

/// Only permanent private-intake failures settle; all others remain unacknowledged because payloads are not stored.
fn private_rejection(error: Error) -> Rejection {
    match error {
        Error::Invalid => Rejection::Invalid,
        Error::Denied => Rejection::Forbidden,
        Error::Conflict
        | Error::Stale
        | Error::Missing
        | Error::Unavailable
        | Error::RateLimited => Rejection::Unavailable,
    }
}

fn rejection_of(error: Error) -> Rejection {
    match error {
        Error::Invalid => Rejection::Invalid,
        Error::Denied => Rejection::Forbidden,
        Error::Conflict => Rejection::Conflict,
        Error::Stale | Error::Missing | Error::Unavailable | Error::RateLimited => {
            Rejection::Unavailable
        }
    }
}

mod api;
mod chain;
pub use api::{ApiInbound, ApiRelease, ApiVerdict, CHAT_PATH};

#[cfg(test)]
mod tests;
