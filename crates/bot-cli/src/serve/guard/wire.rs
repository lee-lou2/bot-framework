//! Active-revision guard (`GuardedWire`) verified before every outbound remote MCP HTTP request.
// Because an MCP session spans multiple HTTP round-trips (version negotiation, tools/list, tools/call),
// re-verifying active revision before each send prevents a stale worker from sending bearer tokens or
// write calls to a destination revoked by a newer activation.
use bot_broker::mcp::wire::{RemoteWire, WireFail, WireReply, WireRequest};
use bot_core::active::ActiveCheck;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

const GRACE: Duration = Duration::from_secs(10);

pub struct GuardedWire {
    inner: Arc<dyn RemoteWire>,
    active: Arc<dyn ActiveCheck>,
    grace: Duration,
}

impl GuardedWire {
    pub fn new(inner: Arc<dyn RemoteWire>, active: Arc<dyn ActiveCheck>) -> Self {
        Self::with_grace(inner, active, GRACE)
    }

    pub fn with_grace(
        inner: Arc<dyn RemoteWire>,
        active: Arc<dyn ActiveCheck>,
        grace: Duration,
    ) -> Self {
        Self {
            inner,
            active,
            grace,
        }
    }

    async fn confirm(&self) -> Result<(), WireFail> {
        let limit = Instant::now() + self.grace;
        // Reject completions that arrive at or past the deadline even if both resolve in the same poll.
        match timeout_at(limit, self.active.verify()).await {
            Ok(Ok(())) if Instant::now() < limit => Ok(()),
            _ => Err(WireFail::Stale),
        }
    }
}

impl RemoteWire for GuardedWire {
    fn send(&self, request: WireRequest) -> WireReply<'_> {
        Box::pin(async move {
            self.confirm().await?;
            self.inner.send(request).await
        })
    }
}

#[cfg(test)]
mod tests;
