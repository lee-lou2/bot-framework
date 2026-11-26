//! 검증된 프로필의 읽기 전용 계약. 호스트 자격 값은 포함하지 않는다.
use crate::{
    command::JsonPayload,
    id::*,
    state::{Risk, Visibility},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub type Scopes = BTreeSet<String>;

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub id: ProfileId,
    pub revision: Revision,
    pub policy_revision: Revision,
    pub timezone: String,
    pub model: String,
    pub root: AgentId,
    pub agents: BTreeMap<AgentId, Agent>,
    pub bindings: BTreeMap<BindingId, Binding>,
    pub roles: BTreeMap<String, Scopes>,
    pub members: BTreeMap<PrincipalId, BTreeSet<String>>,
    pub policy: Policy,
    pub secrets: Vec<SecretName>,
}

#[derive(Clone, Debug)]
pub struct Policy {
    pub scopes: Scopes,
    pub agents: BTreeMap<AgentId, Scopes>,
    pub approve: Scopes,
}

/// 모델 실행기가 제공할 수 있는 읽기 전용 내장 도구.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Builtin {
    Read,
    Grep,
    Glob,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub description: String,
    /// 이 AGENT.md가 명시한 집합. 부모에게서 상속하지 않으며 기본은 비어 있다.
    pub builtin: BTreeSet<Builtin>,
    pub parent: Option<AgentId>,
    /// 모델·지침·환경·스킬·지식·스코프는 조상 상속까지 적용한 값이다.
    pub model: String,
    pub instructions: String,
    pub env: BTreeMap<String, String>,
    pub scopes: Scopes,
    pub skills: BTreeMap<String, Skill>,
    pub knowledge: Vec<Document>,
    /// 시작 시 DB로 옮길 항목. 모델 지침이나 지식 검색에 섞지 않는다.
    pub memory_seeds: Vec<MemorySeed>,
    /// 트리거는 선언한 에이전트만 소유한다.
    pub triggers: Vec<Trigger>,
}

#[derive(Clone, Debug)]
pub struct MemorySeed {
    pub document: Document,
    pub owner: PrincipalId,
    pub visibility: Visibility,
}

#[derive(Clone, Debug)]
pub struct Document {
    pub relative_path: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub instructions: Document,
    /// 키는 전체 skill.tool 이름. 서로 다른 에이전트의 가시 범위는 독립적이다.
    pub tools: BTreeMap<ToolId, Tool>,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub server: String,
    pub revision: Revision,
    pub class: Risk,
    pub scopes: Scopes,
    pub input: JsonPayload,
    /// 입력과 함께 `tools.lock.yaml`에서 가져온 설명. 정의 해시에 포함된다.
    pub description: Option<String>,
    /// Names selected from the calling agent’s inherited non-secret configuration.
    pub env: BTreeSet<String>,
    pub secrets: BTreeMap<String, SecretName>,
    pub egress: BTreeSet<String>,
    pub effects: bool,
    pub external: bool,
    pub destination: Option<Destination>,
    /// MCP 도구 정의(이름·설명·입력 스키마)의 고정 해시(`mcp::definition_sha256`, 소문자 16진). 입력 스키마를 `tools.lock.yaml`에서 가져온 도구만 있다.
    /// 실행할 때 서버가 내세우는 정의가 이 해시와 다르면 호출하기 전에 막는다. 도구 판본(`revision`)에 이미 들어 있어 바뀌면 승인이 무효가 된다.
    pub definition: Option<String>,
    /// 서버가 내세우는 도구 이름(tools.yaml `name`, MCP 이름 규칙). 생략하면 도구 ID의 도구 부분이다(`skill.tool`의 `tool`). 도구 판본에 들어 있다.
    pub mcp_name: Option<String>,
    /// 이 도구를 한 번 부를 때 브로커가 결과를 기다리는 시간(초, 1~`MAX_TOOL_CALL_SECS`). 생략하면 `DEFAULT_TOOL_CALL_SECS`다. 도구 판본에 들어 있어 바뀌면 승인이 무효가 된다.
    pub timeout_secs: Option<u32>,
}

/// 도구 호출 시간 한도(초)의 기본값: `timeout_secs`를 선언하지 않은 도구, 그리고 호스트 천장(`limits.tool_call_max_secs`)을 정하지 않은 호스트의 값이다.
pub const DEFAULT_TOOL_CALL_SECS: u32 = 60;
/// 도구가 선언하거나 호스트 천장으로 정할 수 있는 호출 시간 한도(초)의 상한.
pub const MAX_TOOL_CALL_SECS: u32 = 900;

