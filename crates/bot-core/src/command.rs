use crate::{id::*, state::*};
use std::{
    collections::BTreeSet,
    time::{Duration, SystemTime},
};

/// 경계에서 스키마 검증을 마친 UTF-8 JSON 문서. 인코더 선택은 어댑터 소유다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonPayload(pub String);

/// HTTP 트리거가 대조한 대문자 메서드와 입구 아래 경로. 경로는 /로 시작하고 쿼리를 제외한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRoute {
    pub method: String,
    pub path: String,
    /// 최초 라우팅에서 정한 응답 방식. 중복 접수 해시에서 제외한다.
    pub response: crate::profile::ResponseMode,
}

/// 어댑터 인증 후 만든 봉투. 민감 DM은 일반 접수 전에 지정 도구로 보낸다.
#[derive(Clone, Debug)]
pub struct Envelope {
    pub profile: ProfileId,
    pub request: RequestId,
    pub binding: BindingId,
    pub principal: PrincipalId,
    pub conversation: Conversation,
    /// 인증된 HTTP 어댑터에서만 Some이다. 경로와 메서드는 접수 중복 판정에 함께 쓴다.
    pub http: Option<HttpRoute>,
    pub attachments: Vec<String>,
    pub body: JsonPayload,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Agent {
        agent: AgentId,
        instruction: String,
    },
    Tool {
        agent: AgentId,
        tool: ToolId,
        args: JsonPayload,
    },
}

/// scopes는 접수 시점 요청자 권한의 스냅샷이다. 브로커는 호출마다 현재 정책과 교집합을 취한다.
#[derive(Clone, Debug)]
pub struct Accept {
    pub event: Envelope,
    pub approver: Option<PrincipalId>,
    pub revision: Revision,
    pub target: Target,
    pub scopes: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub attempt: Attempt,
    pub call: CallId,
    pub tool: ToolId,
    pub args: JsonPayload,
}

#[derive(Clone, Debug)]
pub enum CallResult {
    Done(JsonPayload),
    Awaiting(ApprovalId),
    Held(CallId),
}

#[derive(Clone, Debug)]
pub enum MemoryChange {
    Create { text: String },
    Edit { id: MemoryId, text: String },
    Delete { id: MemoryId },
}

#[derive(Clone, Debug)]
pub struct Occurrence {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    pub scheduled_at: std::time::SystemTime,
}

/// scopes는 런타임이 프로필 정책으로 좁힌 값이다. 저장소는 부모 범위의 부분집합만 받는다.
#[derive(Clone, Debug)]
pub struct Delegate {
    /// 위임을 인가한 활성 프로필 판본. 부모 실행의 고정 판본과 별도로 검사한다.
    pub checked_revision: Revision,
    pub parent: Attempt,
    pub step: String,
    pub child: AgentId,
    pub instruction: String,
    pub scopes: BTreeSet<String>,
}

impl Delegate {
    /// MCP 호출과 세션 재개가 같은 두 필드에서 위임 대상과 지시를 읽는다.
    pub fn target(input: &serde_json::Value) -> Option<(AgentId, String)> {
        let object = input.as_object()?;
        let child = object.get("agent")?.as_str()?;
        let instruction = object.get("instruction")?.as_str()?.trim();
        (object.len() == 2 && !child.is_empty() && !instruction.is_empty())
            .then(|| (AgentId(child.to_owned()), instruction.to_owned()))
    }

    pub fn parse_target(args: &JsonPayload) -> Option<(AgentId, String)> {
        Self::target(&crate::canon::parse(&args.0)?)
    }
}

#[derive(Clone, Debug)]
pub struct EffectRequest {
    pub attempt: Attempt,
    pub call: CallId,
    pub step: String,
    pub args: JsonPayload,
}

