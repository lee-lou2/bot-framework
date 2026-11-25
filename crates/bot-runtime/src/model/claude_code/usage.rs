//! 공급자가 **요청마다** 실제로 보고한 토큰 수를 CLI의 스트림 이벤트에서 모아, 한 번의 CLI 실행 전체의 실측 합을 가린다.
//!
//! CLI는 `result`에 요청들의 사용량 합을 내지만 공급자가 보고하지 않은 요청은 0으로 채운다. 그래서 `result`의 양수만으로는 «모든 요청이 보고됐다»를 알 수 없다
//! (한 차례만 미보고여도 나머지 합이 양수로 남는다). 여기서는 `message_start`(입력)·`message_delta`(출력, 누적값)를 요청마다 모아
//! ① 모든 요청이 입력·출력을 양수로 보고했고 ② 요청별 합이 `result`의 총합과 정확히 같을 때만 그 합을 실측으로 낸다.
//! 보이지 않는 요청이 더해졌거나(합이 어긋난다) 스트림이 끊긴 요청이 있으면 `None`이다(0으로 채우지 않는다).

use bot_core::command::TokenUsage;
use serde_json::Value;

/// 한 모델 요청에서 공급자가 보고한 계량. `None`은 보고가 없다는 뜻이다.
#[derive(Debug, Default)]
struct Call {
    /// 입력 = 입력 + 캐시 읽기 + 캐시 쓰기(캐시를 한 번만 센다)
    prompt: Option<u64>,
    /// 마지막으로 보고한 출력 수(`message_delta`가 누적값을 보고한다)
    completion: Option<u64>,
}

#[derive(Debug, Default)]
pub(super) struct Calls(Vec<Call>);

fn count(usage: &Value, key: &str) -> Option<u64> {
    usage[key].as_u64()
}

/// 입력 수: `input_tokens`가 보고됐을 때만, 캐시 읽기·쓰기를 더한다.
fn prompt(usage: &Value) -> Option<u64> {
    Some(
        count(usage, "input_tokens")?
            .saturating_add(count(usage, "cache_read_input_tokens").unwrap_or(0))
            .saturating_add(count(usage, "cache_creation_input_tokens").unwrap_or(0)),
    )
}

impl Calls {
    /// `stream_event`의 이벤트 하나를 먹는다. 계량과 무관한 이벤트는 무시한다.
    pub(super) fn feed(&mut self, event: &Value) {
        match event["type"].as_str() {
            // 새 요청의 시작. 시작 때의 출력 수는 잠정값이라 쓰지 않는다
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
                    // 입력 수를 델타에서 다시 보고하는 공급자는 누적값이다
                    if let Some(input) = prompt(usage) {
                        call.prompt = Some(input);
                    }
                }
            }
            _ => {}
        }
    }

    /// `total`은 `result` 이벤트의 `usage`(CLI가 낸 총합). 모든 요청이 완전히 보고됐고 요청별 합이 총합과 같을 때만 실측이다.
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
        // CLI의 총합과 어긋나면 이 스트림에 보이지 않은 요청이 있거나 일부가 빠졌다: 합을 믿을 수 없다
        (self::prompt(total)? == prompt_sum && count(total, "output_tokens")? == completion_sum)
            .then_some(TokenUsage {
                prompt: prompt_sum,
                completion: completion_sum,
            })
    }
}