impl Tool {
    /// 도구 목록에는 잠금의 설명을 우선 쓰고, 없으면 입력 스키마의 최상위 설명을 쓴다.
    pub fn model_description(&self) -> Option<String> {
        self.description.clone().or_else(|| {
            let input: serde_json::Value = serde_json::from_str(&self.input.0).ok()?;
            input.get("description")?.as_str().map(str::to_owned)
        })
    }

    /// 실제로 적용하는 호출 시간 한도(초): 선언이 없으면 `DEFAULT_TOOL_CALL_SECS`.
    pub fn call_secs(&self) -> u32 {
        self.timeout_secs.unwrap_or(DEFAULT_TOOL_CALL_SECS)
    }
}

#[derive(Clone, Debug)]
pub enum Destination {
    Reply,
    Explicit { recipient: String, content: String },
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub agent: AgentId,
    pub source: Adapter,
}

#[derive(Clone, Debug)]
pub enum Adapter {
    Cli {
        account: String,
    },
    SlackChannel {
        workspace: String,
        channel: String,
        /// 이 채널 바인딩이 받는 이벤트 종류(생략하면 멘션만). 비어 있지 않다.
        events: std::collections::BTreeSet<crate::slack::SlackEvent>,
    },
    SlackDm {
        workspace: String,
    },
    /// 기존 웹훅 입구(호스트 `kind: webhook`). 모양은 OpenAI 변형이 생기기 전과 같다.
    Http {
        endpoint: String,
    },
    /// OpenAI 호환 모델 바인딩: 호스트 별칭 `endpoint`(`kind: openai`)가 호출자에게 보이는 공개 모델 이름 `model` 하나.
    /// 모델을 고르면 이 바인딩의 에이전트가 요청을 직접 받는다(트리거는 이 바인딩을 가리킬 수 없다).
    /// `approvers`: HTTP 주체(`http:<endpoint>/<sub>`) → 일반 도구의 인간 승인 경로. 없는 주체의 승인 필요 도구는 실행하지 않는다.
    OpenAi {
        endpoint: String,
        model: String,
        approvers: BTreeMap<PrincipalId, ApprovalRoute>,
    },
}

/// 프로필이 OpenAI 모델 바인딩의 HTTP 주체마다 정한 인간 승인 경로. HTTP 키·메시지는 승인할 수 없고, 승인은 지정한 사람만 한다.
/// 승인자 지정은 HTTP 주체의 권한을 올리지 않는다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApprovalRoute {
    /// 본인 인증된 관리 화면에서 승인한다. `person`은 프로필 멤버인 `local:*` 신원이다.
    Local { person: PrincipalId },
    /// 같은 프로필·같은 Slack 설치(워크스페이스)의 채널 바인딩 `binding`으로 설치를 확인하고, 카드는 `person`의 DM 하나로 보낸다.
    /// `person`은 프로필 멤버인 `slack:<워크스페이스>/<사용자>` 신원이다.
    Slack {
        person: PrincipalId,
        binding: BindingId,
    },
}

impl ApprovalRoute {
    /// 승인할 사람(프로필 멤버)
    pub fn person(&self) -> &PrincipalId {
        match self {
            Self::Local { person } | Self::Slack { person, .. } => person,
        }
    }
}

