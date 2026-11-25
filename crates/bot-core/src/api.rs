//! 키로 이어지는 긴 요청(OpenAI 호환 HTTP 같은 입구)이 쓰는 접수·조회 포트. 프로토콜 구조체는 없다: 게이트웨이가 검증한 값만 받는다.
//!
//! - 접수(`ApiRuns::accept`)는 키 예약·입구별 진행 중 작업 수 한도·실제 run 접수를 **한 트랜잭션**으로 처리한다. 미커밋 키만, 또는 run만 남지 않는다.
//!   같은 (프로필, 입구, 주체, 키)는 모델·바인딩이 달라도 하나다. 같은 키·같은 의미 지문은 기존 작업에 연결되고(새 실행·새 슬롯 0), 다른 지문은 `ApiAdmission::Conflict`다.
//! - 조회(`ApiRuns::lookup`)는 (프로필, 입구, 주체, 키)로 범위가 정해진다: 다른 주체·입구·프로필의 작업은 «없음»과 구별되지 않는다.
//!   접수 때 고정한 모델·바인딩·에이전트와 최초 시각은 이후 재지정·판본 교체에도 바뀌지 않는다(게이트웨이가 현재 매핑과 비교해 가린다).
//! - 최종 글·잘림·사용량은 worker가 쓴 완료 커밋(`RunStore::finish_with`)의 값이다. 이 포트를 거치지 않고 만든 run에는 없다(지어내지 않는다).

use crate::{
    command::{Accept, TokenUsage},
    id::{AgentId, BindingId, PrincipalId, ProfileId, RequestId, Revision, RunId},
    port::Reply,
};

/// 접수(또는 같은 키의 재전달) 요청.
#[derive(Clone, Debug)]
pub struct ApiAccept {
    /// 기존 접수 명령. `event.request`가 멱등 키(1..=128바이트), `event.principal`이 인증된 HTTP 주체(`http:<별칭>/<주체>`), `event.binding`이 실제 API 바인딩,
    /// `target`이 그 바인딩 에이전트의 직접 접수(`Target::Agent`)다. 대화(HTTP)·스레드는 POST마다 서버가 정한 값이라 재전달에서는 다르다(의미 지문에 넣지 않는다).
    pub accept: Accept,
    /// 호스트 입구 별칭(검증한 값). 주체(`http:<별칭>/…`)의 별칭과 같아야 한다.
    pub endpoint: String,
    /// 호출자가 본 공개 모델 이름(바인딩의 고정 이름, 소문자 slug 1..=63).
    pub model: String,
    /// 의미 지문: 모델 이름·검증된 메시지(순서·역할·내용)·실제 실행 옵션의 정규 JSON의 SHA-256. 게이트웨이가 계산한다.
    pub digest: [u8; 32],
    /// 이 입구에서 아직 끝나지 않은 작업 수의 한도(1..=256). 승인·자식·보류로 기다리는 작업도 센다. 위임 자식은 루트와 한 작업이다.
    pub max_active: u32,
    /// 접수 때 호스트가 확인한 사람 승인 전달 설치의 Slack `team_id`(승인 경로가 Slack일 때만). 원래 요청에 묶여 이후 카드가 같은 팀의 설치로만 간다
    /// (같은 워크스페이스 별칭의 실제 팀이 바뀐 재배포 뒤 옛 요청의 카드는 새 팀에 올리지 않는다). 로컬 경로·승인 경로 없음이면 None이다.
    pub team: Option<String>,
}

