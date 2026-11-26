//! 저장소와 브로커가 같은 뜻으로 쓰는 상태 어휘: run·효과·위험도·가시성의 상태 값, 대화(`Conversation`)와 출처(`RunOrigin`), 실행 맥락(`RunContext`), 승인(`Approval`).
//! 고정 어휘이므로 변종을 더하면 이 값을 가르는 모든 `match`가 컴파일 오류로 알려 준다(`ConversationKind`처럼 SQL 제약에도 있는 값은 마이그레이션도 함께 바꾼다).

use crate::{command::Work, id::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunState {
    Ready,
    Leased,
    Waiting,
    Retry,
    Succeeded,
    Dead,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectState {
    Started,
    Confirmed,
    Rejected,
    Held,
}

/// 운영자가 보류 단계의 미실행을 확인했다. 다음 점유가 성공하면 이 판정은 소비된다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectResolution {
    NotExecuted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Risk {
    Read,
    Write,
    Irreversible,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Visibility {
    Private,
    Profile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConversationKind {
    Direct,
    Channel,
    Cli,
    Http,
    Scheduled,
}

/// 인증된 루트 접수나 예약 발화가 정한 출처. 회신 대화와 별도로 저장한다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunOrigin {
    Message,
    Http,
    Schedule,
    Wake,
    Unknown,
}

impl RunOrigin {
    /// 인증한 게이트웨이나 민감 DM 입구에서만 사용한다. 예약은 생성 경로가 직접 구분한다.
    pub fn from_ingress(kind: &ConversationKind, principal: &PrincipalId) -> Self {
        use crate::identity::{IdentityKind, identity_kind};
        match (kind, identity_kind(&principal.0)) {
            (ConversationKind::Cli, Some(IdentityKind::Local))
            | (ConversationKind::Direct | ConversationKind::Channel, Some(IdentityKind::Slack)) => {
                Self::Message
            }
            (ConversationKind::Http, Some(IdentityKind::Http)) => Self::Http,
            _ => Self::Unknown,
        }
    }

    /// MCP 메타데이터와 원장이 공유하는 고정 값.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Http => "http",
            Self::Schedule => "schedule",
            Self::Wake => "wake",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_recorded(value: &str) -> Self {
        match value {
            "message" => Self::Message,
            "http" => Self::Http,
            "schedule" => Self::Schedule,
            "wake" => Self::Wake,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub kind: ConversationKind,
    pub id: String,
    pub thread: Option<String>,
}

/// 예약을 만든 프로필 안에서 트리거 ID는 소유 에이전트별로 유일하다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerRef {
    pub agent: AgentId,
    pub id: String,
}

/// 공유 범위는 원래 요청의 프로필·요청자·대화에 고정된다.
#[derive(Clone, Debug)]
pub struct RunContext {
    pub attempt: Attempt,
    pub root: RunId,
    /// 위임과 재개에도 유지되는 루트의 출처.
    pub origin: RunOrigin,
    pub principal: PrincipalId,
    pub agent: AgentId,
    pub conversation: Conversation,
    /// 접수 때 고정한 바인딩. 위임 자식은 부모의 것을 물려받는다(루트의 정확한 원래 바인딩).
    pub binding: BindingId,
    /// 루트가 인증된 OpenAI 호환 입구로 접수됐다면 그 접수 원장의 출처(위임 자식도 같다). 아니면 None이다.
    pub api: Option<crate::api::ApiOrigin>,
    pub scopes: std::collections::BTreeSet<String>,
    pub private_memory_read: bool,
    pub work: Work,
    pub approver: Option<PrincipalId>,
    /// 예약 실행과 그 자식은 루트의 출처를 공유한다. 일반 요청은 None이다.
    pub trigger: Option<TriggerRef>,
}

#[derive(Clone, Debug)]
pub struct Approval {
    pub nonce: ApprovalId,
    pub attempt: Attempt,
    pub call: CallId,
    pub requester: PrincipalId,
    pub approver: PrincipalId,
    pub risk: Risk,
    pub policy_revision: Revision,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub args_sha256: [u8; 32],
    pub expires_at: std::time::SystemTime,
}
