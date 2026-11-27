//! Claude CLI (not gateway/`bot-cli`) tool-call checkpoint positions and pre-execution refusal (`CliRefusal`) contract.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const GROUP_PREFIX: &str = "cli-group-";
pub const USE_PREFIX: &str = "cli-use-";
pub const RECORD_PREFIX: &str = "cli-rec-";
pub const RESPONSE_PREFIX: &str = "cli-resp-";

/// Deterministic pre-execution refusal recorded under the current claim before returning the fixed error word to the model.
/// Replayed at the same slot on retry/resume; never recorded when verdict/recording failed or execution status is unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", try_from = "String")]
pub enum CliRefusal {
    UnknownTool,
    Invalid,
    Denied,
    Conflict,
    /// Stale precondition judged under a valid claim; cannot be recorded once the claim is lost.
    Stale,
    /// Wire-encoded as `"unavailable"`; never recorded for store outages.
    #[serde(rename = "unavailable")]
    RateLimited,
}

impl CliRefusal {
    /// Shared refusal checkpoint key format for CLI and API workers (wire-compatible with stored sessions).
    pub fn key(call: &crate::id::CallId) -> String {
        format!("cli-refusal-{}", call.0)
    }

    pub fn step(self, call: &crate::id::CallId) -> crate::command::Step {
        crate::command::Step {
            key: Self::key(call),
            body: crate::command::JsonPayload(serde_json::Value::from(self.word()).to_string()),
        }
    }

    /// Calls already started or settled follow the ledger instead of replaying a refusal.
    pub fn can_replay(record: Option<&crate::ledger::CallRecord>) -> bool {
        record.is_none_or(|row| matches!(row.phase, crate::ledger::CallPhase::Open { .. }))
    }

    /// Corrupt refusal payloads must fail closed rather than allowing execution at the same slot.
    pub fn decode(body: &crate::command::JsonPayload) -> Result<Self, serde_json::Error> {
        serde_json::from_str(&body.0)
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<Value>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    // Preserve runner-validated MCP fields such as structuredContent on live responses.
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

    /// Fixed error word shown to the model; never leaks raw errors or approval nonces.
    pub fn notice(word: &'static str) -> Self {
        Self::text(word, true)
    }

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
        Error::Unavailable | Error::RateLimited => "unavailable",
    }
}

/// Completed provider response tool uses in original block order.
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
    /// Full provider-visible tool name, e.g. `mcp__relay__agenda_list`.
    pub name: String,
    pub input: Value,
}

impl CliResponse {
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

/// Tool-use IDs from one provider response in emission order.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliGroup {
    /// First assistant record sequence number of this response, fixed within the run.
    pub seq: u64,
    pub uses: Vec<String>,
}

/// Maps a CLI session tool use to its persisted turn slot.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliUse {
    /// Session record sequence number containing this tool use.
    pub seq: u64,
    /// Matches `CliGroup.seq` across all uses in the same response.
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
