//! 호스트 등록부의 `gateway` 절(BOT_HOST_CONFIG JSON). 프로필이 쓰는 별칭을 실제 워크스페이스·채널·OS 사용자·
//! 인증된 HTTP 입구에 연결하고, 어댑터 자격의 **이름**(값이 아니라)과 속도·본문 한도를 정한다.
//! 모르는 필드는 거절하고, 오류에는 필드 경로만 싣고 값은 싣지 않는다.
use crate::{identity::is_alias, limits::Limits};
use bot_core::id::SecretName;
use serde::Deserialize;
use std::{collections::BTreeMap, fmt, net::SocketAddr};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default)]
    pub cli: CliHost,
    #[serde(default)]
    pub slack: SlackHost,
    #[serde(default)]
    pub http: HttpHost,
    #[serde(default)]
    pub limits: LimitsHost,
    /// 승인 카드를 Slack에 그대로 담을 수 없을 때 안내할 인증된 관리 화면 주소(선택)
    pub admin_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliHost {
    /// 생략하면 `default_socket()`
    pub socket: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, CliAccount>,
}

/// 별칭이 가리키는 OS 사용자. `os_user`와 `uid` 중 정확히 하나.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliAccount {
    pub os_user: Option<String>,
    pub uid: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackHost {
    #[serde(default)]
    pub workspaces: BTreeMap<String, SlackWorkspace>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Receive {
    /// 앱 토큰으로 Socket Mode 세션을 연다(공개 주소가 필요 없다).
    #[default]
    Socket,
    /// Events API·버튼을 이 호스트의 HTTP 주소로 받고 서명과 시각을 확인한다.
    Http,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackWorkspace {
    pub team_id: String,
    /// 있으면 소켓 hello가 이 앱이 아닐 때 멈춘다.
    pub app_id: Option<String>,
    /// 자격 이름. 값은 바인딩 소유 프로필의 `bot/<profile>/<name>`에서 읽는다.
    pub bot_token: String,
    pub app_token: Option<String>,
    pub signing_secret: Option<String>,
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    /// 이 워크스페이스에서 봇과의 1:1 DM 입구(`dm: true` 바인딩)를 연다. 운영자가 명시해야 한다.
    #[serde(default)]
    pub dm: bool,
    #[serde(default)]
    pub receive: Receive,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpHost {
    /// 생략하면 `127.0.0.1:8788`(관리 화면의 기본 `127.0.0.1:8787`과 겹치지 않는다)
    pub listen: Option<String>,
    /// 루프백이 아닌 주소(`listen`)를 열어도 된다는 운영자의 명시(관리 화면의 `remote`와 같은 뜻). 이 입구는 TLS 없는 평문 HTTP라서 베어러 토큰이 그대로 오간다 —
    /// 켜려면 TLS를 맡는 프록시·사설망 뒤인지 먼저 확인한다. 생략하면 루프백만 열린다.
    #[serde(default)]
    pub remote: bool,
    #[serde(default)]
    pub endpoints: BTreeMap<String, HttpEndpoint>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpEndpoint {
    pub issuer: String,
    pub audience: String,
    /// HS256 키의 자격 이름
    pub key: String,
    /// sync 응답을 기다리는 최대 시간(ms). 생략하면 10초, 넘으면 202로 바꾼다.
    pub sync_wait_ms: Option<u64>,
    /// 있으면 토큰의 남은 수명(exp − 지금)이 이보다 길 때 거절한다.
    pub max_token_age_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsHost {
    pub body_bytes: Option<usize>,
    pub attachments: Option<usize>,
    pub per_entrance_per_min: Option<u32>,
    pub per_principal_per_min: Option<u32>,
}

pub const DEFAULT_LISTEN: &str = "127.0.0.1:8788";
pub const DEFAULT_SYNC_WAIT_MS: u64 = 10_000;

#[derive(Debug, Eq, PartialEq)]
pub struct HostError(pub String);

impl fmt::Display for HostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostError {}

fn bad(path: &str, why: &str) -> HostError {
    HostError(format!("gateway.{path}: {why}"))
}

/// Slack ID(`T…`·`C…`·`A…`): 대문자·숫자
pub(crate) fn is_slack_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && text
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

fn is_os_user(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 32
        && !text.starts_with('-')
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn secret(path: &str, name: &str) -> Result<(), HostError> {
    SecretName::parse(name)
        .map(|_| ())
        .ok_or_else(|| bad(path, "자격 이름은 63자 이내 소문자 kebab-case여야 해요"))
}

impl HostConfig {
    /// `gateway` 절의 JSON 값을 읽고 검사한다.
    pub fn from_value(value: serde_json::Value) -> Result<Self, HostError> {
        let config: HostConfig = serde_json::from_value(value)
            .map_err(|e| HostError(format!("gateway 절을 읽지 못했어요: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_json(text: &str) -> Result<Self, HostError> {
        let value = serde_json::from_str(text)
            .map_err(|e| HostError(format!("gateway 절을 읽지 못했어요: {e}")))?;
        Self::from_value(value)
    }

    pub fn validate(&self) -> Result<(), HostError> {
        for (alias, account) in &self.cli.accounts {
            let path = format!("cli.accounts.{alias}");
            if !is_alias(alias) {
                return Err(bad(&path, "별칭은 63자 이내 소문자 kebab-case여야 해요"));
            }
            match (&account.os_user, account.uid) {
                (Some(user), None) if is_os_user(user) => {}
                (None, Some(_)) => {}
                (Some(_), None) => return Err(bad(&path, "os_user 모양이 올바르지 않아요")),
                _ => return Err(bad(&path, "os_user와 uid 중 정확히 하나를 적어야 해요")),
            }
        }
        if let Some(socket) = &self.cli.socket
            && (!socket.starts_with('/') || socket.len() > 100 || socket.contains('\0'))
        {
            return Err(bad(
                "cli.socket",
                "절대 경로이고 100바이트 이하여야 해요(유닉스 소켓 주소 한도)",
            ));
        }
        let mut teams: BTreeMap<&str, &str> = BTreeMap::new();
        for (alias, ws) in &self.slack.workspaces {
            let path = format!("slack.workspaces.{alias}");
            if !is_alias(alias) {
                return Err(bad(&path, "별칭은 63자 이내 소문자 kebab-case여야 해요"));
            }
            if !is_slack_id(&ws.team_id) {
                return Err(bad(&path, "team_id 모양이 올바르지 않아요"));
            }
            if ws.app_id.as_deref().is_some_and(|id| !is_slack_id(id)) {
                return Err(bad(&path, "app_id 모양이 올바르지 않아요"));
            }
            if let Some(first) = teams.insert(&ws.team_id, alias) {
                return Err(bad(
                    &path,
                    &format!("같은 team_id를 {first}도 써요(주체 별칭이 모호해져요)"),
                ));
            }
            secret(&format!("{path}.bot_token"), &ws.bot_token)?;
            for (field, name) in [
                ("app_token", &ws.app_token),
                ("signing_secret", &ws.signing_secret),
            ] {
                if let Some(name) = name {
                    secret(&format!("{path}.{field}"), name)?;
                }
            }
            match ws.receive {
                Receive::Socket if ws.app_token.is_none() => {
                    return Err(bad(&path, "receive=socket이면 app_token이 필요해요"));
                }
                Receive::Socket => {}
                // HTTP 수신기(Events API·버튼 요청, 서명·재전송 검사)가 아직 없다: 서명 비밀을 줘도 메시지·승인 버튼을 받지 못하므로 조용히 건너뛰지 않고 이름을 밝혀 거절한다
                Receive::Http => {
                    return Err(bad(
                        &format!("{path}.receive"),
                        "http 수신은 아직 지원하지 않아요 — 이 워크스페이스는 socket(app_token)으로만 받을 수 있어요",
                    ));
                }
            }
            for (channel_alias, id) in &ws.channels {
                let channel_path = format!("{path}.channels.{channel_alias}");
                if !is_alias(channel_alias) || !is_slack_id(id) {
                    return Err(bad(
                        &channel_path,
                        "별칭 또는 채널 ID 모양이 올바르지 않아요",
                    ));
                }
                if id.starts_with('D') {
                    return Err(bad(
                        &channel_path,
                        "1:1 DM 채널은 채널이 아니라 dm 바인딩으로 받아요",
                    ));
                }
            }
        }
        if let Some(url) = &self.admin_url
            && (!(url.starts_with("https://") || url.starts_with("http://"))
                || url.len() > 200
                || url.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(bad("admin_url", "http(s) 주소여야 하고 공백이 없어야 해요"));
        }
        if let Some(listen) = &self.http.listen {
            let Ok(addr) = listen.parse::<SocketAddr>() else {
                return Err(bad("http.listen", "host:port 모양이어야 해요"));
            };
            // 평문 HTTP 입구를 루프백 밖에 여는 것은 운영자가 명시해야 한다(`::ffff:127.0.0.1` 같은 매핑 주소는 본래 주소로 본다)
            if !self.http.remote && !addr.ip().to_canonical().is_loopback() {
                return Err(bad(
                    "http.listen",
                    "루프백이 아닌 주소는 TLS 없이 열 수 없어요 — TLS를 맡는 프록시나 보호된 망 뒤에서 정말 필요하면 http.remote를 true로 명시해 주세요",
                ));
            }
        }
        for (alias, endpoint) in &self.http.endpoints {
            let path = format!("http.endpoints.{alias}");
            if !is_alias(alias) {
                return Err(bad(&path, "별칭은 63자 이내 소문자 kebab-case여야 해요"));
            }
            if alias.starts_with('_') {
                return Err(bad(&path, "예약된 이름이에요"));
            }
            for (field, text) in [
                ("issuer", &endpoint.issuer),
                ("audience", &endpoint.audience),
            ] {
                if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
                    return Err(bad(&path, &format!("{field}가 비었거나 너무 길어요")));
                }
            }
            secret(&format!("{path}.key"), &endpoint.key)?;
            if endpoint
                .sync_wait_ms
                .is_some_and(|ms| ms == 0 || ms > 60_000)
            {
                return Err(bad(&path, "sync_wait_ms는 1~60000이어야 해요"));
            }
            if endpoint.max_token_age_secs == Some(0) {
                return Err(bad(&path, "max_token_age_secs는 1 이상이어야 해요"));
            }
        }
        self.limits().validate().map_err(|why| bad("limits", why))
    }

    /// 한도 기본값 위에 호스트가 정한 값을 얹는다.
    pub fn limits(&self) -> Limits {
        let base = Limits::default();
        Limits {
            body_bytes: self.limits.body_bytes.unwrap_or(base.body_bytes),
            attachments: self.limits.attachments.unwrap_or(base.attachments),
            per_entrance_per_min: self
                .limits
                .per_entrance_per_min
                .unwrap_or(base.per_entrance_per_min),
            per_principal_per_min: self
                .limits
                .per_principal_per_min
                .unwrap_or(base.per_principal_per_min),
        }
    }

    pub fn listen(&self) -> SocketAddr {
        self.http
            .listen
            .as_deref()
            .unwrap_or(DEFAULT_LISTEN)
            .parse()
            .expect("검사를 통과한 주소")
    }

    /// Slack team_id → (워크스페이스 별칭, 설정)
    pub fn workspace_by_team(&self, team: &str) -> Option<(&str, &SlackWorkspace)> {
        self.slack
            .workspaces
            .iter()
            .find(|(_, ws)| ws.team_id == team)
            .map(|(alias, ws)| (alias.as_str(), ws))
    }
}

/// 별칭이 가리키는 OS 사용자의 uid. `os_user`는 `/usr/bin/id -u`로 푼다(셸을 거치지 않고, 옵션으로 읽힐 수 없는 이름만 통과한다).
pub fn resolve_uid(account: &CliAccount) -> Result<u32, HostError> {
    if let Some(uid) = account.uid {
        return Ok(uid);
    }
    let user = account
        .os_user
        .as_deref()
        .filter(|user| is_os_user(user))
        .ok_or_else(|| HostError("OS 사용자 이름이 올바르지 않아요".into()))?;
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .arg(user)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| HostError("OS 사용자를 확인하지 못했어요(/usr/bin/id 실행 실패)".into()))?;
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|_| output.status.success())
        .ok_or_else(|| HostError("호스트에 없는 OS 사용자예요".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "cli": {"accounts": {"owner": {"uid": 501}}},
        "slack": {"workspaces": {"home": {
            "team_id": "T0AAAAAAA", "app_id": "A0BBBBBBB",
            "bot_token": "slack-bot-token", "app_token": "slack-app-token",
            "channels": {"planning": "C0CCCCCCC"}}}},
        "http": {"listen": "127.0.0.1:0", "endpoints": {"reminders": {
            "issuer": "https://issuer.example", "audience": "bot", "key": "reminders-key"}}},
        "limits": {"per_principal_per_min": 5}
    }"#;

    #[test]
    fn reads_a_full_section_and_layers_limits_over_defaults() {
        let host = HostConfig::from_json(SAMPLE).unwrap();
        assert_eq!(host.workspace_by_team("T0AAAAAAA").unwrap().0, "home");
        assert_eq!(host.limits().per_principal_per_min, 5);
        assert_eq!(host.limits().body_bytes, Limits::default().body_bytes);
        assert_eq!(host.listen().port(), 0);
        assert!(HostConfig::from_json("{}").is_ok());
    }

    fn rejects(json: &str) -> String {
        HostConfig::from_json(json).unwrap_err().to_string()
    }

    /// 평문 HTTP 입구는 루프백에서만 열린다. 루프백 밖은 운영자가 `http.remote`로 명시해야 하고, 거절은 필드 경로(`gateway.http.listen`)와 고치는 법을 밝힌다.
    #[test]
    fn a_plain_http_entrance_opens_beyond_loopback_only_when_the_operator_says_so() {
        // 생략하면 기본 주소(루프백)라 그대로 열린다
        assert!(HostConfig::from_json("{}").is_ok());
        for local in [
            "127.0.0.1:8788",
            "127.0.0.1:0",
            "127.8.9.10:8788",
            "[::1]:8788",
            "[::ffff:127.0.0.1]:8788",
        ] {
            let json = format!(r#"{{"http":{{"listen":"{local}"}}}}"#);
            assert!(HostConfig::from_json(&json).is_ok(), "{local}");
        }
        for beyond in [
            "0.0.0.0:8788",
            "[::]:8788",
            "192.168.0.10:8788",
            "10.0.0.5:8788",
            "203.0.113.7:8788",
            "[2001:db8::1]:8788",
            "[::ffff:10.0.0.5]:8788",
            "[::ffff:0.0.0.0]:8788",
        ] {
            let json = format!(r#"{{"http":{{"listen":"{beyond}"}}}}"#);
            let message = rejects(&json);
            assert!(
                message.starts_with("gateway.http.listen: ") && message.contains("http.remote"),
                "{beyond} → {message}"
            );
            // 값(주소)은 오류에 싣지 않는다
            let ip = beyond.parse::<SocketAddr>().unwrap().ip().to_string();
            assert!(!message.contains(&ip), "{message}");
            // 명시하면 열린다
            let json = format!(r#"{{"http":{{"listen":"{beyond}","remote":true}}}}"#);
            assert_eq!(
                HostConfig::from_json(&json).unwrap().listen().to_string(),
                beyond,
                "{beyond}"
            );
        }
        // `remote: false`를 적어도 같다. 켜기만 하고 주소를 생략하면 기본 루프백 그대로다
        assert!(
            rejects(r#"{"http":{"listen":"0.0.0.0:8788","remote":false}}"#).contains("http.remote")
        );
        let host = HostConfig::from_json(r#"{"http":{"remote":true}}"#).unwrap();
        assert_eq!(host.listen().to_string(), DEFAULT_LISTEN);
        // 불리언이 아니면 읽지 못한다(문자열 «yes»를 참으로 받지 않는다)
        assert!(
            HostConfig::from_json(r#"{"http":{"listen":"0.0.0.0:8788","remote":"yes"}}"#).is_err()
        );
    }

    #[test]
    fn rejects_unknown_fields_and_bad_shapes_without_echoing_values() {
        assert!(rejects(r#"{"slak": {}}"#).contains("slak"));
        let long_secret = "x".repeat(40);
        let message = rejects(&format!(
            r#"{{"slack":{{"workspaces":{{"home":{{"team_id":"T1","bot_token":"Bad Token {long_secret}","app_token":"a"}}}}}}}}"#
        ));
        assert!(
            message.contains("slack.workspaces.home.bot_token"),
            "{message}"
        );
        assert!(!message.contains(&long_secret));
        for (json, needle) in [
            (r#"{"cli":{"accounts":{"owner":{}}}}"#, "정확히 하나"),
            (
                r#"{"cli":{"accounts":{"owner":{"uid":1,"os_user":"x"}}}}"#,
                "정확히 하나",
            ),
            (r#"{"cli":{"accounts":{"Owner":{"uid":1}}}}"#, "별칭"),
            (r#"{"cli":{"socket":"relative.sock"}}"#, "절대 경로"),
            (r#"{"http":{"listen":"nope"}}"#, "host:port"),
            (
                r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b"}}}}"#,
                "app_token이 필요",
            ),
            (
                r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","receive":"http"}}}}"#,
                "slack.workspaces.home.receive: http 수신은 아직 지원하지 않아요",
            ),
            // 서명 비밀을 줘도 같다(받는 곳이 없으니 조용히 건너뛰지 않는다)
            (
                r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","app_token":"a","signing_secret":"s","receive":"http"}}}}"#,
                "slack.workspaces.home.receive: http 수신은 아직 지원하지 않아요",
            ),
            (
                r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","app_token":"a","channels":{"dm":"D123"}}}}}"#,
                "dm 바인딩",
            ),
            (r#"{"limits":{"body_bytes":0}}"#, "본문 한도"),
            (
                r#"{"http":{"endpoints":{"_requests":{"issuer":"i","audience":"a","key":"k"}}}}"#,
                "별칭",
            ),
            (
                r#"{"http":{"endpoints":{"x":{"issuer":"i","audience":"a","key":"k","sync_wait_ms":0}}}}"#,
                "sync_wait_ms",
            ),
        ] {
            let message = rejects(json);
            assert!(message.contains(needle), "{json} → {message}");
        }
    }

    #[test]
    fn two_aliases_cannot_share_one_team() {
        let message = rejects(
            r#"{"slack":{"workspaces":{
                "a":{"team_id":"T1","bot_token":"b","app_token":"t"},
                "b":{"team_id":"T1","bot_token":"b","app_token":"t"}}}}"#,
        );
        assert!(message.contains("같은 team_id"), "{message}");
    }
}
