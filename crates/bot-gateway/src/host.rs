//! Host registry `gateway` section parsing and validation without echoing secret values.
use crate::{identity::is_alias, limits::Limits};
use bot_core::id::SecretName;
use serde::Deserialize;
use std::{collections::BTreeMap, fmt, net::SocketAddr};

mod openai;
pub use openai::{
    DEFAULT_BODY_BYTES as DEFAULT_OPENAI_BODY_BYTES, DEFAULT_MAX_ACTIVE_RUNS, DEFAULT_WAIT_MS,
    KeySlot, MAX_ACTIVE_RUNS, MAX_KEY_SLOTS, MAX_WAIT_MS, OpenAiEndpoint,
    key_shaped as openai_key_shaped,
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    #[serde(default)]
    pub cli: CliHost,
    #[serde(default)]
    pub slack: SlackHost,
    #[serde(default)]
    pub telegram: TelegramHost,
    #[serde(default)]
    pub http: HttpHost,
    #[serde(default)]
    pub limits: LimitsHost,
    /// Optional authenticated Admin UI URL linked when an approval card cannot fit in Slack.
    pub admin_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliHost {
    pub socket: Option<String>,
    #[serde(default)]
    pub accounts: BTreeMap<String, CliAccount>,
}

/// OS user mapping for a CLI alias; exactly one of `os_user` or `uid` must be set.
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
    /// Optional single-line channel notice replacing the default `DM_SENT_NOTICE` copy when an approval card is delivered via DM.
    pub dm_sent_notice: Option<String>,
    /// Optional trailing footnote line prefix (e.g. `"※"`) split into a Slack `context` block; disabled when unset.
    pub note_prefix: Option<String>,
}

