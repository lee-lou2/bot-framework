//! Private DM intake gate forwarding mapped arguments directly to a declared tool with HMAC-only ledger tracking.
//!
//! Never exposes private inputs to `CallLedger`, workers, models, memory, or reply ports; `PrivateLedger`
//! records only IDs, profile-keyed HMACs, and fixed outcomes (`begin -> start -> finish`), discarding all
//! tool results, errors, and receipts. Only an allowed valid empty `bot/pass` lets the gateway fall back
//! to normal conversation intake.

use crate::{
    classify::classify,
    keyed::hmac,
    ports::{Ran, Sealer, ToolJob, ToolRunner},
    rate::{Limiter, RateConfig},
    resolve::{Resolved, find_tool, pinned_to_image, selected_env},
    sys::Clock,
    text::hex,
};
use bot_core::{
    id::*,
    intake::{PrivateCall, PrivateDecision, PrivateIntake},
    ledger::{PrivateLedger, PrivateOutcome, PrivatePermit, PrivateRequest, PrivateStart},
    port::{Error, Reply, SecretStore},
    profile::{Adapter, Destination, ProfileCatalog, TriggerSource, TriggerTarget},
    scope::effective,
    state::{Conversation, ConversationKind, RunOrigin},
    trace::{SpanIds, SpanRecord, Tracer, key},
};
use std::sync::Arc;

/// Profile-scoped HMAC key name for private intake deduplication; intake fails closed if absent.
pub const FINGERPRINT_SECRET: &str = "private-intake";

/// Host-only secret names that are never resolved into any tool's environment.
pub fn reserved_secret(name: &SecretName) -> bool {
    name.as_str() == FINGERPRINT_SECRET
}

pub struct PrivateParts {
    pub catalog: Arc<dyn ProfileCatalog>,
    pub ledger: Arc<dyn PrivateLedger>,
    pub secrets: Arc<dyn SecretStore>,
    pub sealer: Arc<dyn Sealer>,
    pub runner: Arc<dyn ToolRunner>,
    pub tracer: Arc<dyn Tracer>,
    pub clock: Arc<dyn Clock>,
}

pub struct PrivateGate {
    parts: PrivateParts,
    limiter: Arc<Limiter>,
}

struct Exec {
    attempt: Attempt,
    principal: PrincipalId,
    origin: RunOrigin,
    tool: ToolId,
    resolved: Resolved,
    canonical: String,
    env: Vec<(String, String)>,
    fingerprint: [u8; 32],
    checked_revision: Revision,
    reply_to: Option<Conversation>,
    private_pass: bool,
}

fn error_class(error: &Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Unavailable | Error::RateLimited => "unavailable",
        Error::Invalid | Error::Missing => "invalid",
    }
}

impl PrivateIntake for PrivateGate {
    fn receive(&self, call: PrivateCall) -> Reply<'_, PrivateDecision> {
        Box::pin(async move {
            let (profile, request) = (call.key.profile.clone(), call.key.request.0.clone());
            let mut label = "rejected";
            let result = self.receive_inner(call, &mut label).await;
            self.span(
                &profile,
                &request,
                "broker.private.intake",
                label,
                result.as_ref().err(),
            );
            result
        })
    }
}

impl PrivateGate {
    pub fn new(parts: PrivateParts, rate: RateConfig) -> Self {
        let limiter = Limiter::new(rate, Arc::clone(&parts.clock));
        Self { parts, limiter }
    }

