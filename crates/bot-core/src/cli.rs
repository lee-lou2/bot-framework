//! Claude CLI가 한 모델 응답에서 낸 도구 호출의 **위치**(체크포인트 키)와 실행 전 확정 거절(`CliRefusal`) 계약. 런타임이 기록하고 MCP 중계(브로커 프런트)가 확인한다.
//! 이름의 `cli`는 Claude CLI다. CLI 입구(`bot_gateway::cli`)나 `bot` 바이너리(`bot-cli` 크레이트)가 아니다.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GROUP_PREFIX: &str = "cli-group-";
pub const USE_PREFIX: &str = "cli-use-";
pub const RECORD_PREFIX: &str = "cli-rec-";
pub const RESPONSE_PREFIX: &str = "cli-resp-";

/// 검증된 모델 호출 위치에서 실행 전에 내린 최종 거절. 현재 claim으로 먼저 저장한 뒤
/// 모델에 같은 단어를 오류로 돌려준다. 같은 위치는 재전달·재개 때 이 결과를 재생한다.
/// 판정이나 기록에 실패했거나 실행 여부가 불명인 호출에는 이 표식을 쓸 수 없다.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum CliRefusal {
    UnknownTool,
    Invalid,
    Denied,
    Conflict,
    /// 유효한 claim에서 호출 조건이 낡았다고 판정한 경우. claim을 잃으면 기록할 수 없다.
    Stale,
    /// 호출 한도에 걸렸을 때의 기존 모델 응답. 저장소 장애에는 표식이 생기지 않는다.
    #[serde(rename = "unavailable")]
    RateLimited,
}

impl CliRefusal {
    /// CLI와 API 워커가 같은 형식으로 읽는 확정 거절 표식. 기존 키는 저장된 세션과 호환된다.
    pub fn key(call: &crate::id::CallId) -> String {
        format!("cli-refusal-{}", call.0)
    }

    /// 현재 claim의 체크포인트에 모델이 받을 거절 하나만 저장한다.
    pub fn step(self, call: &crate::id::CallId) -> crate::command::Step {
        crate::command::Step {
            key: Self::key(call),
            body: crate::command::JsonPayload(serde_json::Value::from(self.word()).to_string()),
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::UnknownTool => "unknown_tool",
            Self::Invalid => "invalid",
            Self::Denied => "denied",
            Self::Conflict => "conflict",
            Self::Stale => "stale",
            Self::RateLimited => "unavailable",
        }
    }

    pub fn from_word(word: &str) -> Option<Self> {
        [
            Self::UnknownTool,
            Self::Invalid,
            Self::Denied,
            Self::Conflict,
            Self::Stale,
            Self::RateLimited,
        ]
        .into_iter()
        .find(|refusal| refusal.word() == word)
    }
}

impl TryFrom<String> for CliRefusal {
    type Error = &'static str;

    fn try_from(word: String) -> Result<Self, Self::Error> {
        Self::from_word(&word).ok_or("invalid refusal")
    }
}

/// 라이브 MCP 응답과 복원한 세션의 같은 도구 결과.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    // structuredContent 등 도구 실행기가 검사한 MCP 필드를 라이브 응답에 보존한다.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

impl ToolOutput {
    pub fn from_payload(payload: &str) -> Self {
        if let Some(value) = crate::canon::parse(payload)
            && value.get("content").is_some_and(Value::is_array)
        {
            return serde_json::from_value(value).unwrap_or_else(|_| Self::notice("invalid"));
        }
        Self::text(payload, false)
    }

    /// 모델에 보일 고정 오류 단어. 원문 오류나 승인 nonce는 전달하지 않는다.
    pub fn notice(word: &'static str) -> Self {
        Self::text(word, true)
    }

    /// 자식 에이전트가 돌려준 값은 위임의 성공 결과 한 건이다.
    pub fn child_result(value: &Value) -> Self {
        match value {
            Value::String(text) => Self::text(text, false),
            other => Self::text(&other.to_string(), false),
        }
    }

    fn text(text: &str, is_error: bool) -> Self {
        Self {
            content: vec![serde_json::json!({"type":"text", "text":text})],
            is_error,
            extra: Default::default(),
        }
    }
}

pub fn error_word(error: &crate::port::Error) -> &'static str {
    use crate::port::Error;
    match error {
        Error::Invalid => "invalid",
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Missing => "unknown_tool",
        Error::Unavailable => "unavailable",
    }
}

/// 제공자가 완료한 응답에서 읽은 도구 사용 목록. 순서는 원래 응답의 블록 순서다.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliResponse {
    pub id: String,
    pub uses: Vec<CliResponseUse>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliResponseUse {
    pub id: String,
    /// 제공자가 본 전체 도구 이름. 예: mcp__relay__agenda_list.
    pub name: String,
    pub input: Value,
}

impl CliResponse {
    /// 체크포인트 키로 쓸 수 있는 ID와 서로 다른 도구 사용을 가진 완전한 목록인지 검사한다.
    pub fn is_valid(&self) -> bool {
        fn valid_id(id: &str) -> bool {
            (1..=64).contains(&id.len())
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        }
        let mut seen = std::collections::BTreeSet::new();
        valid_id(&self.id)
            && !self.uses.is_empty()
            && self.uses.iter().all(|u| {
                valid_id(&u.id) && seen.insert(&u.id) && !u.name.is_empty() && u.input.is_object()
            })
    }
}

