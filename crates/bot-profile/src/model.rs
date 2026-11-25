//! 검증 중간 표현. 파일을 읽고 조립·참조 검사를 하는 동안 쓰고, 끝나면 `convert`가
//! `bot_core::profile::Snapshot`으로 바꿔 내보낸다. 바깥에는 보이지 않는다.
use crate::schema::Schema;
use bot_core::{
    command::JsonPayload,
    cron::Cron,
    id::{AgentId, BindingId, PrincipalId, Revision, SecretName, ToolId},
    profile::{Adapter, Builtin, Destination, Document, MemorySeed, Policy},
    state::Risk,
};
use serde_json::{Map, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[derive(Clone, Debug)]
pub struct Member {
    pub identity: PrincipalId,
    /// `service`, `local`, `slack` 같은 신원 종류
    pub kind: bot_core::identity::IdentityKind,
    pub roles: BTreeSet<String>,
    /// 역할 스코프의 합집합 ∩ 프로필 전체 상한
    pub scopes: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct Binding {
    pub id: BindingId,
    pub agent: AgentId,
    pub kind: Adapter,
}

/// 입구를 가리키는 정규 문자열. 활성 프로필 전체에서 유일해야 한다.
pub fn entrance(kind: &Adapter) -> String {
    match kind {
        Adapter::Cli { account } => format!("cli:{account}"),
        Adapter::SlackChannel {
            workspace, channel, ..
        } => {
            format!("slack:{workspace}:channel:{channel}")
        }
        Adapter::SlackDm { workspace } => format!("slack:{workspace}:dm"),
        Adapter::Http { endpoint } => format!("http:{endpoint}"),
        // 같은 별칭에 모델을 여러 개 둘 수 있어, 프로필 안에서는 (별칭, 모델)이 입구다
        Adapter::OpenAi {
            endpoint, model, ..
        } => format!("http:{endpoint}/{model}"),
    }
}

/// 입구를 소유하는 단위의 정규 문자열. 활성 프로필 전체에서 한 프로필만 쓸 수 있다.
/// OpenAI 입구는 별칭 하나를 프로필 하나가 소유하고 그 안에서 모델을 여러 개 둔다(그래서 별칭이 단위다).
pub fn owner_key(kind: &Adapter) -> String {
    match kind {
        Adapter::OpenAi { endpoint, .. } => format!("http:{endpoint}"),
        other => entrance(other),
    }
}

pub fn adapter_name(kind: &Adapter) -> &'static str {
    match kind {
        Adapter::Cli { .. } => "cli",
        Adapter::SlackChannel { .. } | Adapter::SlackDm { .. } => "slack",
        Adapter::Http { .. } => "http",
        Adapter::OpenAi { .. } => "openai",
    }
}

#[derive(Clone, Debug)]
pub struct Tool {
    /// `<스킬>.<도구>` (예: `agenda.list`)
    pub id: ToolId,
    pub name: String,
    pub server: String,
    pub class: Risk,
    pub scopes: BTreeSet<String>,
    pub external: bool,
    pub effects: bool,
    pub destination: Option<Destination>,
    pub env: BTreeSet<String>,
    /// 환경 변수 이름 → 논리 비밀 이름
    pub secrets: BTreeMap<String, SecretName>,
    pub egress: BTreeSet<String>,
    /// 입력 스키마(정규 JSON)
    pub input: JsonPayload,
    pub revision: Revision,
    pub schema: Arc<Schema>,
    /// `tools.lock.yaml`에서 입력 스키마를 가져온 도구의 정의 해시(`bot_core::mcp::definition_sha256`)
    pub definition: Option<String>,
    /// tools.yaml `name`(서버가 내세우는 도구 이름). 생략했거나 도구 id와 같으면 `None`이다.
    pub mcp_name: Option<String>,
    /// tools.yaml `timeout_secs`(호출 시간 한도, 초). 생략하면 `None`(기본 60초).
    pub timeout_secs: Option<u32>,
}

impl Tool {
    /// 선언한 등급. 수신자를 인자로 정하는 전송 도구는 다른 사람에게 보내므로 irreversible로 올린다.
    pub fn risk(&self) -> Risk {
        match self.destination {
            Some(Destination::Explicit { .. }) => Risk::Irreversible,
            _ => self.class,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    /// 프로필 폴더 기준 스킬 폴더 경로
    pub path: String,
    pub instructions: Document,
    pub tools: Vec<Arc<Tool>>,
}

#[derive(Clone, Debug)]
pub enum Source {
    Cron {
        cron: Cron,
        beat: bool,
        principal: PrincipalId,
        approver: Option<PrincipalId>,
    },
    Message {
        binding: BindingId,
    },
    /// 봇이 참여한 스레드의 사람 답글(`on.event: thread-reply`, 바인딩 `events`의 `thread_reply`)
    ThreadReply {
        binding: BindingId,
    },
    /// 등록 채널의 반응(`on.event: reaction`, 바인딩 `events`의 `reaction`)
    Reaction {
        binding: BindingId,
    },
    Http {
        method: String,
        path: String,
        binding: BindingId,
        sync: bool,
    },
}

impl Source {
    /// 같은 후보군 키(바인딩·사건, HTTP는 method/path). cron은 후보군이 없다.
    pub fn group(&self) -> Option<String> {
        match self {
            Self::Cron { .. } => None,
            Self::Message { binding } => Some(format!("message:{}", binding.0)),
            Self::ThreadReply { binding } => Some(format!("thread-reply:{}", binding.0)),
            Self::Reaction { binding } => Some(format!("reaction:{}", binding.0)),
            Self::Http {
                method,
                path,
                binding,
                ..
            } => Some(format!("http:{}:{method} {path}", binding.0)),
        }
    }

    pub fn binding(&self) -> Option<&BindingId> {
        match self {
            Self::Cron { .. } => None,
            Self::Message { binding }
            | Self::ThreadReply { binding }
            | Self::Reaction { binding }
            | Self::Http { binding, .. } => Some(binding),
        }
    }
}

#[derive(Clone, Debug)]
pub enum TriggerTarget {
    Tool {
        tool: ToolId,
        args: Map<String, Value>,
    },
    Instruction(String),
}

/// 봉투의 점 경로와 리터럴의 동등 비교 한 항목
#[derive(Clone, Debug)]
pub struct Match {
    pub path: String,
    pub value: Value,
}

#[derive(Clone, Debug)]
pub struct Trigger {
    pub id: String,
    pub on: Source,
    pub target: TriggerTarget,
    pub order: i64,
    pub private: bool,
    pub matches: Vec<Match>,
    /// 도구 인자 이름 → 봉투의 점 경로
    pub input: BTreeMap<String, String>,
    /// 운영자가 활성화 때 사전 승인한 정적 호출(cron 고정 도구)
    pub preapproved: Option<bot_core::grant::Preapproval>,
    /// 지정한 사람의 메시지·반응으로 승인하는 직접 도구 트리거
    pub human_approval: Option<bot_core::grant::HumanApproval>,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    /// 실효 모델 별칭: 자기 front matter, 없으면 부모, 루트는 프로필 모델
    pub model: String,
    /// 위임 대상으로 보일 한 줄 설명: 자기 front matter(상속 없음), 없으면 경로 마지막 이름
    pub description: String,
    /// 이 에이전트가 명시한 읽기 전용 내장 도구(상속 없음, 기본은 빈 집합)
    pub builtin: BTreeSet<Builtin>,
    /// rules.md → 조상 AGENT.md → 자기 AGENT.md 순
    pub parts: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// 프로필 공용·조상·자기 스킬(이름순)
    pub skills: Vec<Arc<Skill>>,
    /// 프로필·조상·자기 지식(이 순서)
    pub knowledge: Vec<Document>,
    pub triggers: Vec<Trigger>,
    pub memory: Vec<MemorySeed>,
    /// 스코프 상한: 부모 상한 ∩ policy.agents[경로]
    pub ceiling: BTreeSet<String>,
}

impl Agent {
    pub fn skill(&self, name: &str) -> Option<&Arc<Skill>> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// 이 에이전트에게 보이는 도구
    pub fn tool(&self, id: &ToolId) -> Option<&Arc<Tool>> {
        let (skill, _) = id.0.split_once('.')?;
        self.skill(skill)?.tools.iter().find(|t| t.id == *id)
    }

    pub fn tools(&self) -> impl Iterator<Item = &Arc<Tool>> {
        self.skills.iter().flat_map(|s| s.tools.iter())
    }
}

/// profile.yaml에서 읽은 것
pub struct ProfileParts {
    pub id: String,
    pub timezone: String,
    pub model: String,
    pub roles: BTreeMap<String, BTreeSet<String>>,
    pub members: BTreeMap<PrincipalId, Member>,
    pub bindings: BTreeMap<BindingId, Binding>,
    pub secrets: BTreeSet<String>,
    pub policy: Policy,
}

/// 검증을 마친 프로필 하나(변환 직전)
pub struct Built {
    pub parts: ProfileParts,
    pub agents: BTreeMap<AgentId, Agent>,
    pub root: AgentId,
}
