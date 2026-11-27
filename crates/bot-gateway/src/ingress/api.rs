//! OpenAI-compatible ingress admission and pre-response authorization recheck.
use super::{Admission, Ingress, Rejection, Respond};
use crate::{
    entrance::Real,
    inbound::Event,
    observe::{self, Intake},
    routes::Routes,
};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiRun, ApiRuns, ChatBody},
    command::{Accept, Envelope, HttpRoute, Target},
    id::{BindingId, PrincipalId, ProfileId, RequestId},
    port::Error,
    profile::{Adapter, ApprovalRoute, ResponseMode, Snapshot},
    scope::{RUN_SCOPE, effective},
    state::{Conversation, ConversationKind},
    trace::now_nano,
};
use std::time::Duration;

pub const CHAT_PATH: &str = "/v1/chat/completions";

pub struct ApiInbound {
    pub endpoint: String,
    pub model: String,
    pub principal: PrincipalId,
    pub request: RequestId,
    pub body: ChatBody,
    pub digest: [u8; 32],
    /// Fresh server-assigned conversation ID per POST.
    pub conversation: String,
}

#[derive(Debug)]
pub enum ApiVerdict {
    Admitted(Box<ApiRun>),
    Conflict,
    NotMember,
    /// Missing or unauthorized model (indistinguishable to callers).
    Hidden,
    RateLimited(Duration),
    /// Active run limit reached for a new request; nothing was recorded.
    Full,
    /// Confirmed pre-commit failure (including stale process or store outage).
    Unavailable,
    /// Commit status uncertain; caller must confirm via ledger lookup.
    Unknown,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApiRelease {
    Allowed,
    NotMember,
    /// Model disappeared, was reassigned to another binding/agent, or is no longer runnable by this principal.
    Hidden,
    /// Active set could not be verified or process is stale; withhold output.
    Unconfirmed,
}

#[derive(Default)]
struct Seen {
    profile: Option<ProfileId>,
    binding: Option<String>,
}

impl Ingress {
    pub async fn admit_api(
        &self,
        api: &dyn ApiRuns,
        routes: &Routes,
        inbound: ApiInbound,
    ) -> ApiVerdict {
        let started = now_nano();
        let mut seen = Seen::default();
        let verdict = self.admit_api_inner(api, routes, &inbound, &mut seen).await;
        if let (Some(profile), Some(binding)) = (&seen.profile, &seen.binding) {
            let event = Event::Http {
                method: "POST".to_owned(),
                path: CHAT_PATH.to_owned(),
            };
            observe::record_intake(
                &*self.tracer,
                &Intake {
                    profile,
                    binding,
                    request: &inbound.request.0,
                    principal: &inbound.principal.0,
                    conversation: "http",
                    trigger: None,
                    private: false,
                    route: Some(("POST", CHAT_PATH)),
                    adapter: "http",
                    event: &event,
                    started,
                },
                &mirror(&verdict),
            );
        }
        verdict
    }

    async fn admit_api_inner(
        &self,
        api: &dyn ApiRuns,
        routes: &Routes,
        inbound: &ApiInbound,
        seen: &mut Seen,
    ) -> ApiVerdict {
        let Some(profile) = routes.openai_profile(&inbound.endpoint) else {
            return ApiVerdict::Hidden;
        };
        let Some(snapshot) = routes.snapshot(profile) else {
            return ApiVerdict::Hidden;
        };
        seen.profile = Some(profile.clone());
        if !snapshot.members.contains_key(&inbound.principal) {
            return ApiVerdict::NotMember;
        }
        let real = Real::HttpModel {
            endpoint: inbound.endpoint.clone(),
            model: inbound.model.clone(),
        };
        let Some(route) = routes.find(&real) else {
            return ApiVerdict::Hidden;
        };
        seen.binding = Some(route.binding_id.0.clone());
        let scopes = match effective(snapshot, &inbound.principal, &route.binding.agent) {
            Some(scopes) if scopes.contains(RUN_SCOPE) => scopes,
            _ => return ApiVerdict::Hidden,
        };
        // Reject stale processes before consuming rate-limit budget so SDKs retry cleanly.
        if self.verify_active().await.is_err() {
            return ApiVerdict::Unavailable;
        }
        let entrance_key = format!("{}/{}", profile.0, route.binding_id.0);
        if let Err(wait) = self.entrance_limiter.check(&entrance_key) {
            return ApiVerdict::RateLimited(wait);
        }
        let principal_key = format!("{}/{}", profile.0, inbound.principal.0);
        if let Err(wait) = self.principal_limiter.check(&principal_key) {
            return ApiVerdict::RateLimited(wait);
        }
        let Some(question) = inbound
            .body
            .current()
            .and_then(|at| inbound.body.turns.get(at))
        else {
            return ApiVerdict::Unavailable;
        };
        let accept = Accept {
            reply_transport: crate::routes::reply_transport(&route.binding.source),
            // Informational snapshot; broker re-resolves approval routes from both pinned and current revisions.
            approver: snapshot
                .approval_route(route.binding_id, &inbound.principal)
                .map(|route| route.person().clone()),
            revision: snapshot.revision.clone(),
            target: Target::Agent {
                agent: route.binding.agent.clone(),
                instruction: question.content.clone(),
            },
            scopes,
            event: Envelope {
                profile: profile.clone(),
                request: inbound.request.clone(),
                binding: route.binding_id.clone(),
                principal: inbound.principal.clone(),
                conversation: Conversation {
                    kind: ConversationKind::Http,
                    id: inbound.conversation.clone(),
                    thread: Some(inbound.conversation.clone()),
                },
                http: Some(HttpRoute {
                    method: "POST".to_owned(),
                    path: CHAT_PATH.to_owned(),
                    response: ResponseMode::Accepted,
                }),
                attachments: Vec::new(),
                body: inbound.body.to_payload(),
            },
        };
        let request = ApiAccept {
            accept,
            endpoint: inbound.endpoint.clone(),
            model: inbound.model.clone(),
            digest: inbound.digest,
            max_active: self.api_max_active(&inbound.endpoint),
            team: self.delivery_team(snapshot, route.binding_id, &inbound.principal),
        };
        match api.accept(request).await {
            Ok(ApiAdmission::Accepted(run) | ApiAdmission::Replayed(run)) => {
                ApiVerdict::Admitted(Box::new(run))
            }
            Ok(ApiAdmission::Conflict) => ApiVerdict::Conflict,
            Ok(ApiAdmission::Capacity) => ApiVerdict::Full,
            Ok(ApiAdmission::Stale) => ApiVerdict::Unavailable,
            Ok(ApiAdmission::Unknown) => ApiVerdict::Unknown,
            Err(Error::Conflict) => ApiVerdict::Conflict,
            Err(Error::Denied) => ApiVerdict::NotMember,
            // Port `Err` signals confirmed pre-commit failure (`Unknown` covers commit ambiguity).
            Err(Error::Invalid) => {
                tracing::error!(endpoint = %inbound.endpoint, "store rejected OpenAI admission command as Invalid");
                ApiVerdict::Unavailable
            }
            Err(Error::Unavailable | Error::RateLimited | Error::Stale | Error::Missing) => {
                ApiVerdict::Unavailable
            }
        }
    }

