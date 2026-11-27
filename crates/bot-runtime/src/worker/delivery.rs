//! Scheduled cross-destination reply freezing (`ScheduledReplies::draft`) and approval release (`ScheduleDelivery::release`).

use super::{Flow, Worker};
use bot_core::{
    command::{CallResult, Failure, Finish},
    port::Error,
    schedule::{ScheduleDelivery, ScheduledReplies},
    state::{RunContext, RunOrigin},
};
use std::sync::Arc;

#[derive(Clone)]
pub struct Delivery {
    pub replies: Arc<dyn ScheduledReplies>,
    pub release: Arc<dyn ScheduleDelivery>,
}

impl Worker {
    /// Freezes a new completion (`Some`) or loads an already-frozen draft (`None`) and routes it through approval release.
    pub(super) async fn delivery(&self, ctx: &RunContext, finish: Option<Finish>) -> Option<Flow> {
        let delivery = self.delivery.as_ref()?;
        if ctx.origin != RunOrigin::Schedule || ctx.root != ctx.attempt.run {
            return None;
        }
        match delivery.replies.draft(ctx.attempt.clone(), finish).await {
            Ok(None) => None,
            // Forward `Conflict` (frozen under an older active set) to `release` so revoked recipients still record an unsent receipt first.
            Ok(Some(_)) | Err(Error::Conflict) => Some(
                match settled(delivery.release.release(ctx.attempt.clone()).await) {
                    // `Stale` may reflect an expired/superseded approval card rather than lost claim ownership.
                    Err(Error::Stale) => self.rejudge(delivery, ctx).await,
                    Ok(flow) => flow,
                    Err(error) => refused(error, "delivery_unavailable"),
                },
            ),
            Err(error) => Some(refused(error, "draft_unavailable")),
        }
    }

    /// Disambiguates `Stale` by probing the claim/active-set fence via `target`; retries `release` once if the claim is still valid.
    async fn rejudge(&self, delivery: &Delivery, ctx: &RunContext) -> Flow {
        match delivery.replies.target(ctx.attempt.clone()).await {
            Err(Error::Stale) => return Flow::Lost,
            Err(error) => return refused(error, "delivery_unavailable"),
            Ok(_) => {}
        }
        match settled(delivery.release.release(ctx.attempt.clone()).await) {
            Ok(flow) => flow,
            Err(Error::Stale) => Flow::Fail(Failure::Transient("delivery_card_stale")),
            Err(error) => refused(error, "delivery_unavailable"),
        }
    }
}

fn settled(result: Result<CallResult, Error>) -> Result<Flow, Error> {
    result.map(|result| match result {
        CallResult::Done(_) => Flow::Delivered,
        CallResult::Awaiting(_) | CallResult::Held(_) | CallResult::Passed => Flow::Parked,
    })
}

fn refused(error: Error, transient: &'static str) -> Flow {
    match error {
        Error::Invalid => Flow::Fail(Failure::Permanent("card_unsupported")),
        Error::Denied => Flow::Fail(Failure::Permanent("delivery_denied")),
        Error::Missing => Flow::Fail(Failure::Permanent("delivery_missing")),
        Error::Conflict => Flow::Fail(Failure::Permanent("delivery_stale")),
        Error::Stale => Flow::Lost,
        Error::Unavailable | Error::RateLimited => Flow::Fail(Failure::Transient(transient)),
    }
}
