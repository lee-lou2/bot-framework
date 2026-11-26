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
            env: Default::default(),
            secrets: Default::default(),
            egress: Default::default(),
            effects: false,
            external: false,
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
    description: "본인 DM이나 CLI에서 비공개 기억 한 건을 프로필에 공유합니다. 소유자가 정확한 본문을 확인하고 승인해야 합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const SHARE_TOOL: &str = SHARE.id;

pub const CREATE: ModelTool = ModelTool {
    id: "schedule.create",
    class: Risk::Write,
    description: "후속 작업을 예약합니다. instruction에 할 일을 쓰고 at(UTC RFC 3339 시각) 또는 cron(5필드, timezone은 선택) 중 하나를 지정합니다. quiet는 보고할 내용이 없을 때 회신을 생략합니다. 기본 수신자는 현재 대화입니다. 다른 수신자는 destination에 등록된 바인딩과 대화를 지정하며, 소유자가 생성 때 수신자와 지시문을 확인하고 발송할 때마다 초안을 승인합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"at":{"type":"string"},"cron":{"type":"string"},"destination":{"additionalProperties":false,"properties":{"binding":{"type":"string"},"conversation":{"additionalProperties":false,"properties":{"id":{"type":"string"},"kind":{"enum":["direct","channel"],"type":"string"},"thread":{"type":["string","null"]}},"required":["kind","id"],"type":"object"}},"required":["binding","conversation"],"type":"object"},"instruction":{"maxLength":8192,"minLength":1,"type":"string"},"quiet":{"type":"boolean"},"timezone":{"type":"string"}},"required":["instruction"],"type":"object"}"#,
};
pub const CREATE_TOOL: &str = CREATE.id;

pub const LIST: ModelTool = ModelTool {
    id: "schedule.list",
    class: Risk::Read,
    description: "본인의 활성 예약을 조회합니다. 각 예약의 ID, 시각과 quiet 설정을 반환합니다.",
    schema: r#"{"additionalProperties":false,"properties":{},"type":"object"}"#,
};
pub const LIST_TOOL: &str = LIST.id;

pub const CANCEL: ModelTool = ModelTool {
    id: "schedule.cancel",
    class: Risk::Write,
    description: "예약 ID로 본인의 활성 예약 한 건을 취소합니다.",
    schema: r#"{"additionalProperties":false,"properties":{"id":{"maxLength":128,"minLength":1,"type":"string"}},"required":["id"],"type":"object"}"#,
};
pub const CANCEL_TOOL: &str = CANCEL.id;

const MODEL_TOOLS: &[ModelTool] = &[SHARE, CREATE, LIST, CANCEL];

/// 두 실행기가 같은 에이전트 스코프 상한으로 목록을 고른다. 호출 시 브로커가 요청자와 두 판본의 권한을 확인한다.
pub fn model_tools(scopes: &Scopes) -> impl Iterator<Item = &'static ModelTool> {
    MODEL_TOOLS.iter().filter(|spec| scopes.contains(spec.id))
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
