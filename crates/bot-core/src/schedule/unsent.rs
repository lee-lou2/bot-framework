//! 보내지 못한 예약 회신의 이유(`UnsentReason`)와 정산을 막는 펜스(`DeliveryFence`).

use crate::{
    command::JsonPayload,
    id::{ApprovalId, Attempt, PrincipalId},
};

/// 실행 중에는 현재 claim, 승인 대기 중에는 해당 발생의 유효한 카드와 인증된 소유자를 검증한다.
pub enum DeliveryFence {
    Claim(Attempt),
    Approval {
        nonce: ApprovalId,
        actor: PrincipalId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsentReason {
    /// 소유자가 전송을 명시적으로 거절하거나 소비 전 승인을 철회했다.
    Rejected,
    /// 현재 등록부가 저장된 목적지를 거절하거나 같은 바인딩이 다른 실제 수신자를 가리킨다.
    Unrouted,
}

/// 공개 대화에도 남길 수 있는 고정 안내. 초안, 수신자, 지시문은 포함하지 않는다.
pub const UNROUTED_NOTICE: &str = "예약 수신 경로가 바뀌어 이번 답은 보내지 않았습니다.";

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