/// 접수 결과. `Err(Error::Unavailable)`은 «확실히 커밋 전»일 때만이다(그때만 503이 가능하다). 커밋 응답을 잃어 확정할 수 없으면 `Unknown`이고,
/// 호출자는 같은 키로 `lookup`해 확인한다. 키 없는 요청은 새 키를 모르는 채 응답을 잃을 수 있으므로 `Unknown`을 503으로 돌리면 안 된다.
#[derive(Clone, Debug)]
pub enum ApiAdmission {
    /// 새로 접수했다(키 1건·run 1건·감사 1건이 한 커밋이다).
    Accepted(ApiRun),
    /// 같은 키·같은 지문이라 기존 작업에 연결했다. 새 실행도 새 슬롯도 쓰지 않는다(한도가 차 있어도 된다).
    Replayed(ApiRun),
    /// 같은 키인데 다른 지문이다(모델·메시지·실행 옵션이 다르다).
    Conflict,
    /// 새 작업인데 한도가 찼다. 아무것도 기록하지 않았다.
    Capacity,
    /// 활성 판본(또는 호스트 활성 집합)이 어긋났다. 아무것도 기록하지 않았다.
    Stale,
    /// 커밋했는지 알 수 없다.
    Unknown,
}

/// 조회 범위.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKey {
    pub profile: ProfileId,
    pub endpoint: String,
    pub principal: PrincipalId,
    pub key: RequestId,
}

/// 사람 승인 경로 때문에 막힌 이유(닫힌 집합). 호스트가 «도구가 실행되지 않았다»를 고정 안내로 알리는 근거이며 모델이 성공으로 요약할 수 없게 한다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalBlock {
    /// 고정 판본에 승인 경로가 없다.
    NoApprover,
    /// 승인 지정이나 사람 멤버가 철회됐다.
    ApproverRevoked,
    /// 승인자·전달 경로·바인딩이 재지정됐다.
    RouteChanged,
    /// Slack DM 전달이 실패했거나 게시 여부를 확정할 수 없어 작업을 닫았다.
    ApprovalDmUnavailable,
}

impl ApprovalBlock {
    pub const ALL: [ApprovalBlock; 4] = [
        Self::NoApprover,
        Self::ApproverRevoked,
        Self::RouteChanged,
        Self::ApprovalDmUnavailable,
    ];

    /// 저장·감사에 쓰는 고정 낱말.
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoApprover => "no_approver",
            Self::ApproverRevoked => "approver_revoked",
            Self::RouteChanged => "route_changed",
            Self::ApprovalDmUnavailable => crate::cards::APPROVAL_DM_UNAVAILABLE,
        }
    }

    pub fn parse(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|b| b.code() == code)
    }
}

/// 접수한 작업의 권위 있는 상태.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApiState {
    /// 진행 중: 준비·실행·재시도·자식 대기. 사람이 할 일은 없다.
    Active,
    /// 사람의 승인을 기다린다(실제 승인 대기일 때만. 자식 대기·보류는 아니다).
    NeedsHuman,
    /// 운영자 판정을 기다리는 보류.
    Held,
    /// 성공: worker가 저장한 최종 글. 원시 결과 JSON·도구 결과·첨부는 담지 않는다.
    Final {
        text: String,
        /// 최종 글이 호스트 상한으로 실제로 잘렸다(`length`).
        truncated: bool,
        /// 실제 전체 실행의 신뢰할 수 있는 계수가 모두 있을 때만. 모르면 None(0으로 채우지 않는다).
        usage: Option<TokenUsage>,
    },
    /// 사람 승인을 받을 수 없어 도구가 실행되지 않았다는 저장된 판정이 루트 또는 위임 자식에 있다(아직 끝나지 않았거나 끝난 run 모두).
    /// 진행 중인 승인 대기(`NeedsHuman`)와 실패·취소는 이 값으로 덮지 않는다.
    ApprovalBlocked(ApprovalBlock),
    /// 실패한 run의 고정 어휘 사유 코드.
    Failed {
        reason: String,
    },
    Cancelled,
}

/// 접수한 작업. 모델·바인딩·에이전트·판본·최초 시각·지문은 접수 때 고정한 값이라 이후 재지정·판본 교체에도 바뀌지 않는다.
#[derive(Clone, Debug)]
pub struct ApiRun {
    pub run: RunId,
    /// 최초 접수 시각(DB 시계)의 Unix 초.
    pub created: u64,
    pub model: String,
    pub binding: BindingId,
    pub agent: AgentId,
    pub revision: Revision,
    /// 접수 때 저장한 의미 지문(커밋을 확정하지 못한 접수의 뒤처리가 자기 지문과 대조한다).
    pub digest: [u8; 32],
    pub state: ApiState,
}

