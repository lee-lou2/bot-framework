/// 사용자 예약을 더 이상 발화하지 않는 이유. 같은 종료를 다시 받아도 감사와 안내는 한 번이다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CloseReason {
    Finished,
    MemberRevoked,
    BindingRevoked,
    AgentRevoked,
    PermissionRevoked,
    InvalidTiming,
    /// 저장된 목적지(종류·ID)를 읽을 수 없다(저장소 스키마는 받지만 지금 해석할 수 없는 값).
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

    /// 원래 대화에 넣을 공개 가능한 안내. 수신자 식별자나 예약 내용이 들어가지 않는다.
    pub fn notice(self) -> Option<&'static str> {
        (self == Self::Unrouted).then_some("예약 메시지의 목적지가 등록부에 없어 예약을 멈췄어요")
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
                (code == "unrouted")
                    .then_some("예약 메시지의 목적지가 등록부에 없어 예약을 멈췄어요")
            );
        }
    }
}
