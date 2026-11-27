//! Worker OpenTelemetry span recording for attempt roots, model turns, and generation handover links.
//!
//! Never attaches user text, tool arguments, or results—only classifications, counts, and fixed codes.

use super::Outcome;
use bot_core::{
    id::{Attempt, Generation},
    state::{ConversationKind, RunContext},
    trace::{SpanIds, SpanRecord, Tracer, key},
};
use std::sync::Arc;

fn kind_code(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

pub(crate) struct RunSpan {
    tracer: Arc<dyn Tracer>,
    profile: bot_core::id::ProfileId,
    ids: SpanIds,
    record: SpanRecord,
}

impl RunSpan {
    pub(crate) fn begin(tracer: Arc<dyn Tracer>, ctx: &RunContext) -> Self {
        let attempt = &ctx.attempt;
        let ids = SpanIds::attempt(attempt);
        let mut record = SpanRecord::start(&attempt.profile, "worker.run", ids)
            .attr(key::BOT_PROFILE, attempt.profile.0.clone())
            .attr(key::BOT_AGENT, ctx.agent.0.clone())
            .attr(key::GEN_AI_AGENT_NAME, ctx.agent.0.clone())
            .attr(key::GEN_AI_OPERATION_NAME, "invoke_agent")
            .attr(key::BOT_RUN_ID, attempt.run.0.clone())
            .attr(key::BOT_RUN_ROOT, ctx.root.0.clone())
            .attr(key::BOT_ATTEMPT_GENERATION, attempt.generation.0)
            .attr(key::BOT_PRINCIPAL, ctx.principal.0.clone())
            .attr(key::USER_ID, ctx.principal.0.clone())
            .attr(key::GEN_AI_CONVERSATION_ID, ctx.conversation.id.clone())
            .attr(key::SESSION_ID, ctx.conversation.id.clone())
            .attr(
                key::BOT_CONVERSATION_KIND,
                kind_code(&ctx.conversation.kind),
            );
        if let Some(trigger) = &ctx.trigger {
            record = record.attr(key::BOT_TRIGGER, trigger.id.clone());
        }
        if attempt.generation.0 > 1 {
            let before = Attempt {
                generation: Generation(attempt.generation.0 - 1),
                ..attempt.clone()
            };
            let prev = SpanIds::attempt(&before);
            record = record.link(prev.trace, prev.span);
        }
        Self {
            tracer,
            profile: attempt.profile.clone(),
            ids,
            record,
        }
    }

    pub(crate) fn child(&self, tag: &str, id: &str, name: &str) -> SpanRecord {
        SpanRecord::start(&self.profile, name, self.ids.child(tag, id))
    }

    pub(crate) fn emit(&self, record: SpanRecord) {
        record.end(self.tracer.as_ref());
    }

    pub(crate) fn end(self, outcome: &Outcome, error: Option<&'static str>) {
        let label = match outcome {
            Outcome::Succeeded => "succeeded",
            Outcome::Parked => "waiting",
            Outcome::Failed(_) => "failed",
            Outcome::Cancelled => "cancelled",
            Outcome::Lost => "lost",
            Outcome::Released => "released",
        };
        let mut record = self.record.attr(key::BOT_OUTCOME, label);
        if let Some(code) = error {
            record = record.fail(code);
        }
        record.end(self.tracer.as_ref());
    }
}
