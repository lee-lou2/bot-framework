//! 게이트웨이가 인증한 주체의 접수 조회, 민감 입력 전달, 회신 원장.
use crate::{
    command::{HttpRoute, JsonPayload},
    id::*,
    port::Reply,
    state::*,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestKey {
    pub profile: ProfileId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub request: RequestId,
}

pub struct RequestState {
    pub run: RunId,
    pub state: RunState,
    pub result: Option<JsonPayload>,
    /// 최초 접수 때 저장한 HTTP 경로와 응답 방식. 이전 HTTP 행은 None이며 accepted로 조회한다.
    pub http: Option<HttpRoute>,
}

/// body/args를 로그·일반 큐·승인 카드·모델에 보관하지 않는다.
pub struct PrivateCall {
    pub key: RequestKey,
    pub revision: Revision,
    pub conversation: Conversation,
    pub agent: AgentId,
    pub tool: ToolId,
    pub args: JsonPayload,
}

pub trait PrivateIntake: Send + Sync {
    /// 동일 접수는 한 작업만 실행한다. 재점유에 원문이 필요한 동안에는 재전달 신호를 반환한다.
    fn receive(&self, call: PrivateCall) -> Reply<'_, ()>;
}

/// worker의 리스와 독립적으로 잡은 회신 예약. call은 재시도에도 고정된다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryKey {
    pub profile: ProfileId,
    pub run: RunId,
    pub seq: u32,
    pub generation: Generation,
}

pub struct Outgoing {
    pub key: DeliveryKey,
    pub call: CallId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub destination: Conversation,
    pub body: JsonPayload,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotSentReason {
    /// 이번 요청의 바이트를 쓰기 전에 연결이 실패했다.
    BeforeWrite,
    /// 인증된 수신처가 요청을 실행하기 전에 속도 제한으로 거절했다.
    RateLimited,
}

pub enum DeliveryOutcome {
    Confirmed {
        receipt: String,
    },
    /// 미실행이 확정된 일시 실패. 수신처가 준 대기 시간은 재시도 하한이다.
    NotSent {
        reason: NotSentReason,
        retry_after: Option<std::time::Duration>,
    },
    Rejected,
    Held,
}

pub trait Outbox: Send + Sync {
    /// 발송 예약을 리스로 점유한다. sending 불명 항목은 자동 claim 대상에서 제외한다.
    fn claim(&self, kinds: &[ConversationKind], limit: u32) -> Reply<'_, Vec<Outgoing>>;
    /// 현재 리스·현 게시 자격 확인과 sending 기록을 원자적으로 처리한다.
    /// 최초 전송자만 Ok를 받는다. 실패하거나 응답을 잃으면 전송할 수 없다.
    fn begin(&self, key: DeliveryKey, checked_revision: Revision) -> Reply<'_, ()>;
    /// 발송 세대를 확인하고 결과·영수증·감사를 함께 기록한다.
    fn settle(&self, key: DeliveryKey, outcome: DeliveryOutcome) -> Reply<'_, ()>;
}
