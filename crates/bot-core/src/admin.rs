//! 운영 조회·판정 계약. 저장소가 구현하고 관리 API가 소비한다.
//!
//! - 모든 조회는 인증된 `Viewer`로 한정한다. 운영자는 호스트 전체, 그 밖의 주체는 자기 요청과 자기에게 지정된 승인만 본다.
//! - 조회는 읽기 전용이다. 인자·본문·비밀 원문은 싣지 않는다. 인자는 SHA-256 해시, 승인 화면 문구는 브로커가 렌더링한 `render`뿐이다.
//! - 판정(`AdminAct`)은 호출자를 `actor`로 받아 상태 변경과 감사를 한 커밋에 기록한다. 관리 API는 감사 행을 직접 쓰지 않는다.
//! - 승인 판정은 기존 `Broker::decide`를 쓴다. 이 파일은 새 포트만 둔다.

pub use crate::state::EffectResolution;
use crate::{id::*, port::Reply, state::*};
use std::time::SystemTime;

/// 인증을 마친 조회자. `operator`는 호스트 운영자(한 호스트 한 운영자)다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Viewer {
    pub principal: PrincipalId,
    pub operator: bool,
}

/// 쪽 나누기. `limit`은 호출 전에 1..=200으로 고정하고 `after`는 저장소가 정한 불투명 커서다.
#[derive(Clone, Debug, Default)]
pub struct Page {
    pub limit: u32,
    pub after: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Listing<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RunQuery {
    pub profile: Option<ProfileId>,
    pub state: Option<RunState>,
    /// 한 요청 트리(루트 run)만.
    pub root: Option<RunId>,
    pub page: Page,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ApprovalFilter {
    Pending,
    /// 대기를 벗어난 모든 상태. 자동 만료·대체도 포함한다.
    Decided,
    #[default]
    All,
}

#[derive(Clone, Debug, Default)]
pub struct ApprovalQuery {
    pub profile: Option<ProfileId>,
    pub filter: ApprovalFilter,
    pub page: Page,
}

#[derive(Clone, Debug, Default)]
pub struct AuditQuery {
    pub profile: Option<ProfileId>,
    /// 이 순번보다 앞선(작은) 행부터 최신순으로.
    pub before: Option<u64>,
    /// API는 한 쪽에 최대 200건을 표시한다. 저장소는 다음 쪽 판별용 한 건을 더해 최대 201건(limit + 1)까지 받는다.
    pub limit: u32,
}

/// 대기 사유. 승인·자식 결과·보류 중 하나를 기다린다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Waiting {
    Approval,
    Child(RunId),
    Hold(CallId),
}

#[derive(Clone, Debug)]
pub struct RunView {
    pub profile: ProfileId,
    pub run: RunId,
    pub root: RunId,
    pub parent: Option<RunId>,
    /// 위임 단계 ID(자식 run일 때).
    pub step: Option<String>,
    pub agent: AgentId,
    pub principal: PrincipalId,
    pub kind: ConversationKind,
    /// 예약·이벤트 트리거 ID.
    pub trigger: Option<String>,
    pub state: RunState,
    pub generation: Generation,
    pub attempts: u32,
    pub waiting: Option<Waiting>,
    pub private_memory_read: bool,
    /// 마지막 실패의 고정 분류 코드(오류 원문 금지).
    pub error: Option<String>,
    pub created_at: SystemTime,
    pub updated_at: SystemTime,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallState {
    Running,
    Awaiting,
    Done,
    Held,
    Denied,
    Failed,
}

/// 관리 화면·API에 노출할 수 있는 고정된 호출 실패 분류.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallError {
    Denied,
    Timeout,
    InvalidResult,
    OutcomeUnknown,
    ExecutionFailed,
}

impl CallError {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::Timeout => "timeout",
            Self::InvalidResult => "invalid_result",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::ExecutionFailed => "execution_failed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CallView {
    pub call: CallId,
    pub run: RunId,
    pub generation: Generation,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub risk: Risk,
    pub state: CallState,
    /// 확인된 실패 분류만 제공한다. 분류가 없으면 None이다.
    pub error: Option<CallError>,
    pub args_sha256: [u8; 32],
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct EffectView {
    pub effect: EffectId,
    /// 이 단계에서 현재 판정할 수 있는 보류 발생. 재보류하면 새 ID를 받는다.
    pub hold_id: Option<HoldId>,
    pub run: RunId,
    pub call: CallId,
    pub step: String,
    pub state: EffectState,
    /// 운영자가 미실행을 확인한 뒤 이 단계를 다시 시작하기 전까지 표시한다.
    pub resolution: Option<EffectResolution>,
    /// 수신처 영수증(확정된 단계). 원래 영수증은 보류 중에도 보존한다.
    pub receipt: Option<String>,
    /// 보류 사유 코드(held일 때).
    pub reason: Option<String>,
    pub args_sha256: [u8; 32],
    pub generation: Generation,
    pub at: SystemTime,
}

/// 한 run의 계보·자식·도구 호출·효과를 같은 스냅숏에서 읽은 값.
#[derive(Clone, Debug)]
pub struct RunDetail {
    pub run: RunView,
    /// 루트부터 이 run까지(자기 포함).
    pub lineage: Vec<RunView>,
    pub children: Vec<RunView>,
    pub calls: Vec<CallView>,
    pub effects: Vec<EffectView>,
}

/// 저장소가 보류를 읽은 출처. 판정 후 도구 재개·회신 재전송·민감 DM 종료를 구분한다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldKind {
    Tool,
    Reply,
    Private,
}

/// 보류된 호출. 단일 쓰기는 호출 자체가 보류라 `steps`가 비고, 다단계 도구는 모든 단계(원래 영수증 포함)를 순서대로 싣는다.
#[derive(Clone, Debug)]
pub struct HoldView {
    pub kind: HoldKind,
    /// 호출 자체를 판정할 때 사용한다. 다단계 호출은 각 단계에만 ID가 있다.
    pub hold_id: Option<HoldId>,
    pub profile: ProfileId,
    pub run: RunId,
    pub call: CallId,
    pub tool: ToolId,
    pub reason: String,
    pub since: SystemTime,
    pub steps: Vec<EffectView>,
}

pub use crate::ledger::ApprovalState;

#[derive(Clone, Debug)]
pub struct ApprovalView {
    pub nonce: ApprovalId,
    pub profile: ProfileId,
    pub run: RunId,
    pub generation: Generation,
    pub call: CallId,
    pub requester: PrincipalId,
    pub approver: PrincipalId,
    pub risk: Risk,
    pub tool: ToolId,
    pub tool_revision: Revision,
    pub policy_revision: Revision,
    pub args_sha256: [u8; 32],
    pub expires_at: SystemTime,
    pub state: ApprovalState,
    /// 브로커가 검증된 인자로 렌더링한 대상·변경 내용(라벨, 값).
    pub render: Vec<(String, String)>,
    pub decided_by: Option<PrincipalId>,
    pub decided_at: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub seq: u64,
    pub at: SystemTime,
    pub prev: [u8; 32],
    pub hash: [u8; 32],
    pub profile: Option<ProfileId>,
    pub actor: PrincipalId,
    pub action: String,
    pub target: String,
    pub args_sha256: Option<[u8; 32]>,
    pub verdict: String,
}

/// 체인을 처음부터 다시 계산한 결과. `broken_at`은 처음 어긋난 순번이다.
#[derive(Clone, Debug)]
pub struct AuditCheck {
    pub entries: u64,
    pub head: [u8; 32],
    pub broken_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct ProfileStatus {
    pub profile: ProfileId,
    pub runs: Vec<(RunState, u64)>,
    pub oldest_ready_secs: Option<u64>,
    pub approvals_pending: u64,
    pub holds: u64,
}

pub trait AdminRead: Send + Sync {
    fn runs(&self, viewer: &Viewer, query: RunQuery) -> Reply<'_, Listing<RunView>>;
    fn detail(&self, viewer: &Viewer, profile: ProfileId, run: RunId) -> Reply<'_, RunDetail>;
    /// 범위와 필터를 적용한 뒤 페이지를 나눈다. Pending은 expires_at 오름차순이다.
    /// Decided/All은 decided_at 내림차순이고 시각 없는 행은 뒤에 둔다. 그중 Pending이 마지막이다.
    /// 동률은 nonce 오름차순이다. 다음 페이지도 같은 필터와 정렬 키를 사용한다.
    fn approvals(&self, viewer: &Viewer, query: ApprovalQuery) -> Reply<'_, Listing<ApprovalView>>;
    fn approval(&self, viewer: &Viewer, nonce: ApprovalId) -> Reply<'_, ApprovalView>;
    /// 운영자 전용.
    fn holds(&self, viewer: &Viewer, profile: Option<ProfileId>) -> Reply<'_, Vec<HoldView>>;
    /// 운영자 전용.
    fn audit(&self, viewer: &Viewer, query: AuditQuery) -> Reply<'_, Vec<AuditEntry>>;
    /// 운영자 전용.
    fn verify_audit(&self, viewer: &Viewer) -> Reply<'_, AuditCheck>;
    /// 운영자 전용.
    fn status(&self, viewer: &Viewer) -> Reply<'_, Vec<ProfileStatus>>;
    /// 운영자 전용. 활성 사전 승인과 정지 상태·오늘(UTC) 사용량을 한 스냅숏으로 읽는다. 기본 구현은 지원하지 않는다.
    fn preapprovals(&self, viewer: &Viewer) -> Reply<'_, Vec<crate::grant::PreapprovalRow>> {
        let _ = viewer;
        Box::pin(async { Err(crate::port::Error::Unavailable) })
    }
}

/// 보류 대상. 단일 쓰기는 `effect`가 없고 호출 전체를 가리킨다.
#[derive(Clone, Debug)]
pub struct HoldRef {
    pub hold_id: HoldId,
    pub profile: ProfileId,
    pub run: RunId,
    pub call: CallId,
    pub effect: Option<EffectId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HoldVerdict {
    /// 수신처 영수증으로 이미 실행됐음을 확인했다.
    Confirmed { receipt: String },
    /// 옛 도구 종료와 수신처의 미실행 증거를 확인했다. 현재 권한·승인으로 같은 호출 ID를 재개할 수 있다.
    NotExecuted { evidence: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HoldDecision {
    Applied,
    Repeated,
}

pub trait AdminAct: Send + Sync {
    /// 발생 ID와 대상을 함께 대조하고 판정·감사를 한 번만 커밋한다. 같은 운영자가
    /// 같은 판정과 증거로 재요청하면 새 보류가 있어도 Repeated다. 불일치는 Conflict다.
    fn resolve_hold(
        &self,
        actor: PrincipalId,
        hold: HoldRef,
        verdict: HoldVerdict,
    ) -> Reply<'_, HoldDecision>;

    /// 트리거의 사전 승인을 정지(`pause`)하거나 재개한다. `cas`의 승인 ID·정지 세대를 현재와 대조해 상태 변경과 감사를 한 커밋에 쓴다.
    /// 이미 그 상태인 같은 세대의 요청과 직전 성공 명령의 재전달은 `repeated`다(변경·감사 0). 승인이 교체됐거나 세대가 어긋나면 `Stale`,
    /// 대상이 없으면 `Missing`. 기본 구현은 지원하지 않는다.
    fn set_pause(
        &self,
        actor: PrincipalId,
        key: crate::grant::TriggerKey,
        cas: crate::grant::GrantCas,
        pause: bool,
    ) -> Reply<'_, crate::grant::PauseOutcome> {
        let _ = (actor, key, cas, pause);
        Box::pin(async { Err(crate::port::Error::Unavailable) })
    }
}
