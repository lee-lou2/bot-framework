//! 어댑터가 인증·정규화해 접수 순서(`Ingress::admit`)에 넘기는 입력. 이 안의 값은 모두 인증된 어댑터가 채운 것이고
//! 메시지 본문이나 헤더의 글로 입구·주체·대화를 바꿀 수 없다.
use crate::entrance::Real;
use bot_core::{
    id::{PrincipalId, RequestId},
    state::{Conversation, ConversationKind},
};
use serde_json::Value;

/// 접수 사건. 트리거 후보를 고르는 기준이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Event {
    /// 사람의 대화 한 건(Slack 글·CLI 채팅). 트리거 `{event: message}`에 대응한다.
    Message,
    /// 인증된 HTTP 요청. 트리거 `{http: {method, path}}`에 대응한다.
    Http { method: String, path: String },
}

/// 접수 뒤 HTTP 응답 방식. 트리거의 `response.mode`이고 채팅은 항상 `Accepted`다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Respond {
    Accepted,
    Sync,
}

impl From<Respond> for bot_core::profile::ResponseMode {
    fn from(respond: Respond) -> Self {
        match respond {
            Respond::Accepted => Self::Accepted,
            Respond::Sync => Self::Sync,
        }
    }
}

impl From<&bot_core::profile::ResponseMode> for Respond {
    fn from(mode: &bot_core::profile::ResponseMode) -> Self {
        match mode {
            bot_core::profile::ResponseMode::Accepted => Self::Accepted,
            bot_core::profile::ResponseMode::Sync => Self::Sync,
        }
    }
}

/// 로그에 `{:?}`로 찍지 않는다(본문이 들어 있다).
pub struct Inbound {
    pub real: Real,
    pub principal: PrincipalId,
    /// 재시도·재전달에도 같은 값. Slack은 event_id, HTTP·CLI는 호출자가 주는 키.
    pub request: RequestId,
    pub conversation: Conversation,
    /// 접근 권한이 묶인 첨부 참조(내려받지 않는다)
    pub attachments: Vec<String>,
    /// 어댑터가 검증한 본문. 채팅은 `{"text": "…"}`, HTTP는 요청 JSON 그대로.
    pub body: Value,
    pub event: Event,
}

pub fn kind_name(kind: &ConversationKind) -> &'static str {
    match kind {
        ConversationKind::Direct => "direct",
        ConversationKind::Channel => "channel",
        ConversationKind::Cli => "cli",
        ConversationKind::Http => "http",
        ConversationKind::Scheduled => "scheduled",
    }
}

/// 요청 ID로 쓸 수 있는 글: 1~128자, 영숫자와 `. _ : -`
pub fn valid_request_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_short_and_delimiter_free() {
        for ok in ["a", "Ev0123ABC", "req-1.2_3:4", &"x".repeat(128)] {
            assert!(valid_request_id(ok), "{ok}");
        }
        for bad in ["", " ", "a b", "a/b", "a\n", "한글", &"x".repeat(129)] {
            assert!(!valid_request_id(bad), "{bad:?}");
        }
    }
}
