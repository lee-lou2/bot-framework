//! 슈퍼 관리자의 런타임 스킬·MCP 관리 포트. 구현은 호스트 설정과 프로필 폴더를 쥔 `bot-cli`가 제공한다.
use crate::{
    command::JsonPayload,
    id::{AgentId, PrincipalId, ProfileId, Revision},
    port::Reply,
};

/// 브로커가 슈퍼 관리자·스코프·승인 검사를 마친 뒤 실행기에 넘기는 관리 호출 요청.
#[derive(Clone, Debug)]
pub struct ManageCall {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub principal: PrincipalId,
    pub tool: String,
    /// 정규 JSON 인자. 비밀 값은 로그·감사·승인 카드에 적지 않는다.
    pub args: JsonPayload,
}

/// 관리 호출 성공 결과. 새 프로필 집합을 즉시 활성화했다면 새 프로필 판본을 함께 돌려준다.
#[derive(Clone, Debug)]
pub struct ManageOutcome {
    pub payload: JsonPayload,
    pub activated: Option<Revision>,
}

/// 고정 어휘 실패 사유. `code`는 감사에 남는 고정 코드이고 자유 문장·비밀을 싣지 않는다.
#[derive(Clone, Debug)]
pub enum ManageError {
    /// 인자·대상·검증이 규칙에 어긋났다. 모델에게 안전한 요약과 고정 코드를 돌려준다.
    Rejected { code: &'static str, message: String },
    /// 동시 변경 충돌.
    Conflict,
    /// 저장소·호스트·네트워크의 일시적 실패.
    Unavailable(&'static str),
}

pub trait ProfileManager: Send + Sync {
    fn execute(&self, call: ManageCall) -> Reply<'_, Result<ManageOutcome, ManageError>>;
}
