//! Per-request token usage tracker for Claude Code stream events.
//!
//! Because CLI `result.usage` zero-fills unreported requests, measured totals are emitted only when
//! every `message_start`/`message_delta` call reports positive counts whose sum matches `result.usage`.

use bot_core::command::TokenUsage;
use serde_json::Value;

#[derive(Debug, Default)]
struct Call {
    prompt: Option<u64>,
    completion: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct Calls(Vec<Call>);

fn count(usage: &Value, key: &str) -> Option<u64> {
    usage[key].as_u64()
}

fn prompt(usage: &Value) -> Option<u64> {
    Some(
        count(usage, "input_tokens")?
            .saturating_add(count(usage, "cache_read_input_tokens").unwrap_or(0))
            .saturating_add(count(usage, "cache_creation_input_tokens").unwrap_or(0)),
    )
}

impl Calls {
    pub(super) fn feed(&mut self, event: &Value) {
        match event["type"].as_str() {
            // Ignore provisional output_tokens on message_start; final cumulative count comes on message_delta.
            Some("message_start") => self.0.push(Call {
                prompt: prompt(&event["message"]["usage"]),
                completion: None,
            }),
            Some("message_delta") => {
                if let Some(call) = self.0.last_mut() {
                    let usage = &event["usage"];
                    if let Some(output) = count(usage, "output_tokens") {
                        call.completion = Some(output);
                    }
                    if let Some(input) = prompt(usage) {
                        call.prompt = Some(input);
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn measured(&self, total: &Value) -> Option<TokenUsage> {
        if self.0.is_empty() {
            return None;
        }
        let (mut prompt_sum, mut completion_sum) = (0u64, 0u64);
        for call in &self.0 {
            let (prompt, completion) = (call.prompt?, call.completion?);
            if prompt == 0 || completion == 0 {
                return None;
            }
            prompt_sum = prompt_sum.saturating_add(prompt);
            completion_sum = completion_sum.saturating_add(completion);
        }
        // Any mismatch with CLI total indicates unobserved or partial requests; drop measured usage.
        (self::prompt(total)? == prompt_sum && count(total, "output_tokens")? == completion_sum)
            .then_some(TokenUsage {
                prompt: prompt_sum,
                completion: completion_sum,
            })
    }
}