#[derive(Clone, Debug)]
pub struct EffectStatus {
    pub effect: EffectId,
    pub state: EffectState,
    /// Held에서 소비 전인 운영자 판정만 반환한다. NotExecuted는 같은 단계로 다시 점유를 요청할 수 있다.
    /// 조회 자체는 전송을 허가하지 않으며, 도구의 결과 보고에는 None을 쓴다.
    pub resolution: Option<EffectResolution>,
    pub receipt: Option<String>,
}

// ---- 실행(큐) ----

/// 재개용 체크포인트 한 칸. key는 run 안에서 유일하다. 같은 key의 재기록은 같은 내용일 때만 받는다.
/// 자식 결과도 위임 단계 key로 여기에 들어온다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub key: String,
    pub body: JsonPayload,
}

/// claim이 돌려주는 실행 내용. 접수 때 고정한 값과 체크포인트다.
#[derive(Clone, Debug)]
pub struct Work {
    pub revision: Revision,
    pub target: Target,
    pub input: JsonPayload,
    pub attachments: Vec<String>,
    pub steps: Vec<Step>,
}

/// 결과와 원래 대화로 보낼 답을 한 커밋에 기록한다. 답 순번은 목록 위치다.
#[derive(Clone, Debug)]
pub struct Finish {
    pub result: JsonPayload,
    pub answers: Vec<JsonPayload>,
}

/// 공급자가 보고한 토큰 수. 입력(`prompt`)은 공급자가 정의한 캐시 입력 토큰을 한 번 포함한다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenUsage {
    pub prompt: u64,
    pub completion: u64,
}

/// 성공 완료와 같은 커밋에 저장하는 실행 계량. 기본은 «잘리지 않음·사용량 모름»이다.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RunOutcome {
    /// 최종 글이 호스트 상한(최대 출력 토큰)으로 실제로 잘렸다.
    pub truncated: bool,
    /// 실제 전체 실행의 신뢰할 수 있는 계수가 모두 있을 때만 Some. 재개·위임·미측정으로 합계를 보장할 수 없으면 None이다(0으로 채우지 않는다).
    pub usage: Option<TokenUsage>,
}

/// 사유는 고정 어휘 코드만 쓴다(로그·감사에 자유 문장을 남기지 않는다).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Failure {
    Transient(&'static str),
    Permanent(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Retry {
    After(Duration),
    Dead,
}

/// heartbeat 결과. 취소 요청은 다음 외부 효과 전에 확인한다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Beat {
    pub cancel_requested: bool,
}

/// 같은 대화의 이전 차례. from이 None이면 에이전트의 답이다.
#[derive(Clone, Debug)]
pub struct Turn {
    pub from: Option<PrincipalId>,
    pub text: String,
}

// ---- 기억 ----

#[derive(Clone, Debug)]
pub struct MemoryHit {
    pub id: MemoryId,
    pub visibility: Visibility,
    pub text: String,
}

/// 승인 카드는 이 조회가 돌려준 본문 전체를 사용한다. 본문은 진단 출력에서 제외한다.
#[derive(Clone, PartialEq, Eq)]
pub struct MemoryPreview {
    pub id: MemoryId,
    pub visibility: Visibility,
    pub text: String,
    /// 정규화하거나 공백을 지우지 않은 UTF-8 본문의 SHA-256.
    pub sha256: [u8; 32],
}

/// 소유자가 본 본문과 기억 ID를 묶는다. 두 문자열 키는 RFC 8785 순서다.
pub fn memory_share_hash(id: &MemoryId, body_sha256: &[u8; 32]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let digest = sha2::digest::Output::<Sha256>::from(*body_sha256);
    let args = serde_json::json!({"id": id.0, "sha256": format!("{digest:x}")});
    Sha256::digest(args.to_string().as_bytes()).into()
}

/// profile 활성화 때 한 번 등록한다. (profile, agent, path, sha256)가 같으면 다시 만들지 않는다.
#[derive(Clone, Debug)]
pub struct MemorySeed {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub path: String,
    pub sha256: [u8; 32],
    pub owner: PrincipalId,
    pub visibility: Visibility,
    pub text: String,
}

// ---- 예약 ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerSpec {
    pub approver: Option<PrincipalId>,
    pub trigger: String,
    pub cron: String,
    pub timezone: String,
    /// 상태 확인용. 발생 생략·합치기를 허용하는 유일한 종류다.
    pub beat: bool,
    pub principal: PrincipalId,
    pub scopes: BTreeSet<String>,
    pub target: Target,
}

