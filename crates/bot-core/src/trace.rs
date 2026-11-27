//! Tracing port, allowed attribute vocabulary, and deterministic per-attempt trace/span IDs.
//!
//! Trace IDs derive deterministically from `(profile, run, generation)` so split-deployment processes correlate spans without IPC propagation.
//! Exporters re-enforce the attribute allowlist, opt-in content gating, and secret redaction at the boundary.

use crate::id::{Attempt, ProfileId};
use std::time::{SystemTime, UNIX_EPOCH};

pub type Key = &'static str;

/// Attribute value; never store user prose or secrets (lengths, counts, and fixed classifications only).
#[derive(Clone, Debug, PartialEq)]
pub enum Val {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Self::Str(v.to_owned())
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Self::Str(v)
    }
}
impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<i64> for Val {
    fn from(v: i64) -> Self {
        Self::Int(v)
    }
}
impl From<u32> for Val {
    fn from(v: u32) -> Self {
        Self::Int(v.into())
    }
}
impl From<u64> for Val {
    fn from(v: u64) -> Self {
        Self::Int(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

/// Attribute and event key vocabulary (single place to update for OTel GenAI convention changes).
pub mod key {
    use super::Key;
    pub const BOT_PROFILE: Key = "bot.profile";
    pub const BOT_AGENT: Key = "bot.agent";
    pub const BOT_REQUEST_ID: Key = "bot.request.id";
    pub const BOT_RUN_ID: Key = "bot.run.id";
    pub const BOT_RUN_ROOT: Key = "bot.run.root";
    pub const BOT_RUN_PARENT: Key = "bot.run.parent";
    pub const BOT_ATTEMPT_GENERATION: Key = "bot.attempt.generation";
    pub const BOT_BINDING: Key = "bot.binding";
    pub const BOT_TRIGGER: Key = "bot.trigger";
    pub const BOT_CONVERSATION_KIND: Key = "bot.conversation.kind";
    pub const BOT_PRINCIPAL: Key = "bot.principal";
    pub const BOT_OUTCOME: Key = "bot.outcome";
    pub const BOT_TOOL_RISK: Key = "bot.tool.risk";
    pub const BOT_TOOL_REVISION: Key = "bot.tool.revision";
    pub const BOT_CALL_STATE: Key = "bot.call.state";
    pub const BOT_APPROVAL_RISK: Key = "bot.approval.risk";
    pub const BOT_APPROVAL_OUTCOME: Key = "bot.approval.outcome";
    pub const BOT_EFFECT_STEP: Key = "bot.effect.step";
    pub const BOT_EFFECT_STATE: Key = "bot.effect.state";
    pub const BOT_EFFECT_SEND: Key = "bot.effect.send";
    pub const BOT_HOLD_REASON: Key = "bot.hold.reason";
    pub const BOT_DELEGATE_STEP: Key = "bot.delegate.step";
    pub const BOT_DELEGATE_CHILD: Key = "bot.delegate.child";
    pub const BOT_ADMIN_ACTION: Key = "bot.admin.action";
    pub const BOT_ADMIN_VERDICT: Key = "bot.admin.verdict";
    pub const ERROR_TYPE: Key = "error.type";
    pub const GEN_AI_OPERATION_NAME: Key = "gen_ai.operation.name";
    pub const GEN_AI_AGENT_NAME: Key = "gen_ai.agent.name";
    pub const GEN_AI_CONVERSATION_ID: Key = "gen_ai.conversation.id";
    pub const GEN_AI_PROVIDER_NAME: Key = "gen_ai.provider.name";
    pub const GEN_AI_REQUEST_MODEL: Key = "gen_ai.request.model";
    pub const GEN_AI_RESPONSE_MODEL: Key = "gen_ai.response.model";
    pub const GEN_AI_USAGE_INPUT_TOKENS: Key = "gen_ai.usage.input_tokens";
    pub const GEN_AI_USAGE_OUTPUT_TOKENS: Key = "gen_ai.usage.output_tokens";
    pub const GEN_AI_TOOL_NAME: Key = "gen_ai.tool.name";
    pub const GEN_AI_TOOL_CALL_ID: Key = "gen_ai.tool.call.id";
    pub const GEN_AI_TOOL_TYPE: Key = "gen_ai.tool.type";
    pub const HTTP_REQUEST_METHOD: Key = "http.request.method";
    pub const HTTP_ROUTE: Key = "http.route";
    pub const HTTP_RESPONSE_STATUS_CODE: Key = "http.response.status_code";
    pub const USER_ID: Key = "user.id";
    pub const SESSION_ID: Key = "session.id";
    // Exporter markers.
    pub const BOT_DROPPED: Key = "bot.dropped";
    /// Internal bool marker; suppresses content export for this span and is never exported itself.
    pub const BOT_PRIVATE: Key = "bot.private";
    pub const BOT_CONTENT_DROPPED: Key = "bot.content.dropped";
    // Content attributes, exported only when opted in by the profile and after redaction.
    pub const GEN_AI_INPUT_MESSAGES: Key = "gen_ai.input.messages";
    pub const GEN_AI_OUTPUT_MESSAGES: Key = "gen_ai.output.messages";
    pub const GEN_AI_SYSTEM_INSTRUCTIONS: Key = "gen_ai.system_instructions";
    pub const GEN_AI_TOOL_CALL_ARGUMENTS: Key = "gen_ai.tool.call.arguments";
    pub const GEN_AI_TOOL_CALL_RESULT: Key = "gen_ai.tool.call.result";
    pub const BOT_ERROR_MESSAGE: Key = "bot.error.message";

    pub const CONTENT: [Key; 6] = [
        GEN_AI_INPUT_MESSAGES,
        GEN_AI_OUTPUT_MESSAGES,
        GEN_AI_SYSTEM_INSTRUCTIONS,
        GEN_AI_TOOL_CALL_ARGUMENTS,
        GEN_AI_TOOL_CALL_RESULT,
        BOT_ERROR_MESSAGE,
    ];

    /// Keys identifying humans or conversations, subject to pseudonymization.
    pub const IDENTITY: [Key; 5] = [
        BOT_PRINCIPAL,
        GEN_AI_CONVERSATION_ID,
        BOT_REQUEST_ID,
        USER_ID,
        SESSION_ID,
    ];

    /// Closed export allowlist; keys outside this set are dropped.
    pub const ALLOWED: [Key; 42] = [
        BOT_PROFILE,
        BOT_AGENT,
        BOT_REQUEST_ID,
        BOT_RUN_ID,
        BOT_RUN_ROOT,
        BOT_RUN_PARENT,
        BOT_ATTEMPT_GENERATION,
        BOT_BINDING,
        BOT_TRIGGER,
        BOT_CONVERSATION_KIND,
        BOT_PRINCIPAL,
        BOT_OUTCOME,
        BOT_TOOL_RISK,
        BOT_TOOL_REVISION,
        BOT_CALL_STATE,
        BOT_APPROVAL_RISK,
        BOT_APPROVAL_OUTCOME,
        BOT_EFFECT_STEP,
        BOT_EFFECT_STATE,
        BOT_EFFECT_SEND,
        BOT_HOLD_REASON,
        BOT_DELEGATE_STEP,
        BOT_DELEGATE_CHILD,
        BOT_ADMIN_ACTION,
        BOT_ADMIN_VERDICT,
        ERROR_TYPE,
        GEN_AI_OPERATION_NAME,
        GEN_AI_AGENT_NAME,
        GEN_AI_CONVERSATION_ID,
        GEN_AI_PROVIDER_NAME,
        GEN_AI_REQUEST_MODEL,
        GEN_AI_RESPONSE_MODEL,
        GEN_AI_USAGE_INPUT_TOKENS,
        GEN_AI_USAGE_OUTPUT_TOKENS,
        GEN_AI_TOOL_NAME,
        GEN_AI_TOOL_CALL_ID,
        GEN_AI_TOOL_TYPE,
        HTTP_REQUEST_METHOD,
        HTTP_ROUTE,
        HTTP_RESPONSE_STATUS_CODE,
        USER_ID,
        SESSION_ID,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpanKind {
    Internal,
    Server,
    Client,
    Producer,
    Consumer,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpanIds {
    pub trace: [u8; 16],
    pub span: [u8; 8],
    pub parent: Option<[u8; 8]>,
}

/// Non-cryptographic length-prefixed FNV-1a + splitmix64 mixer for deterministic IDs.
fn mix(parts: &[&[u8]]) -> [u8; 16] {
    let (mut a, mut b) = (0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64);
    for part in parts {
        for byte in (part.len() as u64).to_le_bytes().iter().chain(part.iter()) {
            a = (a ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
            b = (b.rotate_left(7) ^ u64::from(*byte)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
    }
    let fin = |mut z: u64| {
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&fin(a ^ b.rotate_left(32)).to_be_bytes());
    out[8..].copy_from_slice(&fin(b ^ a.rotate_left(17)).to_be_bytes());
    if out == [0; 16] {
        out[15] = 1;
    }
    out
}

fn nonzero8(mut id: [u8; 8]) -> [u8; 8] {
    if id == [0; 8] {
        id[7] = 1;
    }
    id
}

impl SpanIds {
    /// Root span for an attempt; identical across all processes for the same `(profile, run, generation)`.
    pub fn attempt(a: &Attempt) -> Self {
        let g = a.generation.0.to_be_bytes();
        let base = mix(&[b"bot-trace", a.profile.0.as_bytes(), a.run.0.as_bytes(), &g]);
        let mut span = [0; 8];
        span.copy_from_slice(&mix(&[b"bot-span", &base])[..8]);
        Self {
            trace: base,
            span: nonzero8(span),
            parent: None,
        }
    }

    pub fn child(&self, tag: &str, id: &str) -> Self {
        let mut span = [0; 8];
        span.copy_from_slice(&mix(&[b"bot-child", &self.span, tag.as_bytes(), id.as_bytes()])[..8]);
        Self {
            trace: self.trace,
            span: nonzero8(span),
            parent: Some(self.span),
        }
    }
}

/// Deterministic trace-ID-based sampling so all processes make the same decision.
pub fn sample_hit(trace: &[u8; 16], ratio: f64) -> bool {
    if ratio.is_nan() || ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    let low = u64::from_be_bytes(trace[8..].try_into().unwrap_or([0; 8]));
    (low as f64 / u64::MAX as f64) < ratio
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventRecord {
    pub name: Key,
    pub at_nano: u64,
    pub attrs: Vec<(Key, Val)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkRecord {
    pub trace: [u8; 16],
    pub span: [u8; 8],
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpanRecord {
    pub profile: ProfileId,
    pub name: String,
    pub kind: SpanKind,
    pub ids: SpanIds,
    pub start_nano: u64,
    pub end_nano: u64,
    pub attrs: Vec<(Key, Val)>,
    pub events: Vec<EventRecord>,
    pub links: Vec<LinkRecord>,
    /// Fixed-vocabulary failure classification; presence marks the span as errored.
    pub error: Option<&'static str>,
}

pub fn now_nano() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

impl SpanRecord {
    pub fn start(profile: &ProfileId, name: impl Into<String>, ids: SpanIds) -> Self {
        let now = now_nano();
        Self {
            profile: profile.clone(),
            name: name.into(),
            kind: SpanKind::Internal,
            ids,
            start_nano: now,
            end_nano: now,
            attrs: Vec::new(),
            events: Vec::new(),
            links: Vec::new(),
            error: None,
        }
    }

    pub fn kind(mut self, kind: SpanKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn attr(mut self, key: Key, val: impl Into<Val>) -> Self {
        self.attrs.push((key, val.into()));
        self
    }

    pub fn event(mut self, name: Key, attrs: Vec<(Key, Val)>) -> Self {
        self.events.push(EventRecord {
            name,
            at_nano: now_nano(),
            attrs,
        });
        self
    }

    pub fn link(mut self, trace: [u8; 16], span: [u8; 8]) -> Self {
        self.links.push(LinkRecord { trace, span });
        self
    }

    pub fn fail(mut self, class: &'static str) -> Self {
        self.error = Some(class);
        self
    }

    pub fn started_at(mut self, nano: u64) -> Self {
        self.start_nano = nano;
        self
    }

    pub fn end(mut self, tracer: &dyn Tracer) {
        self.end_nano = now_nano().max(self.start_nano);
        tracer.record(self);
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find_map(|(k, v)| match v {
            Val::Str(s) if *k == key => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        self.attrs.iter().find_map(|(k, v)| match v {
            Val::Int(n) if *k == key => Some(*n),
            _ => None,
        })
    }
}

/// Span sink; all methods must be synchronous, non-blocking, and infallible (drop on overflow).
pub trait Tracer: Send + Sync {
    fn record(&self, span: SpanRecord);
}

pub struct NoopTracer;

impl Tracer for NoopTracer {
    fn record(&self, _: SpanRecord) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::{Generation, RunId};
    use std::collections::HashSet;

    fn attempt(run: &str, generation: u64) -> Attempt {
        Attempt {
            profile: ProfileId("alpha".into()),
            run: RunId(run.into()),
            claim: None,
            generation: Generation(generation),
        }
    }

    #[test]
    fn same_attempt_gets_the_same_ids_in_every_process() {
        let a = SpanIds::attempt(&attempt("r-1", 4));
        assert_eq!(a, SpanIds::attempt(&attempt("r-1", 4)));
        let tool = a.child("tool", "c-17");
        assert_eq!(
            tool,
            SpanIds::attempt(&attempt("r-1", 4)).child("tool", "c-17")
        );
        assert_eq!((tool.trace, tool.parent), (a.trace, Some(a.span)));
        assert_ne!(tool.span, a.child("tool", "c-18").span);
        assert_ne!(tool.span, a.child("effect", "c-17").span);
    }

    #[test]
    fn a_new_generation_run_or_profile_starts_a_new_trace() {
        let base = SpanIds::attempt(&attempt("r-1", 4));
        assert_ne!(base.trace, SpanIds::attempt(&attempt("r-1", 5)).trace);
        assert_ne!(base.trace, SpanIds::attempt(&attempt("r-2", 4)).trace);
        let mut other = attempt("r-1", 4);
        other.profile = ProfileId("beta".into());
        assert_ne!(base.trace, SpanIds::attempt(&other).trace);
        // Length prefixing prevents boundary collisions across concatenated parts.
        let x = SpanIds::attempt(&attempt("ab", 1)).trace;
        let mut y = attempt("b", 1);
        y.profile = ProfileId("alphaa".into());
        assert_ne!(x, SpanIds::attempt(&y).trace);
    }

    #[test]
    fn ids_are_unique_and_nonzero_over_many_attempts() {
        let mut seen = HashSet::new();
        for run in 0..2000u64 {
            for generation in 1..4u64 {
                let a = SpanIds::attempt(&attempt(&format!("r-{run}"), generation));
                assert!(a.trace != [0; 16] && a.span != [0; 8]);
                assert!(
                    seen.insert(a.trace)
                        && seen.insert({
                            let mut t = [0u8; 16];
                            t[..8].copy_from_slice(&a.span);
                            t
                        })
                );
            }
        }
    }

    #[test]
    fn sampling_follows_the_trace_id() {
        let t = SpanIds::attempt(&attempt("r-1", 1)).trace;
        assert!(sample_hit(&t, 1.0) && !sample_hit(&t, 0.0) && !sample_hit(&t, f64::NAN));
        assert_eq!(sample_hit(&t, 0.5), sample_hit(&t, 0.5));
        let hits = (0..4000u64)
            .filter(|n| {
                sample_hit(
                    &SpanIds::attempt(&attempt(&format!("r-{n}"), 1)).trace,
                    0.25,
                )
            })
            .count();
        assert!((800..1200).contains(&hits), "{hits}");
    }

    #[test]
    fn vocabulary_is_unique_lowercase_and_closed() {
        let all: HashSet<_> = key::ALLOWED.iter().chain(key::CONTENT.iter()).collect();
        assert_eq!(all.len(), key::ALLOWED.len() + key::CONTENT.len());
        for k in &all {
            assert!(
                k.bytes().all(|c| c.is_ascii_lowercase()
                    || c.is_ascii_digit()
                    || c == b'.'
                    || c == b'_'),
                "{k}"
            );
        }
        assert!(!all.contains(&key::BOT_PRIVATE));
        assert!(key::IDENTITY.iter().all(|k| key::ALLOWED.contains(k)));
        assert!(!key::ALLOWED.iter().any(|k| key::CONTENT.contains(k)));
        // Approval nonces, args, bodies, receipts, and secrets are excluded from the allowlist.
        for banned in ["nonce", "args", "receipt", "body", "secret"] {
            assert!(!key::ALLOWED.iter().any(|k| k.contains(banned)), "{banned}");
        }
    }
}
