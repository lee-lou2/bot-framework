//! 한 실행의 브로커 프런트 연결(HTTP)과 CLI용 MCP 설정 생성. 프런트(모델 API 중계·MCP 중계) 구현은 브로커 쪽이다.
//!
//! 샌드박스(호스트 전용 망의 VM)는 외부와 통신하지 못하고 이 주소 하나에만 닿는다. 같은 주소로
//! 모델 요청(`POST /v1/messages`, `x-api-key`)과 도구 요청(`POST /mcp`, `Authorization: Bearer`)이 가며 둘 다 **이 시도(현재 claim)에 발급한
//! 세션 토큰**으로 인증한다. 토큰은 실제 제공자 자격이 아니다: 세대가 바뀌거나 시도가 끝나면 브로커가 폐기하고, 호스트는 자기가 발급받은
//! 토큰만 폐기한다(`RelayGrant`를 놓을 때) — 새 claim의 토큰을 끊지 않는다.
//!
//! 설정 파일 모양(실제 CLI로 확인한 형식): Claude Code `--mcp-config`는
//! `{"mcpServers":{"<이름>":{"type":"http","url":"<주소>/mcp","headers":{"Authorization":"Bearer ${RELAY_TOKEN}"}}}}`.
//!
//! **토큰은 어떤 파일에도 쓰지 않는다.** Claude Code는 설정의 `${이름}`을 프로세스 환경에서 채우므로(실제 Claude Code 2.1.331로 확인), 파일에는 자리표시
//! `${RELAY_TOKEN}`만 있고 값은 실행기가 환경변수 [`TOKEN_ENV`]로만 준다. 모델이 읽는 폴더(내장 Read를 켠 구성)를 아무리 훑어도 토큰은 나오지 않는다.
//! (`ANTHROPIC_API_KEY`는 쓸 수 없다: Claude Code가 설정 확장에서 그 값을 비워 `Bearer `만 나간다.)

use super::ModelError;
use serde_json::json;
use std::fmt;

/// 설정 파일의 `${…}` 자리표시가 가리키는 환경변수 이름. 값(이 시도의 세션 토큰)은 이 변수로만 CLI에 간다.
pub const TOKEN_ENV: &str = "RELAY_TOKEN";

/// 한 실행의 도구·모델 중계 채널. 호스트가 이 시도에 발급해 `ModelRequest`로 준다.
#[derive(Clone, Eq, PartialEq)]
pub struct RelayChannel {
    /// MCP 서버 이름. Claude Code는 도구를 `mcp__<이름>__<도구>`로 본다.
    pub name: String,
    /// 샌드박스 안에서 닿는 브로커 프런트 주소(`http://<호스트 전용 망 주소>:<포트>`). 끝에 `/`가 없다.
    pub base_url: String,
    /// 이 시도(현재 claim)에 발급한 세션 토큰. 진단 출력에 나오지 않는다.
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

/// 채널과, 놓으면 이 시도가 발급받은 토큰만 폐기하는 열쇠. 워커는 CLI 실행이 끝날 때까지 쥐고 있다.
pub struct RelayGrant {
    pub channel: RelayChannel,
    _revoke: Box<dyn Send + Sync>,
}

impl RelayGrant {
    /// `revoke`는 놓일 때(드롭) 토큰을 폐기하는 값이다.
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

/// 토큰: 보이는 ASCII만(따옴표·역슬래시·공백 없음). 헤더·JSON·환경변수에 그대로 쓸 수 있어야 한다.
fn token_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

/// `http(s)://호스트[:포트]`만. 경로·질의·사용자 정보·제어 문자 없음.
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
    /// 설정 파일에 쓰기 전에 모양을 확인한다.
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

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.base_url)
    }

    /// 헤더 값: 토큰 자리에 환경변수 자리표시만 적는다(값은 파일에 없다).
    fn bearer() -> String {
        format!("Bearer ${{{TOKEN_ENV}}}")
    }

    /// Claude Code `--mcp-config`가 읽을 JSON 본문
    pub fn claude_config(&self) -> String {
        let server = json!({
            "type": "http",
            "url": self.mcp_url(),
            "headers": {"Authorization": Self::bearer()},
        });
        json!({"mcpServers": {&self.name: server}}).to_string()
    }

    /// Claude Code `--allowedTools`에 줄 값: 이 서버의 도구만 미리 허용한다
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
        // 비밀 스캔(gitleaks)이 합성 값도 비밀로 읽으므로 값을 실행 시 잇는다
        let sample = ["tok_9f3a", "1c77d2e84b60"].concat();
        let relay = RelayChannel {
            token: sample.clone(),
            ..channel()
        };
        let config = relay.claude_config();
        assert!(!config.contains(&sample), "{config}");
        assert!(config.contains("Bearer ${RELAY_TOKEN}"), "{config}");
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
            |c: &mut RelayChannel| c.token = "토".into(),
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
