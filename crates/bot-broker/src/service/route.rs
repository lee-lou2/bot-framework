//! Host route-registry verification for explicit scheduled-delivery destinations.
//!
//! Schedule creation, delivery approval, and approval decisions all check the current profile route registry
//! without external service lookups, trusting card-embedded target JSON only when its SHA-256 matches the approval hash.

use super::{BrokerService, call::Flow};
use crate::{approval::card_value, canon::sha256};
use bot_core::{
    cards::LABEL_ARGS,
    command::{CallResult, JsonPayload},
    id::{Attempt, BindingId, CallId, ProfileId},
    ledger::{ApprovalState, CallPhase},
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UnsentReason},
    state::{Conversation, ConversationKind},
};
use serde_json::Value;

pub(super) fn same_route(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

fn target_of(value: &Value) -> Option<DeliveryTarget> {
    let conversation = value.get("conversation")?;
    let kind = match conversation.get("kind")?.as_str()? {
        "direct" => ConversationKind::Direct,
        "channel" => ConversationKind::Channel,
        _ => return None,
    };
    Some(DeliveryTarget {
        binding: BindingId(value.get("binding")?.as_str()?.to_owned()),
        conversation: Conversation {
            kind,
            id: conversation.get("id")?.as_str()?.to_owned(),
            thread: match conversation.get("thread")? {
                Value::Null => None,
                Value::String(thread) => Some(thread.clone()),
                _ => return None,
            },
        },
    })
}

/// Extracts `key` from the card's `label` document only if its SHA-256 matches `approved`.
fn card_target(
    card: &JsonPayload,
    label: &str,
    key: &str,
    approved: [u8; 32],
) -> Result<Option<DeliveryTarget>, Error> {
    let document = card_value(card, label).ok_or(Error::Invalid)?;
    if sha256(&document) != approved {
        return Err(Error::Invalid);
    }
    let value: Value = crate::json::parse(&document).ok_or(Error::Invalid)?;
    match value.get(key) {
        None => Ok(None),
        Some(target) => target_of(target).map(Some).ok_or(Error::Invalid),
    }
}

pub(super) fn created_destination(
    card: &JsonPayload,
    approved: [u8; 32],
) -> Result<Option<DeliveryTarget>, Error> {
    card_target(card, LABEL_ARGS, "destination", approved)
}

pub(super) fn delivery_destination(
    card: &JsonPayload,
    approved: [u8; 32],
) -> Result<DeliveryTarget, Error> {
    card_target(
        card,
        bot_core::schedule::DELIVERY_CARD_LABEL,
        "target",
        approved,
    )?
    .ok_or(Error::Invalid)
}

impl BrokerService {
    /// Resolves `target` to its canonical route; returns `Missing` (unsupported) when no route registry is attached.
    pub(super) fn route(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error> {
        self.routes
            .as_ref()
            .ok_or(Error::Missing)?
            .resolve(profile, target)
    }

    /// Handles a definitively removed or reassigned destination: closes delivery occurrences as `Unrouted`,
    /// or refuses schedule creation without persisting anything.
    pub(super) async fn route_gone(
        &self,
        flow: &Flow,
        target: &DeliveryTarget,
    ) -> Result<CallResult, Error> {
        if flow.tool.0 == DELIVER_TOOL {
            return self
                .finish_unsent(&flow.attempt, target, UnsentReason::Unrouted)
                .await;
        }
        let _ = self
            .note(
                &flow.attempt.profile,
                &flow.ctx.principal,
                "call.refuse",
                &flow.tool,
                Some(flow.sealed.sha256),
                self.route_word(),
            )
            .await;
        Err(Error::Denied)
    }

    pub(super) async fn finish_unsent(
        &self,
        attempt: &Attempt,
        target: &DeliveryTarget,
        reason: UnsentReason,
    ) -> Result<CallResult, Error> {
        self.replies
            .finish_unsent(
                DeliveryFence::Claim(attempt.clone()),
                target.clone(),
                reason,
            )
            .await
            .map(CallResult::Done)
    }

    pub(super) async fn owner_rejected(&self, attempt: &Attempt) -> Result<bool, Error> {
        let ledger = &self.parts.ledger;
        let call = CallId(DELIVER_CALL.into());
        let Some(CallPhase::Open {
            approval: Some(nonce),
        }) = ledger
            .lookup(attempt.clone(), call)
            .await?
            .map(|record| record.phase)
        else {
            return Ok(false);
        };
        let stored = ledger.approval(nonce).await?;
        Ok(matches!(
            stored.state,
            ApprovalState::Denied | ApprovalState::Revoked
        ))
    }

    /// Owner rejection takes precedence over an unrouted destination (producing a silent `Rejected` receipt);
    /// otherwise `Gone` closes as `Unrouted` and `Unsure` retries via `Unavailable`.
    pub(super) async fn unsettled_route(
        &self,
        attempt: &Attempt,
        target: &DeliveryTarget,
        route: Route,
    ) -> Result<CallResult, Error> {
        if self.owner_rejected(attempt).await? {
            return self
                .finish_unsent(attempt, target, UnsentReason::Rejected)
                .await;
        }
        match route {
            Route::Gone => {
                self.finish_unsent(attempt, target, UnsentReason::Unrouted)
                    .await
            }
            Route::Same | Route::Unsure => Err(Error::Unavailable),
        }
    }

    pub(super) fn route_word(&self) -> &'static str {
        if self.routes.is_some() {
            "destination_unregistered"
        } else {
            "destination_unsupported"
        }
    }

    /// Distinguishes definitive route drift (`Gone`) from transient/missing registry checks (`Unsure`)
    /// so transient errors retry rather than dropping the occurrence.
    pub(super) fn route_state(&self, profile: &ProfileId, target: &DeliveryTarget) -> Route {
        match self.route(profile, target) {
            Ok(now) if same_route(target, &now) => Route::Same,
            Ok(_) | Err(Error::Denied | Error::Invalid) => Route::Gone,
            Err(_) => Route::Unsure,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Route {
    Same,
    Gone,
    Unsure,
}
