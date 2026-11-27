//! 모델에 공개하는 호스트 도구와 승인·원장에 기록하는 이름. 실행과 인가는 각 처리기가 맡는다.
use crate::{
    command::JsonPayload,
    id::{AgentId, Revision},
    profile::{Scopes, Tool},
    state::Risk,
};
use serde_json::{Value, json};

/// 이미 발급한 승인과 영수증이 참조하는 호스트 내장 도구 판본.
pub const REVISION: &str = "builtin-1";

/// `schedule.create`의 한 번 예약을 지금부터 이 밀리초(1,830일)까지 받는다.
pub const MAX_SCHEDULE_AHEAD_MS: i64 = 5 * 366 * 24 * 3600 * 1000;

/// 모델 목록과 브로커 선언이 공유하는 명세. 각 도구에는 ID와 같은 이름의 스코프가 필요하다.
pub struct ModelTool {
    pub id: &'static str,
    pub class: Risk,
    /// 외부 네트워크로 나가거나 외부 자원을 만지는가(비공개 기억 뒤 호출 시 승인 상승 대상).
    pub external: bool,
    /// 슈퍼 관리자(`profile.yaml`의 `super_admins`)에게만 노출·허용하는 관리 도구인가.
    pub super_admin: bool,
    pub description: &'static str,
    pub schema: &'static str,
}

impl ModelTool {
    pub fn declaration(&self) -> Tool {
        Tool {
            server: "builtin".into(),
            revision: Revision(REVISION.into()),
            class: self.class,
            scopes: Scopes::from([self.id.to_owned()]),
            input: JsonPayload(self.schema.into()),
            description: None,
            env: Default::default(),
            secrets: Default::default(),
            egress: Default::default(),
            effects: false,
            external: self.external,
            destination: None,
            definition: None,
            mcp_name: None,
            timeout_secs: None,
        }
    }
}