impl Snapshot {
    /// OpenAI 모델 바인딩 `binding`을 통해 인증된 HTTP 주체 `requester`의 일반 도구 승인 경로. 한 스냅숏 안에서만 정한다.
    /// 다음이 모두 맞을 때만 돌려준다 — 바인딩이 OpenAI 모델이고, 요청자가 이 프로필의 HTTP 멤버이고, 매핑이 있고,
    /// 승인할 사람이 이 프로필의 인간 멤버이고, Slack이면 전달 바인딩이 같은 프로필의 같은 워크스페이스 채널 바인딩이다.
    /// 어긋나면 None(승인 경로 없음)이라 미승인 도구는 실행하지 않는다. 호출하는 쪽은 고정한 판본과 현재 판본 모두에서 같은 값인지 대조한다.
    pub fn approval_route(
        &self,
        binding: &BindingId,
        requester: &PrincipalId,
    ) -> Option<ApprovalRoute> {
        let Adapter::OpenAi {
            endpoint,
            approvers,
            ..
        } = &self.bindings.get(binding)?.source
        else {
            return None;
        };
        let from_endpoint = requester
            .0
            .strip_prefix("http:")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(alias, _)| alias == endpoint);
        if !from_endpoint || !self.members.contains_key(requester) {
            return None;
        }
        let route = approvers.get(requester)?;
        let person = route.person();
        if !person.is_human() || !self.members.contains_key(person) {
            return None;
        }
        match route {
            ApprovalRoute::Local { .. } => person.0.starts_with("local:").then(|| route.clone()),
            ApprovalRoute::Slack {
                binding: delivery, ..
            } => {
                let workspace = person
                    .0
                    .strip_prefix("slack:")?
                    .split_once('/')
                    .map(|(workspace, _)| workspace)?;
                match &self.bindings.get(delivery)?.source {
                    Adapter::SlackChannel { workspace: w, .. } if w == workspace => {
                        Some(route.clone())
                    }
                    _ => None,
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub id: String,
    pub on: TriggerSource,
    pub order: i64,
    /// 봉투 경로별 리터럴 JSON 값.
    pub conditions: BTreeMap<String, JsonPayload>,
    /// 인자 이름 → 봉투의 점 경로.
    pub input: BTreeMap<String, String>,
    pub private: bool,
    pub private_pass: bool,
    pub target: TriggerTarget,
    /// 운영자가 활성화 때 사전 승인한 정적 호출(cron 고정 도구 트리거만).
    pub preapproved: Option<crate::grant::Preapproval>,
    /// 지정한 사람의 메시지·반응을 승인으로 쓰는 직접 도구 트리거.
    pub human_approval: Option<crate::grant::HumanApproval>,
}

#[derive(Clone, Debug)]
pub enum TriggerSource {
    Cron {
        schedule: String,
        beat: bool,
        principal: PrincipalId,
        approver: Option<PrincipalId>,
    },
    Event {
        binding: BindingId,
        event: String,
    },
    Http {
        binding: BindingId,
        method: String,
        path: String,
        response: ResponseMode,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseMode {
    Accepted,
    Sync,
}

#[derive(Clone, Debug)]
pub enum TriggerTarget {
    Instruction(String),
    Tool { tool: ToolId, args: JsonPayload },
}

/// active는 한 번에 교체된 프로필 집합이다. 이전 판본을 읽어도 현재 권한 검사는 유지한다.
pub trait ProfileCatalog: Send + Sync {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>>;
    fn current(&self, profile: &ProfileId) -> Option<Arc<Snapshot>> {
        self.active().get(profile).cloned()
    }
    /// 실행 또는 예약 구간이 고정한 정확한 판본을 반환한다. 복원된 판본도 이 메서드로 조회한다.
    /// 옛 판본을 복원해도 active/current는 게시된 집합 그대로다. 호스트는 필요한 판본을 역할 시작 전에 확인한다.
    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>>;
}

/// 활성 판본을 저장소에 게시한다. expected가 달라지면 전체 교체를 거절한다.
/// 다른 프로세스는 동일 집합을 확인한 뒤 서비스를 시작한다.
pub trait ActiveRevisions: Send + Sync {
    /// 마지막에 게시된 전체 집합의 판본. 첫 게시 전에는 None이다.
    fn current(&self) -> crate::port::Reply<'_, Option<Revision>>;
    /// CAS·기억 시드·정적 예약 구간·감사를 한 커밋에 게시한다. 모든 구간의 경계는 같은 DB 시각이다.
    /// schedules에 없는 프로필은 정적 예약 0개다. profiles 밖의 키는 Invalid다. 사용자 예약은 독립적이다.
    fn replace(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
    ) -> crate::port::Reply<'_, ()>;

    /// `replace`와 같되, 같은 트랜잭션에서 활성 행 잠금 아래 `profiles`의 ID들이 고정한 판본(`ProfileBundles::pinned_revisions`의 정의)을 다시 읽어
    /// 모두 `prepared`(호출한 쪽이 번들을 검증·보관하고 복원해 둔 집합)에 있는지 대조한다. 하나라도 없으면 — 준비 뒤 게시 전에 옛 프로세스가 새 고정 참조를 커밋했다 —
    /// 아무것도 쓰지 않고 `Stale`이다(호출한 쪽이 목록을 다시 준비한다). 기본 구현은 대조 없이 `replace`와 같다(대조가 필요한 저장소만 덮는다).
    fn replace_prepared(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
        prepared: BTreeSet<(ProfileId, Revision)>,
    ) -> crate::port::Reply<'_, ()> {
        let _ = prepared;
        self.replace(expected, next, profiles, seeds, schedules)
    }

    /// `replace_prepared`와 같되 같은 트랜잭션에서 사전 승인을 동기화한다: 새 집합에 선언된 승인(`grants`)은 지문이 같으면 기존 ID를 유지하고
    /// 다르거나 처음이면 새 ID를 발급하며, 선언이 사라지거나 바뀐 기존 승인은 철회한다(철회된 ID는 되살아나지 않는다). `grants`가 비어 있으면
    /// 모든 승인을 철회한다. 돌려주는 값은 이번에 발급한 승인(운영자에게 범위를 보여 준다). 기본 구현은 승인이 없을 때만 `replace_prepared`와 같다.
    fn replace_granted(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        seeds: Vec<crate::command::MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<crate::command::TriggerSpec>>,
        plan: crate::grant::GrantPlan,
    ) -> crate::port::Reply<'_, Vec<(crate::grant::GrantId, crate::grant::GrantSpec)>> {
        if !plan.grants.is_empty() {
            return Box::pin(async { Err(crate::port::Error::Unavailable) });
        }
        let done = self.replace_prepared(expected, next, profiles, seeds, schedules, plan.prepared);
        Box::pin(async move { done.await.map(|()| Vec::new()) })
    }
}

/// 프로필 판본의 검증한 바이트 묶음을 저장소에 불변으로 보관한다. 재배포 뒤 새 프로세스가 옛 판본을 복원하는 근거다.
/// 활성 집합을 게시하기 전에 그 집합의 묶음을 먼저 보관하고, 보관한 묶음은 지우거나 바꾸지 않는다.
pub trait ProfileBundles: Send + Sync {
    /// `(profile, revision)`으로 보관한다. `revision`은 묶음의 SHA-256 16진수여야 한다(아니면 `Invalid`).
    /// 같은 키로 같은 바이트를 다시 보관해도 된다. 이미 있는 바이트가 다르면 `Conflict`다(저장소가 손상됐을 수 있다 — 교체를 거절한다).
    fn put_bundle(
        &self,
        profile: ProfileId,
        revision: Revision,
        bundle: Vec<u8>,
    ) -> crate::port::Reply<'_, ()>;
    /// 보관한 묶음. 없으면 None.
    fn bundle(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> crate::port::Reply<'_, Option<Vec<u8>>>;
    /// 이 묶음을 처음 보관한 시각(Unix 밀리초). 보관한 적이 없으면 None이다. `/v1/models`의 `created`처럼 요청마다 바뀌면 안 되는 값에 쓴다.
    /// 기본 구현은 모른다고 답한다(기록을 가진 저장소가 덮는다). 값을 지어내지 않는다.
    fn created_ms(
        &self,
        profile: &ProfileId,
        revision: &Revision,
    ) -> crate::port::Reply<'_, Option<i64>> {
        let _ = (profile, revision);
        Box::pin(async { Ok(None) })
    }
    /// 이번에 게시·합류할 집합(`target`: 프로필 → 판본)의 프로필 ID들이 고정한 (프로필, 판본) 전체를 한 스냅숏에서 읽는다 — `target`의 현재 쌍과,
    /// 그 ID들의 살아 있는 참조: 끝나지 않은 실행, 재개할 수 있는 dead letter, 대기·승인 미소비 카드의 실행, 정적 cron의 열린 구간과 커서가 끝에 이르지 않은 닫힌 구간,
    /// 활성 사용자 예약의 생성 실행. 집합에서 빠진 프로필의 참조는 읽지 않고, 아무 작업도 참조하지 않는 옛 판본은 «지금 DB의 활성 판본»이라는 이유만으로 넣지 않는다.
    fn pinned_revisions<'a>(
        &'a self,
        target: &'a BTreeMap<ProfileId, Revision>,
    ) -> crate::port::Reply<'a, BTreeSet<(ProfileId, Revision)>>;
}

/// 선언 검증과 실제 도구 호출에서 동일한 스키마 구현을 사용한다.
pub trait InputValidator: Send + Sync {
    fn validate(&self, schema: &JsonPayload, args: &JsonPayload) -> Result<(), crate::port::Error>;
}

/// Tool environment names share the sandbox launcher’s reserved namespace.
pub use crate::tool_env::forbidden_env_name;
