//! 호스트 `limits` 절: 도구 호출 시간 한도의 천장. `{"limits": {"tool_call_max_secs": 300}}`.
//!
//! 천장은 도구가 `timeout_secs`로 선언할 수 있는 값의 상한이다(기본 60초, 최대 900초). **천장만 올려도 선언하지 않은 도구의 호출 한도는 60초 그대로다.**
//! 도구의 실효 한도(선언 또는 60초)가 천장을 넘으면 활성화를 거절하고, 활성화 뒤에 천장을 내렸거나 복원한 옛 선언이면 실행기가 보내기 전에 거절한다.
//! 천장은 활성화 지문에 들어가(`bind`) 바꾸면 활성화 사건이다: 옛 천장으로 뜬 프로세스는 구판이 된다. 기본값(60초)이면 지문이 그대로다.
use bot_core::profile::{DEFAULT_TOOL_CALL_SECS, MAX_TOOL_CALL_SECS};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostLimits {
    /// 도구가 선언할 수 있는 호출 시간 한도(초)의 천장
    pub tool_call_max_secs: u32,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            tool_call_max_secs: DEFAULT_TOOL_CALL_SECS,
        }
    }
}

/// `limits` 절을 읽는다. 모르는 키와 범위 밖의 값(1~900초가 아니거나 정수가 아닌 값)은 거절한다.
pub fn parse(value: &Value) -> Result<HostLimits, String> {
    let Value::Object(map) = value else {
        return Err("limits: 객체여야 해요".to_owned());
    };
    let mut limits = HostLimits::default();
    for (key, value) in map {
        match key.as_str() {
            "tool_call_max_secs" => {
                limits.tool_call_max_secs = value
                    .as_u64()
                    .and_then(|secs| u32::try_from(secs).ok())
                    .filter(|secs| (1..=MAX_TOOL_CALL_SECS).contains(secs))
                    .ok_or_else(|| {
                        format!("limits.tool_call_max_secs: 1~{MAX_TOOL_CALL_SECS} 사이의 정수(초)여야 해요")
                    })?;
            }
            other => return Err(format!("limits.{other}: 알 수 없는 키예요")),
        }
    }
    Ok(limits)
}

/// 활성화 지문에 천장을 묶는다. 기본값이면 `base` 그대로다(기존 지문이 바뀌지 않는다).
pub fn bind(base: [u8; 32], limits: &HostLimits) -> [u8; 32] {
    if limits.tool_call_max_secs == DEFAULT_TOOL_CALL_SECS {
        return base;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-authority-limits-v1\0");
    hash.update(base);
    hash.update(limits.tool_call_max_secs.to_be_bytes());
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_is_sixty_and_an_omitted_section_or_key_means_the_default() {
        assert_eq!(HostLimits::default().tool_call_max_secs, 60);
        assert_eq!(parse(&json!({})).unwrap(), HostLimits::default());
        assert_eq!(
            parse(&json!({"tool_call_max_secs": 900}))
                .unwrap()
                .tool_call_max_secs,
            900
        );
    }

    #[test]
    fn only_whole_seconds_from_one_to_the_maximum_are_accepted() {
        for bad in [
            json!(0),
            json!(901),
            json!(-5),
            json!(1.5),
            json!(60.0),
            json!("300"),
            json!(null),
            json!(true),
            json!(4_294_967_356_u64),
        ] {
            let error = parse(&json!({"tool_call_max_secs": bad})).unwrap_err();
            assert!(
                error.starts_with("limits.tool_call_max_secs"),
                "{bad}: {error}"
            );
        }
        for ok in [1, 59, 60, 61, 900] {
            assert!(parse(&json!({"tool_call_max_secs": ok})).is_ok(), "{ok}");
        }
        assert!(parse(&json!(300)).is_err(), "객체가 아니다");
        assert_eq!(
            parse(&json!({"tool_call_max_seconds": 300})).unwrap_err(),
            "limits.tool_call_max_seconds: 알 수 없는 키예요"
        );
    }

    #[test]
    fn the_ceiling_binds_into_the_fingerprint_only_when_it_is_not_the_default() {
        let base = [7u8; 32];
        let at = |secs| {
            bind(
                base,
                &HostLimits {
                    tool_call_max_secs: secs,
                },
            )
        };
        assert_eq!(at(60), base, "기본 천장이면 기존 지문 그대로");
        assert_ne!(at(300), base);
        assert_ne!(at(300), at(900));
        assert_ne!(at(30), at(60), "내린 천장도 활성화 사건이다");
        assert_eq!(at(300), at(300));
        // 다른 바탕 지문은 다른 결과다
        assert_ne!(
            bind(
                [8u8; 32],
                &HostLimits {
                    tool_call_max_secs: 300
                }
            ),
            at(300)
        );
    }
}
