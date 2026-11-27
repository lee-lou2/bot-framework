//! Host `limits` section parsing and authority-fingerprint binding for `tool_call_max_secs`.
//!
//! Raising the ceiling does not raise undeclared tools above the 60s default; non-default values
//! fold into the activation fingerprint so lowering or raising the ceiling fences stale workers.

use bot_core::profile::{DEFAULT_TOOL_CALL_SECS, MAX_TOOL_CALL_SECS};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HostLimits {
    pub tool_call_max_secs: u32,
}

impl Default for HostLimits {
    fn default() -> Self {
        Self {
            tool_call_max_secs: DEFAULT_TOOL_CALL_SECS,
        }
    }
}

pub fn parse(value: &Value) -> Result<HostLimits, String> {
    let Value::Object(map) = value else {
        return Err("limits: must be an object".to_owned());
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
                        format!(
                            "limits.tool_call_max_secs: must be an integer between 1 and {MAX_TOOL_CALL_SECS} seconds"
                        )
                    })?;
            }
            other => return Err(format!("limits.{other}: unknown key")),
        }
    }
    Ok(limits)
}

/// Binds non-default limits into the activation fingerprint while leaving default deployments byte-identical.
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
        assert!(parse(&json!(300)).is_err(), "not an object");
        assert_eq!(
            parse(&json!({"tool_call_max_seconds": 300})).unwrap_err(),
            "limits.tool_call_max_seconds: unknown key"
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
        assert_eq!(
            at(60),
            base,
            "default ceiling must preserve base fingerprint"
        );
        assert_ne!(at(300), base);
        assert_ne!(at(300), at(900));
        assert_ne!(
            at(30),
            at(60),
            "lowering the ceiling must change the fingerprint"
        );
        assert_eq!(at(300), at(300));
        assert_eq!(
            bot_core::random::hex(&at(300)),
            "99cb2ce809dac7836ec9d1250e11ed1cd3b57e2cad4822abf7efd857ac16dc9b"
        );
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