pub const SHARE: ModelTool = ModelTool {
    id: "memory.share",
    class: Risk::Irreversible,
    external: false,
    super_admin: false,
    description: "본인 DM이나 CLI에서 비공개 기억 한 건을 프로필에 공유합니다. 소유자가 정확한 본문을 확인하고 승인해야 합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const SHARE_TOOL: &str = SHARE.id;

pub const CREATE: ModelTool = ModelTool {
    id: "schedule.create",
    class: Risk::Write,
    external: false,
    super_admin: false,
    description: "후속 작업을 예약합니다. instruction에 할 일을 쓰고 at(UTC RFC 3339 시각) 또는 cron(5필드, timezone은 선택) 중 하나를 지정합니다. quiet는 보고할 내용이 없을 때 회신을 생략합니다. 기본 수신자는 현재 대화입니다. 다른 수신자는 destination에 등록된 바인딩과 대화를 지정하며, 소유자가 생성 때 수신자와 지시문을 확인하고 발송할 때마다 초안을 승인합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"at":{"type":"string"},"cron":{"type":"string"},"destination":{"additionalProperties":false,"properties":{"binding":{"type":"string"},"conversation":{"additionalProperties":false,"properties":{"id":{"type":"string"},"kind":{"enum":["direct","channel"],"type":"string"},"thread":{"type":["string","null"]}},"required":["kind","id"],"type":"object"}},"required":["binding","conversation"],"type":"object"},"instruction":{"maxLength":8192,"minLength":1,"type":"string"},"quiet":{"type":"boolean"},"timezone":{"type":"string"}},"required":["instruction"],"type":"object"}"#,
};
pub const CREATE_TOOL: &str = CREATE.id;

pub const LIST: ModelTool = ModelTool {
    id: "schedule.list",
    class: Risk::Read,
    external: false,
    super_admin: false,
    description: "본인의 활성 예약을 조회합니다. 각 예약의 ID, 시각과 quiet 설정을 반환합니다.",
    schema: r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
};
pub const LIST_TOOL: &str = LIST.id;

pub const CANCEL: ModelTool = ModelTool {
    id: "schedule.cancel",
    class: Risk::Write,
    external: false,
    super_admin: false,
    description: "예약 ID로 본인의 활성 예약 한 건을 취소합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const CANCEL_TOOL: &str = CANCEL.id;

pub const SKILL_INSPECT: ModelTool = ModelTool {
    id: "skill.inspect",
    class: Risk::Read,
    external: false,
    super_admin: true,
    description: "슈퍼 관리자 전용. 현재 프로필의 에이전트 구조·설명·스킬 용량(32 KiB 한도), 스킬 목록, 등록된 MCP 서버, 비밀 이름(값 제외)을 조회합니다. skill을 지정하면 해당 스킬의 본문까지 반환합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"skill":{"maxLength":63,"minLength":1,"type":"string"}},"type":"object"}"#,
};
pub const SKILL_INSPECT_TOOL: &str = SKILL_INSPECT.id;

pub const SKILL_CREATE: ModelTool = ModelTool {
    id: "skill.create",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "슈퍼 관리자 전용. 새 스킬(SKILL.md)을 프로필 공용(agent 생략 또는 null) 또는 특정 에이전트에 생성하고 검증 뒤 즉시 활성화합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"body":{"maxLength":32768,"minLength":1,"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name","description","body"],"type":"object"}"#,
};
pub const SKILL_CREATE_TOOL: &str = SKILL_CREATE.id;

pub const SKILL_UPDATE: ModelTool = ModelTool {
    id: "skill.update",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "슈퍼 관리자 전용. 기존 스킬의 description이나 본문(body)을 수정하고 기존 도구 선언은 유지한 채 검증 뒤 즉시 활성화합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"body":{"maxLength":32768,"minLength":1,"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name"],"type":"object"}"#,
};
pub const SKILL_UPDATE_TOOL: &str = SKILL_UPDATE.id;

pub const SKILL_DELETE: ModelTool = ModelTool {
    id: "skill.delete",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "슈퍼 관리자 전용. 기존 스킬 폴더를 삭제하고 검증 뒤 즉시 활성화합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"name":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["name"],"type":"object"}"#,
};
pub const SKILL_DELETE_TOOL: &str = SKILL_DELETE.id;

pub const MCP_ADD: ModelTool = ModelTool {
    id: "mcp.add",
    class: Risk::Write,
    external: true,
    super_admin: true,
    description: "슈퍼 관리자 전용. 원격 Streamable HTTP 또는 로컬 stdio MCP 서버를 호스트 설정·프로필·스킬에 연결하고 검증 뒤 즉시 활성화합니다. 비밀 값은 SOPS/키체인 보관소에만 암호화 저장하고 평문 파일에는 적지 않습니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"alias":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"auth_secret":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"command":{"items":{"minLength":1,"type":"string"},"minItems":1,"type":"array"},"default_class":{"enum":["read","write","irreversible"],"type":"string"},"description":{"maxLength":1024,"minLength":1,"type":"string"},"instructions":{"maxLength":32768,"minLength":1,"type":"string"},"only_tools":{"items":{"minLength":1,"type":"string"},"type":"array"},"roles":{"items":{"maxLength":63,"minLength":1,"type":"string"},"type":"array"},"root":{"minLength":1,"type":"string"},"secret_value":{"minLength":1,"type":"string"},"secret_values":{"additionalProperties":{"minLength":1,"type":"string"},"type":"object"},"skill":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"stdio_tools":{"items":{"additionalProperties":false,"properties":{"class":{"enum":["read","write","irreversible"],"type":"string"},"description":{"maxLength":4096,"type":"string"},"egress":{"items":{"type":"string"},"type":"array"},"env":{"items":{"type":"string"},"type":"array"},"input":{"type":"object"},"name":{"maxLength":63,"minLength":1,"type":"string"},"secrets":{"additionalProperties":{"maxLength":63,"minLength":1,"type":"string"},"type":"object"},"timeout_secs":{"maximum":900,"minimum":1,"type":"integer"}},"required":["name","class","input"],"type":"object"},"type":"array"},"tool_classes":{"additionalProperties":{"enum":["read","write","irreversible"],"type":"string"},"type":"object"},"transport":{"enum":["http","stdio"],"type":"string"},"url":{"minLength":1,"type":"string"}},"required":["alias","transport"],"type":"object"}"#,
};
pub const MCP_ADD_TOOL: &str = MCP_ADD.id;

pub const MCP_REMOVE: ModelTool = ModelTool {
    id: "mcp.remove",
    class: Risk::Write,
    external: false,
    super_admin: true,
    description: "슈퍼 관리자 전용. 등록된 MCP 서버와 스킬 도구 바인딩을 제거하고 검증 뒤 즉시 활성화합니다. remove_secrets가 true이면 미사용 비밀도 암호화 보관소에서 제거합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"agent":{"maxLength":256,"minLength":1,"type":["string","null"]},"alias":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"},"delete_skill":{"type":"boolean"},"remove_secrets":{"type":"boolean"},"skill":{"maxLength":63,"minLength":1,"pattern":"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$","type":"string"}},"required":["alias"],"type":"object"}"#,
};
pub const MCP_REMOVE_TOOL: &str = MCP_REMOVE.id;