    async fn receive_inner(
        &self,
        call: PrivateCall,
        label: &mut &'static str,
    ) -> Result<PrivateDecision, Error> {
        let PrivateCall {
            key,
            revision,
            conversation,
            agent,
            trigger,
            tool,
            args,
            envelope_sha256,
        } = call;
        let parts = &self.parts;
        let snapshot = parts.catalog.current(&key.profile).ok_or(Error::Denied)?;
        if revision != snapshot.revision {
            return Err(Error::Stale);
        }
        // Sensitive intake requires a direct conversation on a Slack DM binding for this agent (or an ancestor).
        let dm = snapshot.bindings.get(&key.binding).is_some_and(|b| {
            (b.agent == agent || agent.0.starts_with(&format!("{}/", b.agent.0)))
                && matches!(b.source, Adapter::SlackDm { .. })
        });
        if conversation.kind != ConversationKind::Direct || !dm {
            return Err(Error::Denied);
        }
        let owner = snapshot.agents.get(&agent).ok_or(Error::Denied)?;
        let declaration = owner.triggers.iter().find(|t| {
            t.id == trigger && t.private
                && matches!(&t.on, TriggerSource::Event { binding, .. } if *binding == key.binding)
                && matches!(&t.target, TriggerTarget::Tool { tool: target, .. } if *target == tool)
        }).ok_or(Error::Denied)?;
        let declared_tool = find_tool(&snapshot, &agent, &tool).ok_or(Error::Denied)?;
        // Approval cards display arguments and effect journals store argument hashes, so neither is permitted for private intake.
        if declared_tool.effects
            || classify(declared_tool, &snapshot.policy.approve, false).needs_approval()
        {
            return Err(Error::Denied);
        }
        if declared_tool
            .secrets
            .values()
            .any(|name| !snapshot.secrets.contains(name))
        {
            return Err(Error::Denied);
        }
        let scopes = effective(&snapshot, &key.principal, &agent).ok_or(Error::Denied)?;
        if declared_tool.scopes.is_empty() || !declared_tool.scopes.is_subset(&scopes) {
            return Err(Error::Denied);
        }
        let image = parts
            .runner
            .installed(&declared_tool.server)
            .ok_or(Error::Denied)?;
        parts.runner.admit(&declared_tool.server)?;
        let runnable = pinned_to_image(declared_tool.clone(), &image);
        let env = selected_env(declared_tool, &agent, &snapshot, &snapshot).ok_or(Error::Denied)?;
        let sealed = parts.sealer.seal(declared_tool, &args)?;
        drop(args);
        let secret = SecretName::parse(FINGERPRINT_SECRET).ok_or(Error::Unavailable)?;
        let hmac_key = parts
            .secrets
            .for_profile(key.profile.clone())
            .await?
            .resolve(secret)
            .await
            .map_err(|_| Error::Unavailable)?
            .0;
        let fingerprint = hmac(
            &hmac_key,
            &[
                key.profile.0.as_bytes(),
                key.binding.0.as_bytes(),
                key.principal.0.as_bytes(),
                key.request.0.as_bytes(),
                agent.0.as_bytes(),
                tool.0.as_bytes(),
                // Bind reply destination (conversation ID and thread) so different threads are distinct requests.
                conversation.id.as_bytes(),
                conversation.thread.as_deref().unwrap_or("").as_bytes(),
                &[u8::from(conversation.thread.is_some())],
                sealed.canonical.as_bytes(),
            ],
        );
        let pass_fingerprint = declaration.private_pass.then(|| {
            hmac(
                &hmac_key,
                &[
                    b"private-pass/v2",
                    &fingerprint,
                    revision.0.as_bytes(),
                    trigger.as_bytes(),
                    &envelope_sha256,
                ],
            )
        });
        let permit = parts
            .ledger
            .begin(PrivateRequest {
                key: key.clone(),
                fingerprint,
                pass_fingerprint,
                revision: revision.clone(),
                agent,
                tool: tool.clone(),
                conversation: conversation.clone(),
                scopes,
            })
            .await?;
        match permit {
            PrivatePermit::Start {
                attempt,
                allow_pass,
            } => {
                let fingerprint = if allow_pass {
                    pass_fingerprint.ok_or(Error::Denied)?
                } else {
                    fingerprint
                };
                *label = "accepted";
                let origin = RunOrigin::from_ingress(&conversation.kind, &key.principal);
                let reply_to = matches!(runnable.destination, Some(Destination::Reply))
                    .then_some(conversation);
                let run = Exec {
                    attempt,
                    origin,
                    principal: key.principal,
                    resolved: Resolved::new(tool.clone(), runnable).ok_or(Error::Invalid)?,
                    tool,
                    canonical: sealed.canonical,
                    env,
                    fingerprint,
                    checked_revision: snapshot.revision.clone(),
                    reply_to,
                    private_pass: allow_pass,
                };
                let result = self.execute(run).await;
                if result.is_err() {
                    *label = "failed";
                }
                result
            }
            // Still in progress or indeterminate: do not ACK so delivery retries.
            PrivatePermit::InProgress => {
                *label = "in_progress";
                Err(Error::Unavailable)
            }
            PrivatePermit::Completed | PrivatePermit::Held => {
                *label = "duplicate";
                Ok(PrivateDecision::Consumed)
            }
            PrivatePermit::Passed => {
                *label = "duplicate";
                pass_fingerprint
                    .map(|_| PrivateDecision::Pass)
                    .ok_or(Error::Denied)
            }
            PrivatePermit::Rejected => {
                *label = "duplicate";
                Err(Error::Denied)
            }
        }
    }