/// 루트가 인증된 OpenAI 호환 입구로 접수됐음을 접수 원장이 보증한 출처. 레거시 webhook이나 본문의 필드는 증거가 아니다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiOrigin {
    pub endpoint: String,
    pub model: String,
}

pub trait ApiRuns: Send + Sync {
    /// 원자 accept-or-replay.
    fn accept(&self, request: ApiAccept) -> Reply<'_, ApiAdmission>;
    /// 범위가 정해진 조회. 없거나 다른 주체·입구·프로필의 것이면 `None`.
    fn lookup(&self, key: ApiKey) -> Reply<'_, Option<ApiRun>>;
}

/// 외부 클라이언트가 보낸 대화 한 턴의 역할. 원래 라벨을 보존하며 어느 것도 신뢰 입력이 아니다(system·developer·assistant도 마찬가지다).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TurnRole {
    System,
    Developer,
    User,
    Assistant,
}

impl TurnRole {
    pub const fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }

    pub fn parse(label: &str) -> Option<Self> {
        match label {
            "system" => Some(Self::System),
            "developer" => Some(Self::Developer),
            "user" => Some(Self::User),
            "assistant" => Some(Self::Assistant),
            _ => None,
        }
    }
}

/// 검증된 대화 한 턴. `name`은 데이터일 뿐 인가와 무관하다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatTurn {
    pub role: TurnRole,
    pub content: String,
    pub name: Option<String>,
}

/// 입구가 검증한 대화 전체를 접수 본문(`Envelope.body`)에 싣는 모양: `{"messages":[{"role","content","name"?}…]}`. 순서·역할·문자열을 그대로 보존한다.
/// 게이트웨이(직렬화)와 런타임(비신뢰 입력으로 풀기)이 같은 코드를 쓴다. 본문은 신뢰 경계를 정하지 않는다: 어떤 실행이 이 모양으로 읽히는지는
/// 인증된 접수 원장(`RunContext::api`)이 정한다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatBody {
    pub turns: Vec<ChatTurn>,
}

impl ChatBody {
    pub fn to_payload(&self) -> crate::command::JsonPayload {
        let messages: Vec<serde_json::Value> = self
            .turns
            .iter()
            .map(|t| {
                let mut m = serde_json::json!({"role": t.role.label(), "content": t.content});
                if let Some(name) = &t.name {
                    m["name"] = serde_json::Value::String(name.clone());
                }
                m
            })
            .collect();
        crate::command::JsonPayload(serde_json::json!({ "messages": messages }).to_string())
    }

    /// 엄격하게 읽는다: 정확히 `messages` 하나, 비어 있지 않은 배열, 각 턴은 알려진 역할과 문자열 내용(선택적 문자열 `name`)뿐, 적어도 한 user 턴.
    /// 어긋나면 `None`이다(호출자는 이 실행을 비신뢰 대화로 읽을 수 없으므로 닫힌 채 실패한다).
    pub fn from_payload(payload: &crate::command::JsonPayload) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(&payload.0).ok()?;
        let object = value.as_object().filter(|o| o.len() == 1)?;
        let messages = object
            .get("messages")?
            .as_array()
            .filter(|m| !m.is_empty())?;
        let mut turns = Vec::with_capacity(messages.len());
        for message in messages {
            let fields = message.as_object()?;
            if fields
                .keys()
                .any(|k| !matches!(k.as_str(), "role" | "content" | "name"))
            {
                return None;
            }
            turns.push(ChatTurn {
                role: TurnRole::parse(fields.get("role")?.as_str()?)?,
                content: fields.get("content")?.as_str()?.to_owned(),
                name: match fields.get("name") {
                    None => None,
                    Some(name) => Some(name.as_str()?.to_owned()),
                },
            });
        }
        let body = Self { turns };
        body.current().map(|_| body)
    }

    /// 현재 질문: 마지막 user 턴의 위치. user 턴이 없으면 None이다.
    pub fn current(&self) -> Option<usize> {
        self.turns.iter().rposition(|t| t.role == TurnRole::User)
    }
}