const MODEL_TOOLS: &[ModelTool] = &[
    SHARE,
    CREATE,
    LIST,
    CANCEL,
    SKILL_INSPECT,
    SKILL_CREATE,
    SKILL_UPDATE,
    SKILL_DELETE,
    MCP_ADD,
    MCP_REMOVE,
];

/// 일반 요청자용 내장 도구 목록(슈퍼 관리자 전용 도구는 제외한다).
pub fn model_tools(scopes: &Scopes) -> impl Iterator<Item = &'static ModelTool> {
    model_tools_for(scopes, false)
}

/// 두 실행기가 같은 에이전트 스코프 상한과 슈퍼 관리자 여부로 목록을 고른다. 호출 시 브로커가 요청자와 두 판본의 권한을 다시 확인한다.
pub fn model_tools_for(
    scopes: &Scopes,
    is_super_admin: bool,
) -> impl Iterator<Item = &'static ModelTool> {
    MODEL_TOOLS
        .iter()
        .filter(move |spec| (!spec.super_admin || is_super_admin) && scopes.contains(spec.id))
}

/// 슈퍼 관리자 전용 관리 도구 명세를 ID로 찾는다.
pub fn manage_tool(id: &str) -> Option<&'static ModelTool> {
    MODEL_TOOLS
        .iter()
        .find(|spec| spec.super_admin && spec.id == id)
}

/// 실제 발화에서 소유자가 회신 초안을 승인하는 호스트 전용 도구.
pub const DELIVER_TOOL: &str = "schedule.deliver";
pub const DELEGATE_TOOL: &str = "delegate";
pub const DELEGATE_DESCRIPTION: &str = "직계 하위 에이전트에게 일을 맡기고 그 결과를 받습니다.";

pub fn delegate_schema<'a>(children: impl IntoIterator<Item = &'a AgentId>) -> Value {
    let ids: Vec<&str> = children.into_iter().map(|id| id.0.as_str()).collect();
    json!({
        "additionalProperties": false,
        "properties": {
            "agent": {"enum": ids, "type": "string"},
            "instruction": {"minLength": 1, "type": "string"},
        },
        "required": ["agent", "instruction"],
        "type": "object",
    })
}

pub fn reserved(id: &str) -> bool {
    matches!(id, DELIVER_TOOL | DELEGATE_TOOL) || MODEL_TOOLS.iter().any(|tool| id == tool.id)
}

/// 저장소에서 원문을 가리고 관리 API에서 내용과 승인 지문을 다시 대조할 카드 본문의 종류.
#[derive(Clone, Copy, Eq, PartialEq)]
pub enum CardBody {
    None,
    Share,
    Draft,
}

pub fn card_body(id: &str) -> CardBody {
    match id {
        SHARE_TOOL => CardBody::Share,
        DELIVER_TOOL => CardBody::Draft,
        _ => CardBody::None,
    }
}