    /// Executes a leased private intake call; pre-start failures release the lease for retry, while post-start outcomes persist fixed status codes only.
    async fn execute(&self, run: Exec) -> Result<PrivateDecision, Error> {
        let (ledger, profile) = (&self.parts.ledger, run.attempt.profile.clone());
        let release = |attempt: Attempt| async move {
            let _ = ledger
                .finish(attempt, PrivateOutcome::RetryBeforeStart)
                .await;
        };
        let Ok(_slot) = self.limiter.admit(&profile, &run.principal) else {
            release(run.attempt).await;
            return Err(Error::Unavailable);
        };
        // Re-check active revision immediately before `start` (the store re-verifies under the same lock).
        let still = self
            .parts
            .catalog
            .current(&profile)
            .is_some_and(|s| s.revision == run.checked_revision);
        if !still {
            release(run.attempt).await;
            return Err(Error::Stale);
        }
        let started = ledger
            .start(PrivateStart {
                attempt: run.attempt.clone(),
                tool: run.tool.clone(),
                fingerprint: run.fingerprint,
                checked_revision: run.checked_revision.clone(),
            })
            .await;
        if let Err(error) = started {
            // Releases only if `start` never committed; if `start` committed and the reply was lost, release is rejected and it transitions to held.
            release(run.attempt).await;
            return Err(error);
        }
        let tag = hex(&run.fingerprint[..8]);
        let job = ToolJob {
            attempt: run.attempt.clone(),
            principal: run.principal,
            origin: run.origin,
            call: CallId(format!("p-{tag}")),
            tool: run.resolved,
            args: run.canonical,
            env: run.env,
            reply_to: run.reply_to,
            owner: format!(
                "private.{}.{}.{tag}",
                run.attempt.run.0, run.attempt.generation.0
            ),
        };
        let outcome = match self.parts.runner.run(job).await {
            // `isError` may still have caused partial effects; hold rather than mark completed.
            Ok(Ran::Done(payload)) if reports_error(&payload.0) => PrivateOutcome::Held,
            Ok(Ran::Done(payload)) if crate::service::pass::carries_marker(&payload.0) => {
                let (_, marker) = crate::service::pass::split(&payload);
                if run.private_pass && marker.is_some_and(|m| m.valid) {
                    PrivateOutcome::Passed
                } else {
                    PrivateOutcome::Held
                }
            }
            Ok(Ran::Done(_)) => PrivateOutcome::Completed,
            Ok(Ran::Rejected | Ran::Refused(_)) => PrivateOutcome::Rejected,
            Ok(Ran::NotSent) => PrivateOutcome::NotSent,
            // Blocked or unknown outcomes mean the tool may already have executed; hold without retrying.
            Ok(Ran::Blocked(_) | Ran::Unknown(_)) | Err(_) => PrivateOutcome::Held,
        };
        let result = match outcome {
            PrivateOutcome::Passed => Ok(PrivateDecision::Pass),
            PrivateOutcome::Completed | PrivateOutcome::Held => Ok(PrivateDecision::Consumed),
            PrivateOutcome::NotSent | PrivateOutcome::RetryBeforeStart => Err(Error::Unavailable),
            PrivateOutcome::Rejected => Err(Error::Denied),
        };
        self.span(
            &profile,
            &run.attempt.run.0,
            "broker.private.exec",
            outcome_label(&outcome),
            result.as_ref().err(),
        );
        ledger.finish(run.attempt, outcome).await?;
        result
    }

    /// Records classification-only telemetry tagged with `BOT_PRIVATE` and no payload or principal.
    fn span(
        &self,
        profile: &ProfileId,
        anchor: &str,
        name: &str,
        outcome: &'static str,
        error: Option<&Error>,
    ) {
        let anchor = Attempt {
            profile: profile.clone(),
            run: RunId(format!("private-{anchor}")),
            generation: Generation(0),
            claim: None,
        };
        let mut span = SpanRecord::start(profile, name, SpanIds::attempt(&anchor))
            .attr(key::BOT_PRIVATE, true)
            .attr(key::BOT_OUTCOME, outcome);
        if let Some(error) = error {
            span = span.fail(error_class(error));
        }
        span.end(self.parts.tracer.as_ref());
    }
}

/// Treats malformed or missing `isError` payloads as errors rather than success.
fn reports_error(payload: &str) -> bool {
    match crate::json::parse(payload) {
        Some(serde_json::Value::Object(map)) => {
            map.get("isError") != Some(&serde_json::Value::Bool(false))
        }
        _ => true,
    }
}

fn outcome_label(outcome: &PrivateOutcome) -> &'static str {
    match outcome {
        PrivateOutcome::Completed => "completed",
        PrivateOutcome::Passed => "passed",
        PrivateOutcome::Held => "held",
        PrivateOutcome::NotSent => "not_sent",
        PrivateOutcome::RetryBeforeStart => "retry",
        PrivateOutcome::Rejected => "rejected",
    }
}

#[cfg(test)]
mod tests;
