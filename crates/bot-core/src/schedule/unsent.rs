//! Reasons for unsent scheduled replies (`UnsentReason`) and settlement fencing (`DeliveryFence`).

use crate::{
    command::JsonPayload,
    id::{ApprovalId, Attempt, PrincipalId},
};

/// Verifies the active claim while running, or the occurrence's valid approval card and owner while parked.
pub enum DeliveryFence {
    Claim(Attempt),
    Approval {
        nonce: ApprovalId,
        actor: PrincipalId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsentReason {
    /// Owner explicitly rejected delivery or revoked approval before consumption.
    Rejected,
    /// Current registry rejected the stored destination or the binding now resolves to a different recipient.
    Unrouted,
}

/// Fixed public-safe notice containing no draft, recipient, or instruction.
pub const UNROUTED_NOTICE: &str = "Scheduled delivery route changed; this reply was not sent.";

impl UnsentReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Rejected => "delivery_rejected",
            Self::Unrouted => "delivery_unrouted",
        }
    }

    pub fn receipt(self) -> JsonPayload {
        JsonPayload(serde_json::json!({ "status": self.code() }).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_rejection_and_route_removal_have_distinct_fixed_receipts() {
        assert_eq!(
            UnsentReason::Rejected.receipt().0,
            r#"{"status":"delivery_rejected"}"#
        );
        assert_eq!(
            UnsentReason::Unrouted.receipt().0,
            r#"{"status":"delivery_unrouted"}"#
        );
    }
}
