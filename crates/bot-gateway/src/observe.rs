//! Non-blocking gateway trace span recording with fixed vocabulary attributes and no payload leakage.
use crate::{
    inbound::Event,
    ingress::{Admission, Rejection},
};
use bot_core::{
    id::{ProfileId, RunId},
    trace::{Key, SpanIds, SpanKind, SpanRecord, Tracer, key, now_nano},
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Fixed HTTP span attributes for requests not matching a registered trigger (OTel convention).
pub const UNMATCHED_METHOD: &str = "_OTHER";
pub const UNMATCHED_ROUTE: &str = "unmatched";

fn digest(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

/// Derives a shared trace ID per `(profile, run)` so ingress and delivery spans correlate with worker traces.
pub fn ids(profile: &ProfileId, run: &RunId) -> SpanIds {
    let base = digest(&[b"bot-gateway-trace", profile.0.as_bytes(), run.0.as_bytes()]);
    ids_from(base)
}

pub fn ids_for_request(profile: &ProfileId, binding: &str, request: &str) -> SpanIds {
    let base = digest(&[
        b"bot-gateway-request",
        profile.0.as_bytes(),
        binding.as_bytes(),
        request.as_bytes(),
    ]);
    ids_from(base)
}

fn ids_from(base: [u8; 32]) -> SpanIds {
    let mut trace = [0u8; 16];
    trace.copy_from_slice(&base[..16]);
    let unique = digest(&[
        b"bot-gateway-span",
        &base,
        &now_nano().to_be_bytes(),
        &SEQ.fetch_add(1, Ordering::Relaxed).to_be_bytes(),
    ]);
    let mut span = [0u8; 8];
    span.copy_from_slice(&unique[..8]);
    if span == [0; 8] {
        span[7] = 1;
    }
    SpanIds {
        trace,
        span,
        parent: None,
    }
}

pub fn outcome(admission: &Admission) -> &'static str {
    match admission {
        Admission::Accepted { .. } => "accepted",
        Admission::Private => "private",
        Admission::Ignored => "ignored",
        Admission::Rejected(rejection) => match rejection {
            Rejection::Unregistered => "unregistered",
            Rejection::Forbidden => "forbidden",
            Rejection::RateLimited(_) => "rate_limited",
            Rejection::TooLarge => "too_large",
            Rejection::NoRoute => "no_route",
            Rejection::Invalid => "invalid",
            Rejection::Conflict => "conflict",
            Rejection::Unavailable => "unavailable",
        },
    }
}

pub fn failure(admission: &Admission) -> Option<&'static str> {
    match admission {
        Admission::Rejected(Rejection::Unavailable) => Some("unavailable"),
        Admission::Rejected(Rejection::Conflict) => Some("conflict"),
        _ => None,
    }
}

pub struct Intake<'a> {
    pub profile: &'a ProfileId,
    pub binding: &'a str,
    pub request: &'a str,
    pub principal: &'a str,
    pub conversation: &'static str,
    pub trigger: Option<&'a str>,
    pub private: bool,
    /// Matched trigger definition `(method, path)`; never raw caller-supplied path.
    pub route: Option<(&'a str, &'a str)>,
    pub adapter: &'static str,
    pub event: &'a Event,
    pub started: u64,
}

pub fn record_intake(tracer: &dyn Tracer, intake: &Intake<'_>, admission: &Admission) {
    let ids = match admission {
        Admission::Accepted { run, .. } => ids(intake.profile, run),
        _ => ids_for_request(intake.profile, intake.binding, intake.request),
    };
    let mut span = SpanRecord::start(intake.profile, format!("ingress {}", intake.adapter), ids)
        .kind(SpanKind::Server)
        .started_at(intake.started)
        .attr(key::BOT_PROFILE, intake.profile.0.as_str())
        .attr(key::BOT_BINDING, intake.binding)
        .attr(key::BOT_REQUEST_ID, intake.request)
        .attr(key::BOT_PRINCIPAL, intake.principal)
        .attr(key::BOT_CONVERSATION_KIND, intake.conversation)
        .attr(key::BOT_OUTCOME, outcome(admission));
    if let Admission::Accepted { run, .. } = admission {
        span = span.attr(key::BOT_RUN_ID, run.0.as_str());
    }
    if let Some(trigger) = intake.trigger {
        span = span.attr(key::BOT_TRIGGER, trigger);
    }
    if let Event::Http { .. } = intake.event {
        // Never record caller-supplied HTTP method/path; use only registered trigger routes or fixed placeholders.
        let (method, route) = intake.route.unwrap_or((UNMATCHED_METHOD, UNMATCHED_ROUTE));
        span = span
            .attr(key::HTTP_REQUEST_METHOD, method)
            .attr(key::HTTP_ROUTE, route);
    }
    if intake.private {
        // Internal suppression flag for private DM spans; never exported and carries no payload.
        span = span.attr(key::BOT_PRIVATE, true);
    }
    if let Some(class) = failure(admission) {
        span = span.fail(class);
    }
    span.end(tracer);
}

pub struct Delivered<'a> {
    pub profile: &'a ProfileId,
    pub run: &'a RunId,
    pub binding: &'a str,
    pub conversation: &'static str,
    pub outcome: &'static str,
    pub error: Option<&'static str>,
    pub started: u64,
}

pub fn record_delivery(tracer: &dyn Tracer, delivered: &Delivered<'_>) {
    let mut span = SpanRecord::start(
        delivered.profile,
        "deliver reply",
        ids(delivered.profile, delivered.run),
    )
    .kind(SpanKind::Producer)
    .started_at(delivered.started)
    .attr(key::BOT_PROFILE, delivered.profile.0.as_str())
    .attr(key::BOT_RUN_ID, delivered.run.0.as_str())
    .attr(key::BOT_BINDING, delivered.binding)
    .attr(key::BOT_CONVERSATION_KIND, delivered.conversation)
    .attr(key::BOT_OUTCOME, delivered.outcome);
    if let Some(class) = delivered.error {
        span = span.fail(class);
    }
    span.end(tracer);
}

/// Records an approval click span without including the approval nonce.
pub fn record_approval(
    tracer: &dyn Tracer,
    profile: &ProfileId,
    principal: &str,
    outcome: &'static str,
    started: u64,
) {
    let seed = format!("{principal}:{started}");
    let ids = ids_for_request(profile, "approval", &seed);
    let key_outcome: Key = key::BOT_APPROVAL_OUTCOME;
    let mut span = SpanRecord::start(profile, "approval decide", ids)
        .kind(SpanKind::Server)
        .started_at(started)
        .attr(key::BOT_PROFILE, profile.0.as_str())
        .attr(key::BOT_PRINCIPAL, principal)
        .attr(key_outcome, outcome);
    if matches!(outcome, "unavailable") {
        span = span.fail("unavailable");
    }
    span.end(tracer);
}
