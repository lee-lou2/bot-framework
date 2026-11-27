//! Outbox push-reply delivery loop enforcing single-attempt begin/settle semantics.
use crate::{inbound::kind_name, ingress::Ingress, observe, reply::ReplyBody};
use bot_core::{
    intake::{DeliveryOutcome, DeliveryScope, NotSentReason, Outbox, Outgoing},
    port::{Error, Gateway, Reply},
    profile::Snapshot,
    trace::now_nano,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const BATCH: u32 = 8;
const IDLE: Duration = Duration::from_millis(500);
const BACKOFF: Duration = Duration::from_secs(2);
/// Sends exceeding this timeout settle as `Held` (ambiguous outcome, no auto-retry).
const SEND_TIMEOUT: Duration = Duration::from_secs(150);

/// Result of a single push attempt: `Held` is post-write ambiguity, `NotSent` is confirmed pre-execution failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    Sent {
        receipt: String,
    },
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    Rejected,
    Held,
}

pub trait Sender: Send + Sync {
    fn scope(&self) -> DeliveryScope;
    /// Checks whether the current revision still permits sending to this binding and destination.
    fn allows(&self, snapshot: &Snapshot, item: &Outgoing) -> bool;
    fn send<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        item: &'a Outgoing,
        body: &'a ReplyBody,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + 'a>>;
}

pub struct Deliverer {
    outbox: Arc<dyn Outbox>,
    ingress: Arc<Ingress>,
    sender: Arc<dyn Sender>,
    wake: Notify,
}

impl Deliverer {
    pub fn new(outbox: Arc<dyn Outbox>, ingress: Arc<Ingress>, sender: Arc<dyn Sender>) -> Self {
        Self {
            outbox,
            ingress,
            sender,
            wake: Notify::new(),
        }
    }

    pub async fn tick(&self) -> Result<usize, Error> {
        let items = self.outbox.claim(&self.sender.scope(), BATCH).await?;
        let count = items.len();
        for item in items {
            self.deliver_one(item).await;
        }
        Ok(count)
    }

    pub async fn run(&self, stop: CancellationToken) {
        loop {
            let pause = match self.tick().await {
                Ok(n) if n >= BATCH as usize => Duration::ZERO,
                Ok(_) => IDLE,
                Err(error) => {
                    tracing::warn!(?error, "failed to claim reply deliveries");
                    BACKOFF
                }
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }

    async fn deliver_one(&self, item: Outgoing) {
        let started = now_nano();
        let key = item.key.clone();
        let binding = item.binding.0.clone();
        let conversation = kind_name(&item.destination.kind);
        // Delivery generations are not worker attempts; correlate via (profile, run) without a synthetic Attempt.
        let trace = |outcome: &'static str, error: Option<&'static str>| {
            observe::record_delivery(
                &**self.ingress.tracer(),
                &observe::Delivered {
                    profile: &key.profile,
                    run: &key.run,
                    binding: &binding,
                    conversation,
                    outcome,
                    error,
                    started,
                },
            );
        };
        let snapshot = match self.ingress.routes() {
            Ok(routes) => routes.snapshot(&key.profile).cloned(),
            Err(_) => None,
        };
        let Some(snapshot) = snapshot else {
            // Skip inactive profiles and let the lease expire for another owner.
            tracing::warn!(profile = %key.profile.0, run = %key.run.0, "skipping delivery for inactive profile");
            return trace("skipped", None);
        };
        let body = ReplyBody::parse(&item.body);
        let allowed = snapshot.members.contains_key(&item.principal)
            && snapshot.bindings.contains_key(&item.binding)
            && self.sender.allows(&snapshot, &item);
        let revision = snapshot.revision.clone();
        let body = match body {
            Ok(body) if allowed => body,
            invalid => {
                if let Err(reason) = invalid {
                    tracing::warn!(profile = %key.profile.0, run = %key.run.0, reason, "invalid reply body");
                }
                // Record begin + Rejected so invalid/disallowed items are not re-claimed.
                if self.outbox.begin(key.clone(), revision).await.is_ok()
                    && let Err(error) = self
                        .outbox
                        .settle(key.clone(), DeliveryOutcome::Rejected)
                        .await
                {
                    tracing::error!(?error, "failed to record rejected delivery settlement");
                }
                return trace("rejected", Some("rejected"));
            }
        };
        // Only the caller whose `begin` succeeds may transmit.
        if self.outbox.begin(key.clone(), revision).await.is_err() {
            return trace("skipped", None);
        }
        let delivery =
            tokio::time::timeout(SEND_TIMEOUT, self.sender.send(&snapshot, &item, &body))
                .await
                .unwrap_or(Delivery::Held);
        let (outcome, label, error) = match delivery {
            Delivery::Sent { receipt } => {
                (DeliveryOutcome::Confirmed { receipt }, "confirmed", None)
            }
            Delivery::NotSent {
                reason,
                retry_after,
            } => (
                DeliveryOutcome::NotSent {
                    reason,
                    retry_after,
                },
                "not_sent",
                Some("not_sent"),
            ),
            Delivery::Rejected => (DeliveryOutcome::Rejected, "rejected", Some("rejected")),
            Delivery::Held => (DeliveryOutcome::Held, "held", Some("held")),
        };
        if let Err(error) = self.outbox.settle(key.clone(), outcome).await {
            // Do not retransmit on settle failure; the store treats `sending` as ambiguous.
            tracing::error!(?error, "failed to record reply delivery settlement");
        }
        trace(label, error);
    }
}

/// In-process wakeup hint from the runtime; polling remains the cross-process baseline.
impl Gateway for Deliverer {
    fn deliver(
        &self,
        _run: bot_core::id::RunId,
        _profile: bot_core::id::ProfileId,
    ) -> Reply<'_, ()> {
        self.wake.notify_one();
        Box::pin(std::future::ready(Ok(())))
    }
}