/// 같은 provider 응답의 모든 도구 사용 ID를 보낸 순서대로 보존한다.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliGroup {
    /// 이 응답의 첫 assistant 레코드 번호. run 안에서 고정된다.
    pub seq: u64,
    pub uses: Vec<String>,
}

/// CLI 세션의 한 도구 사용과 저장된 차례의 위치를 연결한다.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliUse {
    /// 이 도구가 담긴 세션 레코드의 번호.
    pub seq: u64,
    /// CliGroup.seq, 같은 응답의 모든 사용에 공통이다.
    pub turn: u64,
    pub pos: u32,
    pub name: String,
    pub input: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::CallId;
    use serde_json::json;

    #[test]
    fn a_refusal_preserves_the_slot_and_the_exact_word_on_resume() {
        let key = CliRefusal::key(&CallId::cli(7, 0));
        assert_eq!(key, "cli-refusal-cli-7-0");
        assert_ne!(key, CliRefusal::key(&CallId::cli(7, 1)));
        for (refusal, word) in [
            (CliRefusal::UnknownTool, "unknown_tool"),
            (CliRefusal::Invalid, "invalid"),
            (CliRefusal::Denied, "denied"),
            (CliRefusal::Conflict, "conflict"),
            (CliRefusal::Stale, "stale"),
            (CliRefusal::RateLimited, "unavailable"),
        ] {
            let stored = serde_json::to_value(refusal).unwrap();
            assert_eq!(stored, json!(word));
            let restored = serde_json::from_value::<CliRefusal>(stored).unwrap();
            assert_eq!(restored, refusal);
            let output = ToolOutput::notice(restored.word());
            assert!(output.is_error);
            assert_eq!(output.content, [json!({"type":"text","text":word})]);
        }
        for invalid in [
            json!("secret_missing"),
            json!("destination_unsupported"),
            json!("held"),
            json!("rate_limited"),
            json!({"invalid":null}),
            json!(0),
            json!({"reason":"invalid"}),
        ] {
            assert!(serde_json::from_value::<CliRefusal>(invalid).is_err());
        }
    }

    #[test]
    fn restore_preserves_mcp_blocks_and_error_status() {
        let wire = json!({
            "content":[{"type":"image","mimeType":"image/png","data":"YQ=="}],
            "isError":true, "structuredContent":{"receipt":"r1"}
        });
        let output = ToolOutput::from_payload(&wire.to_string());
        assert!(output.is_error);
        assert_eq!(output.content[0], wire["content"][0]);
        assert_eq!(serde_json::to_value(output).unwrap(), wire);
        assert!(!ToolOutput::from_payload(r#"{"content":[]}"#).is_error);
        assert!(ToolOutput::from_payload(r#"{"content":[],"isError":"wrong"}"#).is_error);
    }

    #[test]
    fn ordinary_results_and_host_errors_remain_distinct_on_resume() {
        let payload = r#"{"receipt":"r1"}"#;
        let output = ToolOutput::from_payload(payload);
        assert!(!output.is_error);
        assert_eq!(output.content, [json!({"type":"text","text":payload})]);
        let denied = ToolOutput::notice(error_word(&crate::port::Error::Denied));
        assert!(denied.is_error);
        assert_eq!(denied.content, [json!({"type":"text","text":"denied"})]);
    }

    #[test]
    fn child_results_always_render_as_one_successful_text_block() {
        for (value, text) in [
            (json!("answer"), "answer"),
            (
                json!({"content":[],"isError":true}),
                r#"{"content":[],"isError":true}"#,
            ),
            (Value::Null, "null"),
        ] {
            let output = ToolOutput::child_result(&value);
            assert!(!output.is_error);
            assert_eq!(output.content, [json!({"type":"text","text":text})]);
        }
    }

    #[test]
    fn completed_response_binds_distinct_uses_to_original_names_and_json_objects() {
        let wire = json!({"id":"msg_A-1", "uses":[
            {"id":"u1","name":"mcp__relay__agenda_list","input":{"at":1}},
            {"id":"u2","name":"Read","input":{}}
        ]});
        let proof: CliResponse = serde_json::from_value(wire.clone()).unwrap();
        assert!(proof.is_valid());
        assert_eq!(serde_json::to_value(&proof).unwrap(), wire);
        for (path, bad) in [
            ("/id", json!("")),
            ("/id", json!("a/b")),
            ("/uses/1/id", json!("u1")),
            ("/uses/0/name", json!("")),
            ("/uses/0/input", json!("{}")),
            ("/uses", json!([])),
        ] {
            let mut invalid = wire.clone();
            *invalid.pointer_mut(path).unwrap() = bad;
            assert!(
                !serde_json::from_value::<CliResponse>(invalid)
                    .unwrap()
                    .is_valid()
            );
        }
    }

    #[test]
    fn separate_records_of_one_response_keep_two_distinct_call_ids() {
        let group: CliGroup = serde_json::from_value(json!({"seq":7, "uses":["a","b"]})).unwrap();
        let positions: Vec<_> = [7, 8]
            .into_iter()
            .enumerate()
            .map(|(pos, seq)| {
                let entry =
                    json!({"seq":seq, "turn":group.seq, "pos":pos, "name":"relay", "input":{}});
                let slot: CliUse = serde_json::from_value(entry.clone()).unwrap();
                assert_eq!(serde_json::to_value(&slot).unwrap(), entry);
                CallId::cli(slot.turn, slot.pos)
            })
            .collect();
        assert_eq!(
            positions,
            [CallId("cli-7-0".into()), CallId("cli-7-1".into())]
        );
    }
}