/// 프로필 판본의 정적 예약 정의. 활성 집합을 게시하는 트랜잭션의 DB 시각이 구간 경계다.
#[derive(Clone, Debug)]
pub struct ScheduleSet {
    pub profile: ProfileId,
    pub revision: Revision,
    pub triggers: Vec<TriggerSpec>,
}

/// (from, until] 구간에 적용되는 정의. until이 None이면 현재 구간이다.
#[derive(Clone, Debug)]
pub struct Segment {
    pub revision: Revision,
    pub from: SystemTime,
    pub until: Option<SystemTime>,
    pub spec: TriggerSpec,
}

#[derive(Clone, Debug)]
pub struct ScheduledTrigger {
    pub profile: ProfileId,
    pub agent: AgentId,
    pub trigger: String,
    /// 이 시각 이하의 발생은 접수를 마쳤다.
    pub cursor: SystemTime,
    /// cursor 이후에 걸친 구간만, 오래된 순서.
    pub segments: Vec<Segment>,
}

#[derive(Clone, Debug)]
pub struct ScheduleSnapshot {
    /// DB 시각. 스케줄러 호스트 시계는 쓰지 않는다.
    pub now: SystemTime,
    pub triggers: Vec<ScheduledTrigger>,
}

// ---- 감사 ----

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub profile: ProfileId,
    pub actor: String,
    pub action: String,
    pub target: String,
    pub args_sha256: Option<[u8; 32]>,
    pub verdict: String,
}

#[derive(Clone, Debug)]
pub struct ChainReport {
    pub entries: u64,
    pub head: [u8; 32],
    /// 체인이 처음 어긋난 순번. None이면 정상이다.
    pub first_bad: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::{MemoryId, memory_share_hash};
    use sha2::{Digest, Sha256};

    #[test]
    fn memory_share_hash_matches_the_canonical_json_vectors() {
        for (id, body, expected) in [
            (
                "m-1",
                "한 줄\n",
                "e470c9310bd403e534e248a2a3063f5ef8c074d09714d8efe073d5b1a9a01083",
            ),
            (
                "m-\"\\",
                "A\r\nB",
                "e4475fd51fb76dfd8a7fb09d20875dc08ce071d49dfd6203b7166884f5e6d5c4",
            ),
        ] {
            let hash = memory_share_hash(&MemoryId(id.into()), &Sha256::digest(body).into());
            let hex: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            assert_eq!(hex, expected);
        }
    }
}

#[cfg(test)]
mod delegation_input_tests {
    use super::{AgentId, Delegate, JsonPayload};
    use serde_json::json;

    #[test]
    fn restored_and_live_delegations_accept_the_same_target_and_instruction() {
        let input = json!({"agent": "main/research", "instruction": "  read  "});
        let wanted = Some((AgentId("main/research".into()), "read".to_owned()));
        assert_eq!(Delegate::target(&input), wanted);
        assert_eq!(
            Delegate::parse_target(&JsonPayload(input.to_string())),
            wanted
        );
        for bad in [
            json!({"agent": "main/research", "instruction": "read", "extra": true}),
            json!({"agent": "main/research", "instruction": "   "}),
            json!({"agent": 1, "instruction": "read"}),
        ] {
            assert_eq!(Delegate::target(&bad), None);
            assert_eq!(Delegate::parse_target(&JsonPayload(bad.to_string())), None);
        }
        let duplicate = r#"{"agent":"main/a","agent":"main/b","instruction":"read"}"#;
        assert_eq!(Delegate::parse_target(&JsonPayload(duplicate.into())), None);
    }
}
