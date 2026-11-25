//! 상류 응답에 서비스 키가 있는가. 키는 요청 머리에만 실려 갔으니 응답 어디에든 있으면 새는 것이다.
//!
//! 원문에서 글자 그대로 찾는 것만으로는 부족하다: 키에 JSON 이스케이프가 필요한 글자(`"`·`\`)가 있으면 원문에는 `\"`·`\\`로 나온다. 그래서 키 자체와 JSON 문자열로 한 번·두 번
//! 이스케이프한 모양을 원문에서 찾고, 응답을 **풀어 읽은 모든 글**(문자열 값·객체 키, 재귀)에서도 찾는다. 글이 다시 JSON이면(호출 인자 글·불투명 공급자 상태) 그 안도 풀어서 같은 검사를 한다.
//! 워커·브로커로 넘길 결과(최종 글·호출과 함께 온 글·인자·공급자 상태)도 따로 본다. 가림 장치(전체 프로필이 같이 쓰는 목록)로 출력을 고치지 않는다: 찾으면 통째로 닫는다.

use crate::model::ModelStep;
use serde_json::Value;

/// 글이 다시 JSON이면 풀어 보는 깊이(이중·삼중 인코딩까지)
const MAX_NEST: usize = 4;

/// 한 요청의 키를 찾는 도구
pub(super) struct Needles(Vec<String>);

impl Needles {
    /// 키와, 그것을 JSON 문자열로 한 번·두 번 이스케이프한 모양
    pub(super) fn new(key: &str) -> Self {
        let once = escaped(key);
        let twice = escaped(&once);
        let mut forms = vec![key.to_owned()];
        for form in [once, twice] {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
        Self(forms)
    }

    fn in_text(&self, text: &str) -> bool {
        self.0.iter().any(|needle| text.contains(needle.as_str()))
    }

    /// 글 자체, 또 그 글이 JSON이면 푼 값 안의 모든 글
    fn in_string(&self, text: &str, depth: usize) -> bool {
        if self.in_text(text) {
            return true;
        }
        let trimmed = text.trim_start();
        depth < MAX_NEST
            && trimmed.starts_with(['{', '[', '"'])
            && serde_json::from_str::<Value>(text)
                .is_ok_and(|inner| self.in_value(&inner, depth + 1))
    }

    /// 푼 JSON 값 안의 모든 글(문자열 값·객체 키)
    fn in_value(&self, value: &Value, depth: usize) -> bool {
        match value {
            Value::String(text) => self.in_string(text, depth),
            Value::Array(items) => items.iter().any(|item| self.in_value(item, depth)),
            Value::Object(map) => map
                .iter()
                .any(|(name, item)| self.in_text(name) || self.in_value(item, depth)),
            _ => false,
        }
    }

    /// 응답 원문과 그것을 푼 모든 글
    pub(super) fn in_body(&self, raw: &str) -> bool {
        self.in_text(raw)
            || serde_json::from_str::<Value>(raw).is_ok_and(|value| self.in_value(&value, 0))
    }

    /// 워커·브로커로 넘길 결과: 글·인자·공급자 상태(푼 모양까지)
    pub(super) fn in_step(&self, step: &ModelStep) -> bool {
        match step {
            ModelStep::Final { text, .. } => self.in_string(text, 0),
            ModelStep::Calls {
                text, calls, state, ..
            } => {
                self.in_string(text, 0)
                    || state
                        .as_deref()
                        .is_some_and(|state| self.in_string(state, 0))
                    || calls.iter().any(|call| self.in_string(&call.args.0, 0))
            }
        }
    }
}

/// JSON 문자열로 이스케이프한 모양(앞뒤 따옴표 없이)
fn escaped(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or_default()
        .to_owned()
}
