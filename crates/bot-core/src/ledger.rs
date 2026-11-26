//! 브로커가 판정하고 저장소가 세대 검사·전이·감사를 한 번에 커밋한다.
use crate::{command::JsonPayload, id::*, port::Reply, state::*};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalState {
    Pending,
    Approved,
    Consumed,
    Denied,
    Expired,
    Superseded,
    Revoked,
}

#[derive(Clone, Debug)]
pub enum Resolution {
    NotExecuted,
    Executed { receipt: String },
}

#[derive(Clone, Debug)]
pub enum CallPhase {
    Open {
        approval: Option<ApprovalId>,
    },
    Started {
        generation: Generation,
        owner: String,
        /// 이 호출에 수신처가 확정한 단계가 있다. 최종 결과 유실을 전체 미실행으로 해석할 수 없다.
        confirmed_step: bool,
    },
    Done(JsonPayload),
    Denied,
    Held {
        reason: String,
        resolution: Option<Resolution>,
    },
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub call: CallId,
    pub tool: ToolId,
    pub args_sha256: [u8; 32],
    pub phase: CallPhase,
}

#[derive(Clone, Debug)]
pub struct StoredApproval {
    pub approval: Approval,
    pub reasons: Vec<String>,
    pub card: JsonPayload,
    pub state: ApprovalState,
    /// 소유자 DM으로 전달한 카드의 게시 영수증. 게시를 확인하기 전이거나 DM으로 전달하지 않는 카드면 `None`이다.
    pub delivery: Option<crate::cards::CardReceipt>,
}

pub struct OpenApproval {
    pub approval: Approval,
    pub reasons: Vec<String>,
    pub card: JsonPayload,
    /// OpenAI 모델 바인딩에서 접수된 실행의 일반 도구 승인 카드를 지정된 사람의 Slack DM으로 보낼 때, 그 설치를 정하는 **전달 바인딩**(같은 프로필·같은
    /// 워크스페이스의 Slack 채널 바인딩). 브로커가 고정 판본과 지금 활성 판본이 같은 경로를 가리킬 때만 싣는다. `None`이면 기존 규칙이다(채널 출처 명시
    /// 목적지 카드는 원래 바인딩의 소유자 DM, 그 밖은 같은 대화).
    pub delivery: Option<BindingId>,
}

pub struct Decision {
    pub nonce: ApprovalId,
    pub actor: PrincipalId,
    pub allow: bool,
}

pub struct StartCall {
    /// 마지막 권한 검사에 사용한 활성 프로필 전체의 판본.
    pub checked_revision: Revision,
    pub risk: Risk,
    pub tool_revision: Revision,
    pub attempt: Attempt,
    pub call: CallId,
    pub approval: Option<(Approval, Vec<String>)>,
    pub private_recall: bool,
    pub owner: String,
}

pub enum Outcome {
    Done(JsonPayload),
    Released,
    Held(String),
    Blocked,
}

pub struct FinishCall {
    pub attempt: Attempt,
    pub call: CallId,
    pub outcome: Outcome,
}