/// Kept private because `validate` rejects `http`; all validated workspaces use Socket Mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Receive {
    #[default]
    Socket,
    Http,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackWorkspace {
    pub team_id: String,
    pub app_id: Option<String>,
    /// Secret name resolved under `bot/<profile>/<name>` for the binding-owning profile.
    pub bot_token: String,
    pub app_token: Option<String>,
    /// Secret name retained for activation fingerprint stability and future HTTP ingress support.
    pub signing_secret: Option<String>,
    #[serde(default)]
    pub channels: BTreeMap<String, String>,
    /// Explicit opt-in enabling 1:1 DM bindings (`dm: true`) for this workspace.
    #[serde(default)]
    pub dm: bool,
    #[serde(default)]
    receive: Receive,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramHost {
    #[serde(default)]
    pub bots: BTreeMap<String, TelegramBot>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramBot {
    /// Positive numeric Telegram bot user ID verified against `getMe` on startup.
    pub bot_id: u64,
    /// Secret name resolved under `bot/<profile>/<name>` for the binding-owning profile.
    pub bot_token: String,
    /// Explicit opt-in enabling 1:1 DM bindings (`dm: true`) for this bot.
    #[serde(default)]
    pub dm: bool,
}

/// Deserialized by `host/openai.rs`, splitting `endpoints` by `kind` into `endpoints` (webhook) and `openai`.
#[derive(Clone, Debug, Default)]
pub struct HttpHost {
    /// Defaults to `127.0.0.1:8788` (distinct from Admin UI's default `127.0.0.1:8787`).
    pub listen: Option<String>,
    /// Explicit opt-in to bind non-loopback addresses over plaintext HTTP (use only behind TLS/private network).
    pub remote: bool,
    pub endpoints: BTreeMap<String, HttpEndpoint>,
    pub openai: BTreeMap<String, OpenAiEndpoint>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpEndpoint {
    pub issuer: String,
    pub audience: String,
    /// HS256 secret name.
    pub key: String,
    pub sync_wait_ms: Option<u64>,
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
    SecretName::parse(name).map(|_| ()).ok_or_else(|| {
        bad(
            path,
            "secret name must be lowercase kebab-case within 63 chars",
        )
    })
}

impl HostConfig {
    pub fn from_value(value: serde_json::Value) -> Result<Self, HostError> {
        let config: HostConfig = serde_json::from_value(value)
            .map_err(|e| HostError(format!("failed to parse gateway section: {e}")))?;
        config.validate()?;
        Ok(config)
    }

    pub fn from_json(text: &str) -> Result<Self, HostError> {
        let value = serde_json::from_str(text)
            .map_err(|e| HostError(format!("failed to parse gateway section: {e}")))?;
        Self::from_value(value)
    }

    pub fn validate(&self) -> Result<(), HostError> {
        for (alias, account) in &self.cli.accounts {
            let path = format!("cli.accounts.{alias}");
            if !is_alias(alias) {
                return Err(bad(
                    &path,
                    "alias must be lowercase kebab-case within 63 chars",
                ));
            }
            match (&account.os_user, account.uid) {
                (Some(user), None) if is_os_user(user) => {}
                (None, Some(_)) => {}
                (Some(_), None) => return Err(bad(&path, "invalid os_user format")),
                _ => return Err(bad(&path, "must specify exactly one of os_user or uid")),
            }
        }
        if let Some(socket) = &self.cli.socket
            && (!socket.starts_with('/') || socket.len() > 100 || socket.contains('\0'))
        {
            return Err(bad(
                "cli.socket",
                "must be an absolute path of at most 100 bytes (Unix socket limit)",
            ));
        }
        let mut teams: BTreeMap<&str, &str> = BTreeMap::new();
        for (alias, ws) in &self.slack.workspaces {
            let path = format!("slack.workspaces.{alias}");
            if !is_alias(alias) {
                return Err(bad(
                    &path,
                    "alias must be lowercase kebab-case within 63 chars",
                ));
            }
            if !is_slack_id(&ws.team_id) {
                return Err(bad(&path, "invalid team_id format"));
            }
            if ws.app_id.as_deref().is_some_and(|id| !is_slack_id(id)) {
                return Err(bad(&path, "invalid app_id format"));
            }
            if let Some(first) = teams.insert(&ws.team_id, alias) {
                return Err(bad(
                    &path,
                    &format!("duplicate team_id also used by {first} (ambiguous principal alias)"),
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
                    return Err(bad(&path, "app_token is required when receive=socket"));
                }
                Receive::Socket => {}
                // Reject explicitly rather than silently ignoring because no Slack HTTP receiver exists.
                Receive::Http => {
                    return Err(bad(
                        &format!("{path}.receive"),
                        "http receive is not supported yet — this workspace must use socket (app_token)",
                    ));
                }
            }
            for (channel_alias, id) in &ws.channels {
                let channel_path = format!("{path}.channels.{channel_alias}");
                if !is_alias(channel_alias) || !is_slack_id(id) {
                    return Err(bad(&channel_path, "invalid alias or channel ID format"));
                }
                if id.starts_with('D') {
                    return Err(bad(
                        &channel_path,
                        "1:1 DM channels must use a dm binding, not a channel entry",
                    ));
                }
            }
        }
        if let Some(notice) = &self.slack.dm_sent_notice
            && (notice.trim().is_empty()
                || notice.chars().count() > 200
                || notice.chars().any(char::is_control))
        {
            return Err(bad(
                "slack.dm_sent_notice",
                "must be a non-empty single-line string of at most 200 chars without control characters",
            ));
        }
        if let Some(prefix) = &self.slack.note_prefix {
            let first = prefix.chars().next();
            let bad_start =
                first.is_none_or(|c| matches!(c, '-' | '*' | '>' | '#') || c.is_ascii_digit());
            if bad_start
                || prefix.chars().count() > 16
                || prefix.chars().any(|c| c.is_whitespace() || c.is_control())
            {
                return Err(bad(
                    "slack.note_prefix",
                    "must be 1..=16 non-whitespace printable chars and cannot start with '-', '*', '>', '#', or an ASCII digit",
                ));
            }
        }
        let mut telegram_ids: BTreeMap<u64, &str> = BTreeMap::new();
        for (alias, bot) in &self.telegram.bots {
            let path = format!("telegram.bots.{alias}");
            if !is_alias(alias) {
                return Err(bad(
                    &path,
                    "alias must be lowercase kebab-case within 63 chars",
                ));
            }
            if bot.bot_id == 0 {
                return Err(bad(&path, "bot_id must be a positive Telegram bot user ID"));
            }
            if let Some(first) = telegram_ids.insert(bot.bot_id, alias) {
                return Err(bad(
                    &path,
                    &format!("duplicate bot_id also used by {first} (ambiguous bot alias)"),
                ));
            }
            secret(&format!("{path}.bot_token"), &bot.bot_token)?;
        }
        if let Some(url) = &self.admin_url
            && (!(url.starts_with("https://") || url.starts_with("http://"))
                || url.len() > 200
                || url.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(bad(
                "admin_url",
                "must be an http(s) URL without whitespace or control characters",
            ));
        }
        if let Some(listen) = &self.http.listen {
            let Ok(addr) = listen.parse::<SocketAddr>() else {
                return Err(bad("http.listen", "must be in host:port format"));
            };
            // Canonicalize IPv4-mapped IPv6 addresses such as `::ffff:127.0.0.1`.
            if !self.http.remote && !addr.ip().to_canonical().is_loopback() {
                return Err(bad(
                    "http.listen",
                    "cannot bind non-loopback address without TLS — set http.remote to true only behind a TLS proxy or private network",
                ));
            }
        }
        for (alias, endpoint) in &self.http.endpoints {
            let path = format!("http.endpoints.{alias}");
            if !is_alias(alias) {
                return Err(bad(
                    &path,
                    "alias must be lowercase kebab-case within 63 chars",
                ));
            }
            if alias.starts_with('_') {
                return Err(bad(&path, "reserved name"));
            }
            for (field, text) in [
                ("issuer", &endpoint.issuer),
                ("audience", &endpoint.audience),
            ] {
                if text.is_empty() || text.len() > 256 || text.chars().any(char::is_control) {
                    return Err(bad(
                        &path,
                        &format!("{field} must be non-empty and at most 256 chars"),
                    ));
                }
            }
            secret(&format!("{path}.key"), &endpoint.key)?;
            if endpoint
                .sync_wait_ms
                .is_some_and(|ms| ms == 0 || ms > 60_000)
            {
                return Err(bad(&path, "sync_wait_ms must be between 1 and 60000"));
            }
            if endpoint.max_token_age_secs == Some(0) {
                return Err(bad(&path, "max_token_age_secs must be at least 1"));
            }
        }
        openai::validate(&self.http)?;
        self.limits().validate().map_err(|why| bad("limits", why))
    }

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

    /// Uses explicit `limits.body_bytes` if configured (up to 1 MiB); otherwise defaults to 256 KiB.
    pub fn openai_body_bytes(&self) -> usize {
        self.limits.body_bytes.unwrap_or(openai::DEFAULT_BODY_BYTES)
    }

    pub fn listen(&self) -> SocketAddr {
        self.http
            .listen
            .as_deref()
            .unwrap_or(DEFAULT_LISTEN)
            .parse()
            .expect("validated listen address")
    }

    pub fn workspace_by_team(&self, team: &str) -> Option<(&str, &SlackWorkspace)> {
        self.slack
            .workspaces
            .iter()
            .find(|(_, ws)| ws.team_id == team)
            .map(|(alias, ws)| (alias.as_str(), ws))
    }
}

/// Resolves `os_user` directly via `/usr/bin/id -u` without a shell, rejecting leading `-` names.
pub fn resolve_uid(account: &CliAccount) -> Result<u32, HostError> {
    if let Some(uid) = account.uid {
        return Ok(uid);
    }
    let user = account
        .os_user
        .as_deref()
        .filter(|user| is_os_user(user))
        .ok_or_else(|| HostError("invalid OS user name".into()))?;
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .arg(user)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|_| HostError("failed to resolve OS user (/usr/bin/id failed)".into()))?;
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|_| output.status.success())
        .ok_or_else(|| HostError("OS user does not exist on host".into()))
}

#[cfg(test)]
mod tests;
