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
    /// Names selected from the calling agent’s inherited non-secret configuration.
    pub env: BTreeSet<String>,
    pub secrets: BTreeMap<String, SecretName>,
    pub egress: BTreeSet<String>,
    pub effects: bool,
    pub external: bool,
    pub destination: Option<Destination>,
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
    Cli { account: String },
    SlackChannel { workspace: String, channel: String },
    SlackDm { workspace: String },
    Http { endpoint: String },
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
    pub target: TriggerTarget,
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
