//! Slack installation (`team_id`) verification for posted approval cards against host routes.
//!
//! Profile snapshots name workspace aliases only, so redeploying an alias to a different `team_id`
//! escapes revision comparison. Before clicking or consuming approval, this checks `CardReceipt::team`
//! against the canonical `team/channel` returned by `ScheduleRoutes` (and DB commits re-verify under lock).

use super::{BrokerService, route::Route};
use crate::approver::openai::Origin;
use bot_core::{
    id::{BindingId, ProfileId},
    ledger::StoredApproval,
    port::Error,
    profile::{Adapter, ApprovalRoute, Snapshot},
    schedule::DeliveryTarget,
    state::{Conversation, ConversationKind},
};

impl BrokerService {
    pub(super) fn site_state(
        &self,
        profile: &ProfileId,
        snapshot: &Snapshot,
        binding: &BindingId,
        posted: &str,
    ) -> Route {
        let Some(Adapter::SlackChannel { channel, .. }) =
            snapshot.bindings.get(binding).map(|b| &b.source)
        else {
            return Route::Gone;
        };
        let target = DeliveryTarget {
            binding: binding.clone(),
            conversation: Conversation {
                kind: ConversationKind::Channel,
                id: channel.clone(),
                thread: None,
            },
        };
        match self.route(profile, &target) {
            Ok(now) => match now.conversation.id.split_once('/') {
                Some((team, _)) if team == posted => Route::Same,
                _ => Route::Gone,
            },
            Err(Error::Denied | Error::Invalid) => Route::Gone,
            Err(_) => Route::Unsure,
        }
    }

    /// Returns `Same` for local Admin UI routes, and `Gone` for Slack routes lacking a posted receipt.
    pub(super) fn card_site(
        &self,
        stored: &StoredApproval,
        snapshot: &Snapshot,
        found: &Origin,
    ) -> Route {
        let ApprovalRoute::Slack { binding, .. } = &found.route else {
            return Route::Same;
        };
        match &stored.delivery {
            Some(receipt) => self.site_state(
                &stored.approval.attempt.profile,
                snapshot,
                binding,
                &receipt.team,
            ),
            None => Route::Gone,
        }
    }
}
