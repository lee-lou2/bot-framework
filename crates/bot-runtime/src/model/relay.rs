//! Per-attempt broker front relay channel and Claude Code `--mcp-config` generation.
//!
//! Security boundary: session tokens are never written to any file. The MCP config uses
//! `${RELAY_TOKEN}` env expansion so read-only built-ins cannot exfiltrate tokens from disk
//! (`ANTHROPIC_API_KEY` cannot be used here because Claude Code strips it during config expansion).

use super::ModelError;
use serde_json::json;
use std::fmt;

pub const TOKEN_ENV: &str = "RELAY_TOKEN";

#[derive(Clone, Eq, PartialEq)]
pub struct RelayChannel {
    pub name: String,
    pub base_url: String,
    pub token: String,
}

impl fmt::Debug for RelayChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayChannel")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Holds a relay channel and revokes only this attempt's session token on drop.
pub struct RelayGrant {
    pub channel: RelayChannel,
    _revoke: Box<dyn Send + Sync>,
}

impl RelayGrant {
    pub fn new(channel: RelayChannel, revoke: impl Send + Sync + 'static) -> Self {
        Self {
            channel,
            _revoke: Box::new(revoke),
        }
    }
}

fn simple_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && s.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn token_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

fn base_ok(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
    else {
        return false;
    };
    !rest.is_empty()
        && s.len() <= 256
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

impl RelayChannel {
    pub fn validate(&self) -> Result<(), ModelError> {
        if simple_name(&self.name) && base_ok(&self.base_url) && token_ok(&self.token) {
            Ok(())
        } else {
            Err(ModelError::Failed {
                code: "invalid_relay",
                transient: false,
            })
        }
    }

    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    pub fn codex_base_url(&self) -> String {
        format!("{}/v1", self.base_url)
    }

    fn bearer() -> String {
        format!("Bearer ${{{TOKEN_ENV}}}")
    }

    pub fn claude_config(&self) -> String {
        let server = json!({
            "type": "http",
            "url": self.mcp_url(),
            "headers": {"Authorization": Self::bearer()},
        });
        json!({"mcpServers": {&self.name: server}}).to_string()
    }

    /// AGY MCP config template using `${RELAY_TOKEN}` (expanded to RAM-only `/tmp/mcp_config.json` by `bot-init`).
    pub fn agy_mcp_template(&self) -> String {
        let server = json!({
            "url": self.mcp_url(),
            "headers": {"Authorization": Self::bearer()},
        });
        json!({"mcpServers": {&self.name: server}}).to_string()
    }

    pub fn claude_allowed_tools(&self) -> String {
        format!("mcp__{}", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    fn channel() -> RelayChannel {
        RelayChannel {
            name: "relay".into(),
            base_url: "http://192.168.64.1:8787".into(),
            token: "sess_tok.1".into(),
        }
    }

    #[test]
    fn claude_config_points_at_the_http_mcp_endpoint_and_names_the_token_variable() {
        let config: Value = serde_json::from_str(&channel().claude_config()).unwrap();
        let server = &config["mcpServers"]["relay"];
        assert_eq!(server["type"], "http");
        assert_eq!(server["url"], "http://192.168.64.1:8787/mcp");
        assert_eq!(
            server["headers"],
            json!({"Authorization": "Bearer ${RELAY_TOKEN}"})
        );
        assert_eq!(TOKEN_ENV, "RELAY_TOKEN");
        assert_eq!(channel().claude_allowed_tools(), "mcp__relay");
    }

    #[test]
    fn no_config_ever_contains_the_token_itself() {
        // Construct sample at runtime to avoid triggering secret scanners.
        let sample = ["tok_9f3a", "1c77d2e84b60"].concat();
        let relay = RelayChannel {
            token: sample.clone(),
            ..channel()
        };
        let config = relay.claude_config();
        assert!(!config.contains(&sample), "{config}");
        assert!(config.contains("Bearer ${RELAY_TOKEN}"), "{config}");
        let agy = relay.agy_mcp_template();
        assert!(!agy.contains(&sample), "{agy}");
        assert!(agy.contains("Bearer ${RELAY_TOKEN}"), "{agy}");
        assert_eq!(relay.codex_base_url(), "http://192.168.64.1:8787/v1");
    }

    #[test]
    fn the_token_never_shows_up_in_diagnostics() {
        let text = format!("{:?}", channel());
        assert!(!text.contains("sess_tok.1") && text.contains("redacted"));
    }

    #[test]
    fn invalid_shapes_are_rejected_before_any_file_is_written() {
        let bad = ModelError::Failed {
            code: "invalid_relay",
            transient: false,
        };
        assert_eq!(channel().validate(), Ok(()));
        for mutate in [
            |c: &mut RelayChannel| c.name = "Relay".into(),
            |c: &mut RelayChannel| c.name = "a b".into(),
            |c: &mut RelayChannel| c.name = String::new(),
            |c: &mut RelayChannel| c.base_url = "ftp://x".into(),
            |c: &mut RelayChannel| c.base_url = "http://".into(),
            |c: &mut RelayChannel| c.base_url = "http://user@host".into(),
            |c: &mut RelayChannel| c.base_url = "http://host/path".into(),
            |c: &mut RelayChannel| c.base_url = "http://host?x=1".into(),
            |c: &mut RelayChannel| c.base_url = "http://ho st".into(),
            |c: &mut RelayChannel| c.token = String::new(),
            |c: &mut RelayChannel| c.token = "a b".into(),
            |c: &mut RelayChannel| c.token = "a\"b".into(),
            |c: &mut RelayChannel| c.token = "a\\b".into(),
            |c: &mut RelayChannel| c.token = "a\nb".into(),
            |c: &mut RelayChannel| c.token = "€".into(),
        ] {
            let mut bad_channel = channel();
            mutate(&mut bad_channel);
            assert_eq!(bad_channel.validate(), Err(bad), "{bad_channel:?}");
        }
    }

    struct Revoke(Arc<AtomicUsize>);

    impl Drop for Revoke {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_the_grant_revokes_exactly_once() {
        let count = Arc::new(AtomicUsize::new(0));
        let grant = RelayGrant::new(channel(), Revoke(count.clone()));
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(grant);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
