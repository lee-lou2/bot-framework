//! Durable checkpoint recording for deterministic pre-execution CLI tool refusals.
//!
//! At verified `CallId::cli` positions, the exact refusal word shown to the model is persisted
//! as a `CliRefusal` marker before returning it so redeliveries replay the same word without
//! re-evaluating or consuming retries. Ledger records take precedence (`CallBook::confirm`);
//! transient `Unavailable` / `Stale` failures are never sealed as refusals.

use super::{Session, Shared};
use bot_core::{
    cli::{CliRefusal, error_word},
    command::JsonPayload,
    id::{CallId, ToolId},
    port::Error,
};

/// Returns the refusal word for the model, sealing a marker first when deterministic.
pub(super) async fn word(
    shared: &Shared,
    session: &Session,
    (call, tool, args): (CallId, ToolId, JsonPayload),
    error: &Error,
) -> &'static str {
    let Some(refusal) = crate::service::preflight::expected(error) else {
        return error_word(error);
    };
    let attempt = session.attempt.clone();
    match shared
        .parts
        .calls
        .confirm(attempt.clone(), call.clone(), tool, args, refusal)
        .await
    {
        Ok(true) => seal(shared, session, call, refusal).await,
        Ok(false) => error_word(error),
        Err(failure) => error_word(&failure),
    }
}

/// Persists a confirmed refusal marker and returns its word (or the write failure word).
pub(super) async fn seal(
    shared: &Shared,
    session: &Session,
    call: CallId,
    refusal: CliRefusal,
) -> &'static str {
    match shared
        .parts
        .turns
        .refused(session.attempt.clone(), call, refusal)
        .await
    {
        Ok(()) => refusal.word(),
        Err(failure) => error_word(&failure),
    }
}
