//! Terminal closure reasons and user-facing notices for chat-created schedules.

/// Closing a schedule with the same reason is idempotent for both audit and notice emission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseReason {
    Finished,
    MemberRevoked,
    BindingRevoked,
    AgentRevoked,
    PermissionRevoked,
    InvalidTiming,
    /// Stored destination kind or ID cannot be interpreted by the current runtime.
    InvalidDestination,
    Unrouted,
    ProfileInactive,
}

impl CloseReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Finished => "finished",
            Self::MemberRevoked => "member_revoked",
            Self::BindingRevoked => "binding_revoked",
            Self::AgentRevoked => "agent_revoked",
            Self::PermissionRevoked => "permission_revoked",
            Self::InvalidTiming => "invalid_timing",
            Self::InvalidDestination => "invalid_destination",
            Self::Unrouted => "unrouted",
            Self::ProfileInactive => "profile_inactive",
        }
    }

    /// Public notice posted back to the origin conversation; must never include recipient IDs or schedule content.
    pub fn notice(self) -> Option<&'static str> {
        (self == Self::Unrouted)
            .then_some("Scheduled message destination is no longer registered; schedule stopped.")
    }
}

#[cfg(test)]
mod tests {
    use super::CloseReason::*;

    #[test]
    fn closure_codes_and_original_conversation_notice_are_stable() {
        for (reason, code) in [
            (Finished, "finished"),
            (MemberRevoked, "member_revoked"),
            (BindingRevoked, "binding_revoked"),
            (AgentRevoked, "agent_revoked"),
            (PermissionRevoked, "permission_revoked"),
            (InvalidTiming, "invalid_timing"),
            (InvalidDestination, "invalid_destination"),
            (Unrouted, "unrouted"),
            (ProfileInactive, "profile_inactive"),
        ] {
            assert_eq!(reason.code(), code);
            assert_eq!(
                reason.notice(),
                (code == "unrouted").then_some(
                    "Scheduled message destination is no longer registered; schedule stopped."
                )
            );
        }
    }
}