/// 감사 actor·대상·판정은 저장소가 인증된 요청과 전이로부터 작성한다.
pub trait CallLedger: Send + Sync {
    /// Actor-bound, read-only policy context while parked. Returned attempt has no live claim.
    fn approval_context(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, RunContext>;
    fn context(&self, attempt: Attempt) -> Reply<'_, RunContext>;
    /// 현재 일반 실행의 claim·세대·리스·취소를 확인하고 이 run의 기존 호출 한 건을 읽는다.
    /// 없는 호출은 None이다. 호출 생성, 승인 소비, 감사 기록은 발생하지 않는다.
    fn lookup(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CallRecord>>;
    /// 유효한 일반 claim으로 이 run의 체크포인트 한 건을 같은 DB 시점에서 읽는다.
    /// 없는 키는 None, 인계·만료·취소는 Stale, 민감 DM은 Denied다.
    fn step(&self, attempt: Attempt, key: String) -> Reply<'_, Option<JsonPayload>>;
    /// 결과 반환 시점에 유효한 일반 claim과 그 프로필의 활성 판본을 같은 DB 시점에서 읽는다.
    /// 비활성 프로필은 None, 무효 claim은 Stale, 민감 DM은 Denied다.
    fn active_revision(&self, attempt: Attempt) -> Reply<'_, Option<Revision>>;
    fn begin(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args_sha256: [u8; 32],
    ) -> Reply<'_, CallRecord>;
    fn open_approval(&self, open: OpenApproval) -> Reply<'_, ()>;
    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval>;
    /// 현재 호출에 연결된 승인만 조건부로 닫는다. 감사·대기 해제와 한 커밋이다.
    /// 재호출은 새 wake를 만들지 않으며 이미 소비된 승인은 변경하지 않는다.
    fn supersede(&self, nonce: ApprovalId, actor: PrincipalId) -> Reply<'_, ()>;
    fn decide(&self, decision: Decision) -> Reply<'_, ApprovalState>;
    /// 활성 판본 교체와 직렬화한다. 판본 불일치·비활성은 승인을 소비하기 전에 Stale로 반환한다.
    /// 세대·취소·private 플래그·승인 대조와 시작 기록은 같은 커밋이다.
    /// memory.share는 여기서 거절한다. 승인과 공유를 함께 커밋하는 MemoryStore::share를 사용한다.
    fn start(&self, start: StartCall) -> Reply<'_, ()>;
    /// `start`와 같되 사전 승인이나 지정한 사람의 승인 증거를 같은 커밋에서 확인한다. 확인에 실패하면 아무것도 쓰지 않고
    /// `Granted::Refused(고정 사유)`를 돌려준다(호출을 닫는 것은 호출자의 몫이다). 이 권한을 모르는 구현은 거절한다(기본 구현).
    /// 사전 승인이 묶인 run의 직접 호출은 `start`로 시작할 수 없다(권한을 건너뛰지 못한다).
    fn start_with(
        &self,
        start: StartCall,
        authority: crate::grant::Authority,
    ) -> Reply<'_, crate::grant::Granted> {
        let _ = (start, authority);
        Box::pin(async { Err(crate::port::Error::Denied) })
    }
    /// 이 run의 접수 때 묶은 증거(루트 run만). 없으면 None. 읽기 전용이다. 기본 구현은 없다고 답한다.
    fn evidence(&self, attempt: Attempt) -> Reply<'_, Option<crate::grant::Evidence>> {
        let _ = attempt;
        Box::pin(async { Ok(None) })
    }
    fn finish(&self, finish: FinishCall) -> Reply<'_, ()>;
    /// 승인 경로 때문에 호출을 `Outcome::Blocked`로 닫고 그 이유(`ApprovalBlock`)를 호출 기록에 함께 남긴다. 조회(`ApiRuns::lookup`)가 이 저장된 판정으로 `ApprovalBlocked`를 알린다.
    /// 이유를 저장하지 않는 구현은 이유를 버리고 `finish`로 닫는다(기본 구현).
    fn finish_blocked(&self, finish: FinishCall, _why: crate::api::ApprovalBlock) -> Reply<'_, ()> {
        self.finish(finish)
    }
    /// 넘김: 직접 호출이 «처리하지 않음»으로 끝났다. `finish.outcome`은 `Done`(표식을 뗀 결과)이어야 한다. 호출 결론·커서 전진(`pos` → 다음 후보 또는 기본 대화)·감사를
    /// 한 커밋에 기록하고 run을 대기열로 되돌린다. 쓰기 단계·보류·승인·회신이 이미 있거나 커서가 `pos`가 아니면 아무것도 쓰지 않고 `PassNext::Refused`/`Stale`이다.
    /// 사슬을 저장하지 않는 구현은 `Unavailable`이다(기본 구현).
    ///
    /// `runnable`은 **지금**(현재 판본과 실행이 고정한 판본 둘 다에서) 요청자가 `run`을 쓸 수 있는 에이전트들이다. 마지막 후보가 넘겨 기본 대화(에이전트)로 갈 때 그 에이전트가
    /// 이 집합에 있고 접수가 고정한 스코프에도 `run`이 있어야 시작한다 — 아니면 대화 없이 run을 조용히 끝낸다(접수 뒤에 역할에서 `run`만 철회돼도 기본 대화가 시작되지 않는다).
    fn finish_pass(
        &self,
        _finish: FinishCall,
        _pos: u32,
        _runnable: std::collections::BTreeSet<AgentId>,
    ) -> Reply<'_, crate::pass::PassNext> {
        Box::pin(async { Err(crate::port::Error::Unavailable) })
    }
}

/// 민감 DM 본문은 메모리에서만 처리한다. HMAC과 접수 키만 저장한다.
pub struct PrivateRequest {
    pub key: crate::intake::RequestKey,
    /// 전용 처리용 HMAC. 구판 행도 이 형식으로 대조한다.
    pub fingerprint: [u8; 32],
    /// 선택한 선언이 넘김을 허용할 때만 있는 v2 HMAC. 판본·트리거·전체 봉투까지 묶는다.
    pub pass_fingerprint: Option<[u8; 32]>,
    pub revision: Revision,
    pub agent: AgentId,
    pub tool: ToolId,
    pub conversation: Conversation,
    pub scopes: crate::profile::Scopes,
}

/// 지정 도구가 실제로 시작되기 직전 재확인할 값. 민감 입력은 담지 않는다.
pub struct PrivateStart {
    pub attempt: Attempt,
    pub tool: ToolId,
    pub fingerprint: [u8; 32],
    pub checked_revision: Revision,
}

/// 일반 워커가 접근할 수 없는 접수 전용 원장. 호출 ID는 같은 접수의 재점유에도 고정한다.
pub trait PrivateLedger: Send + Sync {
    /// 신규 접수는 활성 판본과 대조한다. Start는 예약된 접수 리스만 부여한다.
    fn begin(&self, request: PrivateRequest) -> Reply<'_, PrivatePermit>;
    /// 리스·epoch·지정 도구·HMAC·활성 판본을 확인하고 전송 직전에 한 번만 커밋한다.
    fn start(&self, start: PrivateStart) -> Reply<'_, ()>;
    fn finish(&self, permit: Attempt, outcome: PrivateOutcome) -> Reply<'_, ()>;
}

pub enum PrivatePermit {
    Start {
        attempt: Attempt,
        /// 최초 접수에서 넘길 봉투가 결속됐고 이번 전달과 일치한다.
        allow_pass: bool,
    },
    InProgress,
    Completed,
    Passed,
    Rejected,
    Held,
}

pub enum PrivateOutcome {
    /// 시작 기록이 없을 때만 리스를 반납한다. 동일 입력 재전달로만 재점유한다.
    RetryBeforeStart,
    /// 실행기가 전송 전 실패와 이전 프로세스 종료를 확인했다.
    NotSent,
    Completed,
    Passed,
    Held,
    Rejected,
}
