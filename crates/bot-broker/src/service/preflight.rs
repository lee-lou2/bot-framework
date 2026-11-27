//! Deterministic pre-execution refusal checks shared by the CLI front and stepped worker.
//!
//! Distinguishes true `UnknownTool`/`Invalid` against the pinned revision from store errors
//! that surface as `Missing`/`Invalid` through `Broker::call`. Returns `Err` when the revision
//! or claim cannot be verified.

use super::{
    BrokerService, memory, schedule,
    share::{self, SHARE_TOOL},
};
use crate::resolve::find_tool;
use bot_core::{
    cli::CliRefusal,
    command::{JsonPayload, ToolCall},
    id::{Attempt, ToolId},
    ledger::CallLedger,
    port::{Error, Reply},
};

/// Built-in delegation tool name exposed over MCP; validated directly by the front.
pub use bot_core::builtin::DELEGATE_TOOL;

pub(crate) fn expected(error: &Error) -> Option<CliRefusal> {
    Some(match error {
        Error::Missing => CliRefusal::UnknownTool,
        Error::Invalid => CliRefusal::Invalid,
        Error::Denied => CliRefusal::Denied,
        Error::Conflict => CliRefusal::Conflict,
        Error::Stale => CliRefusal::Stale,
        Error::RateLimited => CliRefusal::RateLimited,
        Error::Unavailable => return None,
    })
}

/// Ledger state takes precedence over preflight; `UnknownTool`/`Invalid` must match the pinned revision.
pub(crate) async fn confirm(
    ledger: &dyn CallLedger,
    judge: Option<&dyn Preflight>,
    call: ToolCall,
    expected: CliRefusal,
) -> Result<bool, Error> {
    let known = ledger.lookup(call.attempt.clone(), call.call).await?;
    if !CliRefusal::can_replay(known.as_ref()) {
        return Ok(false);
    }
    match expected {
        CliRefusal::UnknownTool | CliRefusal::Invalid => {
            let Some(judge) = judge else { return Ok(false) };
            Ok(judge.refusal(call.attempt, call.tool, call.args).await? == Some(expected))
        }
        CliRefusal::Denied | CliRefusal::Conflict | CliRefusal::Stale | CliRefusal::RateLimited => {
            Ok(true)
        }
    }
}

pub trait Preflight: Send + Sync {
    /// Checks whether a call is deterministically refused before execution for the current claim.
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>>;
}

impl Preflight for BrokerService {
    fn refusal(
        &self,
        attempt: Attempt,
        tool: ToolId,
        args: JsonPayload,
    ) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            let ctx = self.parts.ledger.context(attempt.clone()).await?;
            // Host-reserved tool: always `UnknownTool` to models.
            if tool.0 == schedule::DELIVER_TOOL {
                return Ok(Some(CliRefusal::UnknownTool));
            }
            // Delegation is validated directly by the front, not the catalog.
            if tool.0 == DELEGATE_TOOL {
                return Ok(None);
            }
            let pinned = self
                .parts
                .catalog
                .lookup(&attempt.profile, &ctx.work.revision)
                .ok_or(Error::Unavailable)?;
            // Built-in tools are outside the catalog; only argument validation is deterministic here.
            if tool.0 == SHARE_TOOL {
                return Ok(share::judged(&args));
            }
            if memory::is_builtin(&tool.0) {
                return Ok(memory::judged(&tool.0, &args));
            }
            if schedule::is_builtin(&tool.0) {
                return Ok(schedule::judged(
                    &tool.0,
                    &args,
                    &pinned.timezone,
                    self.parts.clock.now(),
                ));
            }
            if let Some(spec) = bot_core::builtin::manage_tool(&tool.0) {
                let decl = spec.declaration();
                return match self.parts.sealer.seal(&decl, &args) {
                    Ok(_) => Ok(None),
                    Err(Error::Invalid) => Ok(Some(CliRefusal::Invalid)),
                    Err(error) => Err(error),
                };
            }
            let Some(declared) = find_tool(&pinned, &ctx.agent, &tool) else {
                return Ok(Some(CliRefusal::UnknownTool));
            };
            match self.parts.sealer.seal(declared, &args) {
                Ok(_) => Ok(None),
                Err(Error::Invalid) => Ok(Some(CliRefusal::Invalid)),
                Err(error) => Err(error),
            }
        })
    }
}
