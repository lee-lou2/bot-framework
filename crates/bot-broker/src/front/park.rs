//! 승인·보류·위임으로 시도가 대기에 들어간 뒤(원장이 이미 run을 대기로 보내고 리스를 놓았다) CLI가 모델을 더 부르거나 완료로
//! 끝나지 못하게 그 시도의 세션만 거두고, 호스트에 알린다. 다른 claim의 세션은 건드리지 않는다.

use bot_core::id::Attempt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkReason {
    /// 승인을 기다린다.
    Approval,
    /// 효과를 보류했다(운영자 판정 대기).
    Held,
    /// 직계 자식의 결과를 기다린다.
    Child,
}

/// 시도가 대기에 들어갔음을 호스트에 알리는 곳(선택). 호스트는 CLI 프로세스를 곧바로 정리할 수 있다.
pub trait Park: Send + Sync {
    fn parked(&self, attempt: &Attempt, reason: ParkReason);
}

/// 알리지 않는다(세션 회수만 한다).
pub struct NoPark;

impl Park for NoPark {
    fn parked(&self, _: &Attempt, _: ParkReason) {}
}
