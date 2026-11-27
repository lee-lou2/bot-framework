//! Active-revision guard for model upstream requests and open SSE streams (`GuardedUpstream`).

use bot_broker::model::{ChunkSource, Upstream, UpstreamRequest, UpstreamResponse, UpstreamStream};
use bot_core::{
    active::ActiveCheck,
    port::{Error, Reply},
};
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, sleep_until, timeout_at};

const STREAM_CHECK: Duration = Duration::from_secs(1);
/// Bounds both pre-send verification and how long an open stream may continue while the store is unreachable.
const STREAM_GRACE: Duration = Duration::from_secs(10);

/// Verifies the host's active revision before every upstream request and periodically across open streams.
///
/// Pre-send `Stale` maps to 403 `permission_error` (non-retryable) and `Unavailable` maps to 503
/// `overloaded_error`, sending nothing upstream in either case. Open streams share one deadline
/// (`first_due + grace`) across verification and retries and never emit chunks past the deadline.
pub struct GuardedUpstream {
    inner: Arc<dyn Upstream>,
    active: Arc<dyn ActiveCheck>,
    every: Duration,
    grace: Duration,
}

impl GuardedUpstream {
    pub fn new(inner: Arc<dyn Upstream>, active: Arc<dyn ActiveCheck>) -> Self {
        Self::with_timing(inner, active, STREAM_CHECK, STREAM_GRACE)
    }

    pub fn with_timing(
        inner: Arc<dyn Upstream>,
        active: Arc<dyn ActiveCheck>,
        every: Duration,
        grace: Duration,
    ) -> Self {
        Self {
            inner,
            active,
            every,
            grace,
        }
    }

    /// Rejects late successes that arrive at or after `limit`, while still honoring late `Stale` rejections.
    async fn confirm(&self) -> Result<(), Error> {
        let limit = Instant::now() + self.grace;
        match timeout_at(limit, self.active.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            Ok(Err(error)) => Err(error),
            _ => Err(Error::Unavailable),
        }
    }
}

impl Upstream for GuardedUpstream {
    fn send(&self, request: UpstreamRequest) -> Reply<'_, UpstreamResponse> {
        Box::pin(async move {
            self.confirm().await?;
            self.inner.send(request).await
        })
    }

    fn stream(&self, request: UpstreamRequest) -> Reply<'_, UpstreamStream> {
        Box::pin(async move {
            self.confirm().await?;
            let UpstreamStream { status, body } = self.inner.stream(request).await?;
            Ok(UpstreamStream {
                status,
                body: Box::new(Watched {
                    inner: body,
                    active: self.active.clone(),
                    every: self.every,
                    grace: self.grace,
                    next_check: Instant::now() + self.every,
                    deadline: None,
                }),
            })
        })
    }
}

struct Watched {
    inner: Box<dyn ChunkSource>,
    active: Arc<dyn ActiveCheck>,
    every: Duration,
    grace: Duration,
    next_check: Instant,
    deadline: Option<Instant>,
}

impl ChunkSource for Watched {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            let Self {
                inner,
                active,
                every,
                grace,
                next_check,
                deadline,
            } = self;
            // Keep the chunk future alive across periodic checks so no chunk is dropped.
            let mut pending = inner.next();
            loop {
                if deadline.is_some_and(|limit| Instant::now() >= limit) {
                    return Err(Error::Unavailable);
                }
                let wake = deadline.map_or(*next_check, |limit| (*next_check).min(limit));
                // Biased so continuous chunk arrival cannot starve revision checks or deadlines.
                tokio::select! {
                    biased;
                    () = sleep_until(wake) => {
                        if deadline.is_some_and(|limit| Instant::now() >= limit) {
                            return Err(Error::Unavailable);
                        }
                        if Instant::now() < *next_check {
                            continue;
                        }
                        let limit = *deadline.get_or_insert(*next_check + *grace);
                        match timeout_at(limit, active.verify()).await {
                            Ok(Ok(())) if Instant::now() < limit => {
                                *deadline = None;
                                *next_check = Instant::now() + *every;
                            }
                            Ok(Err(Error::Stale)) => return Err(Error::Stale),
                            _ => *next_check = (Instant::now() + *every).min(limit),
                        }
                    }
                    chunk = &mut pending => {
                        if deadline.is_some_and(|limit| Instant::now() >= limit) {
                            return Err(Error::Unavailable);
                        }
                        return chunk;
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