    /// Pins the Slack `team_id` verified at admission so cards never post to a reassigned team after redeploy.
    fn delivery_team(
        &self,
        snapshot: &Snapshot,
        binding: &BindingId,
        principal: &PrincipalId,
    ) -> Option<String> {
        let ApprovalRoute::Slack {
            binding: delivery, ..
        } = snapshot.approval_route(binding, principal)?
        else {
            return None;
        };
        let Adapter::SlackChannel { workspace, .. } = &snapshot.bindings.get(&delivery)?.source
        else {
            return None;
        };
        let workspace = self.host.slack.workspaces.get(workspace)?;
        Some(workspace.team_id.clone())
    }

    fn api_max_active(&self, endpoint: &str) -> u32 {
        self.host
            .http
            .openai
            .get(endpoint)
            .map_or(crate::host::DEFAULT_MAX_ACTIVE_RUNS, |e| e.max_active())
    }

    pub async fn recheck_api(
        &self,
        endpoint: &str,
        principal: &PrincipalId,
        run: &ApiRun,
    ) -> ApiRelease {
        // Run the async active-set check before reading the local route table so evaluation has no await gap.
        if self.verify_active().await.is_err() {
            return ApiRelease::Unconfirmed;
        }
        let Ok(routes) = self.routes() else {
            return ApiRelease::Unconfirmed;
        };
        let Some(profile) = routes.openai_profile(endpoint) else {
            return ApiRelease::Hidden;
        };
        let Some(snapshot) = routes.snapshot(profile) else {
            return ApiRelease::Hidden;
        };
        if !snapshot.members.contains_key(principal) {
            return ApiRelease::NotMember;
        }
        let real = Real::HttpModel {
            endpoint: endpoint.to_owned(),
            model: run.model.clone(),
        };
        let Some(route) = routes.find(&real) else {
            return ApiRelease::Hidden;
        };
        if *route.binding_id != run.binding || route.binding.agent != run.agent {
            return ApiRelease::Hidden;
        }
        let runnable = effective(snapshot, principal, &route.binding.agent)
            .is_some_and(|scopes| scopes.contains(RUN_SCOPE));
        if runnable {
            ApiRelease::Allowed
        } else {
            ApiRelease::Hidden
        }
    }
}

fn mirror(verdict: &ApiVerdict) -> Admission {
    match verdict {
        ApiVerdict::Admitted(run) => Admission::Accepted {
            run: run.run.clone(),
            respond: Respond::Accepted,
        },
        ApiVerdict::Conflict => Admission::Rejected(Rejection::Conflict),
        ApiVerdict::NotMember => Admission::Rejected(Rejection::Unregistered),
        ApiVerdict::Hidden => Admission::Rejected(Rejection::NoRoute),
        ApiVerdict::RateLimited(wait) => Admission::Rejected(Rejection::RateLimited(*wait)),
        ApiVerdict::Full => Admission::Rejected(Rejection::RateLimited(Duration::from_secs(1))),
        ApiVerdict::Unavailable | ApiVerdict::Unknown => {
            Admission::Rejected(Rejection::Unavailable)
        }
    }
}
