//! 호스트 등록부(BOT_HOST_CONFIG, JSON). 운영자가 프로필 밖에서 정하는 것: 설치한 도구 서버·프로필별 허용 모델·egress 상한,
//! 그리고 입구(CLI·Slack·HTTP)의 실제 대상과 자격 이름·한도(`gateway` 절, bot-gateway가 해석).
//!
//! ```json
//! {"version": 1,
//!  "gateway": {"cli": {"accounts": {"owner": {"os_user": "me"}}}},
//!  "servers": {"agenda": {"root": "/opt/bot/servers/agenda", "command": ["bin/server", "--stdio"]}},
//!  "models": {"personal": ["assistant"]},
//!  "providers": {"assistant": {"kind": "demo"}},
//!  "egress": {"personal": ["calendar.example:443"]},
//!  "sandbox": {"srt": {"binary": "/opt/srt/bin/srt", "work_root": "/var/bot/work"}},
//!  "admin": {"bind": "127.0.0.1:8787"}}
//! ```
//! `admin`은 관리 화면 설정(`bot_admin::AdminSettings`, docs/admin.md)이고 생략해도 로컬 기본값으로 동작한다.
//! `providers`(모델 별칭의 실행 방법)·`sandbox`(srt·VM)는 모양만 검사한다 — 실제로 필요한 것은 런타임 역할이다(`execution`).
//! 모르는 최상위 키는 거절한다. 파일이 없으면 내장 데모 호스트(`demo`)를 쓴다.
use crate::{
    execution::{self, Provider, Sandbox},
    install::{ServerSpec, Servers, parse_all},
};
use bot_admin::AdminSettings;
use bot_broker::mcp::remote::Remote;
use bot_gateway::host::{CliAccount, HostConfig};
use bot_profile::{HostCatalog, SlackWorkspace};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Eq, PartialEq)]
pub struct HostFileError(pub String);

impl fmt::Display for HostFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostFileError {}

fn bad(message: impl Into<String>) -> HostFileError {
    HostFileError(message.into())
}

/// 해석한 호스트 등록부
pub struct HostFile {
    pub gateway: HostConfig,
    pub catalog: HostCatalog,
    /// 관리 화면 설정(비밀 값은 담지 않는다)
    pub admin: AdminSettings,
    /// 도구 서버 설치 명세(별칭 → 설치 폴더·실행 명령). 내장 데모 호스트는 `attach_demo_server`가 채운다.
    pub servers: BTreeMap<String, ServerSpec>,
    /// 원격(Streamable HTTP) 도구 서버(별칭 → 접속 대상). 설치할 파일이 없고 브로커가 직접 연결한다.
    pub remote_servers: BTreeMap<String, Remote>,
    /// 모델 별칭 → 실행 방법
    pub providers: BTreeMap<String, Provider>,
    /// srt·VM 설정(런타임 역할이 쓴다)
    pub sandbox: Sandbox,
    /// 파일 없이 내장 데모로 만든 것인가
    pub demo: bool,
}

fn name_list(value: &Value, path: &str) -> Result<BTreeSet<String>, HostFileError> {
    let Value::Array(items) = value else {
        return Err(bad(format!("{path}: 문자열 배열이어야 해요")));
    };
    items
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| bad(format!("{path}: 문자열 배열이어야 해요")))
        })
        .collect()
}

fn profile_map(
    value: &Value,
    path: &str,
    check: fn(&str) -> bool,
) -> Result<BTreeMap<String, BTreeSet<String>>, HostFileError> {
    let Value::Object(map) = value else {
        return Err(bad(format!(
            "{path}: 프로필 이름을 키로 하는 객체여야 해요"
        )));
    };
    map.iter()
        .map(|(profile, list)| {
            if !bot_core::id::is_name(profile) {
                return Err(bad(format!("{path}: 프로필 이름이 올바르지 않아요")));
            }
            let items = name_list(list, &format!("{path}.{profile}"))?;
            if !items.iter().all(|item| check(item)) {
                return Err(bad(format!(
                    "{path}.{profile}: 올바르지 않은 항목이 있어요"
                )));
            }
            Ok((profile.clone(), items))
        })
        .collect()
}

/// `host:port` (소문자·숫자·`.`·`-`, 포트 1~5자리)
fn is_egress(text: &str) -> bool {
    let Some((host, port)) = text.rsplit_once(':') else {
        return false;
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-'))
        && (1..=5).contains(&port.len())
        && port.bytes().all(|b| b.is_ascii_digit())
}

pub fn parse(text: &str) -> Result<HostFile, HostFileError> {
    let Ok(Value::Object(mut top)) = serde_json::from_str::<Value>(text) else {
        return Err(bad("호스트 설정은 JSON 객체여야 해요"));
    };
    if top.remove("version").and_then(|v| v.as_u64()) != Some(1) {
        return Err(bad("version: 1이 필요해요"));
    }
    let gateway = match top.remove("gateway") {
        Some(value) => HostConfig::from_value(value).map_err(|e| bad(e.to_string()))?,
        None => HostConfig::default(),
    };
    let servers = match top.remove("servers") {
        Some(value) => parse_all(&value).map_err(bad)?,
        None => Servers::default(),
    };
    let models = match top.remove("models") {
        Some(value) => profile_map(&value, "models", bot_core::id::is_name)?,
        None => BTreeMap::new(),
    };
    let egress = match top.remove("egress") {
        Some(value) => profile_map(&value, "egress", is_egress)?,
        None => BTreeMap::new(),
    };
    let providers = match top.remove("providers") {
        Some(value) => execution::parse_providers(&value).map_err(bad)?,
        None => BTreeMap::new(),
    };
    let sandbox = match top.remove("sandbox") {
        Some(value) => execution::parse_sandbox(&value).map_err(bad)?,
        None => Sandbox::default(),
    };
    let admin = match top.remove("admin") {
        Some(value) => AdminSettings::from_json(&value).map_err(bad)?,
        None => default_admin(),
    };
    if let Some(unknown) = top.keys().next() {
        return Err(bad(format!("알 수 없는 최상위 키예요: {unknown}")));
    }
    Ok(HostFile {
        catalog: catalog(
            &gateway,
            servers
                .local
                .keys()
                .chain(servers.remote.keys())
                .cloned()
                .collect(),
            &servers.remote,
            models,
            egress,
        ),
        gateway,
        admin,
        servers: servers.local,
        remote_servers: servers.remote,
        providers,
        sandbox,
        demo: false,
    })
}

impl HostFile {
    /// 권한이 달라지는 호스트 설정의 지문(활성화 판본에 묶는다). `revisions`는 서버 설치 판본이다.
    pub fn authority_fingerprint(
        &self,
        revisions: &BTreeMap<String, bot_core::id::Revision>,
    ) -> Result<[u8; 32], String> {
        let gateway = bot_gateway::mapping::fingerprint(&self.gateway)
            .map_err(|e| format!("입구 매핑을 확인하지 못했어요: {e}"))?;
        let base = execution::authority_fingerprint(
            &gateway,
            &self.catalog,
            &self.providers,
            &self.sandbox,
            revisions,
        );
        // 수집(OTLP) 설정도 권한이다: 어디로 무엇을(내용·식별자 표기·표본) 내보내는지가 바뀌거나 거두어지면 활성화 사건이라, 옛 설정으로 남은 프로세스가
        // 구판이 되어 그 프로세스의 내보내기 문이 닫힌다(`serve`가 `Exporter::set_gate`로 잇는다). 수집을 쓰지 않는 호스트는 지문이 그대로다.
        Ok(match bot_admin::trace::fingerprint(&self.admin.trace) {
            None => base,
            Some(trace) => {
                let mut hash = Sha256::new();
                hash.update(b"bot-authority-trace-v1\0");
                hash.update(base);
                hash.update(trace);
                hash.finalize().into()
            }
        })
    }

    /// 내장 데모 호스트에 합성 일정 서버(`examples/servers/demo-agenda`)를 연결한다. 프로필 루트(`BOT_PROFILES`) 옆의
    /// `servers/demo-agenda`를 먼저 찾고, 없으면 이 바이너리를 빌드한 저장소의 `examples/`를 찾는다. 둘 다 없으면 서버 없이
    /// 둔다(별칭만 있다 — 도구 호출은 설치 판본이 없어 일시 실패한다).
    pub fn attach_demo_server(&mut self, profiles_root: Option<&Path>) {
        let sibling = profiles_root
            .and_then(Path::parent)
            .map(|dir| dir.join("servers/demo-agenda"));
        let built_from =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/servers/demo-agenda");
        let root = sibling
            .into_iter()
            .chain([built_from])
            .find_map(|candidate| {
                std::fs::canonicalize(candidate)
                    .ok()
                    .filter(|path| path.is_dir())
            });
        if let Some(root) = root {
            self.servers.insert(
                "demo-agenda".to_owned(),
                ServerSpec {
                    root,
                    command: vec!["bin/server".to_owned(), "--stdio".to_owned()],
                    state: Default::default(),
                },
            );
        }
    }

    /// 계정 별칭이 가리키는 OS 사용자를 **한 번만** 풀어 실효 uid로 고정한다. 활성화 판본의 지문·입구 표(인증)·관리 화면의 운영자
    /// 신원·소켓 확인이 모두 같은 표를 쓰게 한다 — 따로 다시 풀면 사이에 계정이 바뀌어 결과가 달라질 수 있다.
    pub fn pin_uids(&mut self) -> Result<(), String> {
        for (alias, account) in &mut self.gateway.cli.accounts {
            let uid = bot_gateway::host::resolve_uid(account)
                .map_err(|e| format!("gateway.cli.accounts.{alias}: {e}"))?;
            *account = CliAccount {
                os_user: None,
                uid: Some(uid),
            };
        }
        Ok(())
    }
}

/// 키를 모두 생략한 관리 화면 설정(루프백 기본 주소, 추적 내보내기 없음)
fn default_admin() -> AdminSettings {
    AdminSettings::from_json(&Value::Object(Default::default()))
        .expect("빈 admin 설정은 항상 올바르다")
}

/// 프로필 검증이 쓰는 호스트 목록. 입구 별칭은 gateway 절에서 파생한다.
fn catalog(
    gateway: &HostConfig,
    servers: BTreeSet<String>,
    remote: &BTreeMap<String, Remote>,
    models: BTreeMap<String, BTreeSet<String>>,
    egress: BTreeMap<String, BTreeSet<String>>,
) -> HostCatalog {
    HostCatalog {
        servers,
        remote_servers: remote
            .iter()
            .map(|(alias, remote)| (alias.clone(), remote.target_text()))
            .collect(),
        models,
        cli_accounts: gateway.cli.accounts.keys().cloned().collect(),
        slack: gateway
            .slack
            .workspaces
            .iter()
            .map(|(alias, ws)| {
                (
                    alias.clone(),
                    SlackWorkspace {
                        channels: ws.channels.keys().cloned().collect(),
                        dm: ws.dm,
                    },
                )
            })
            .collect(),
        http_endpoints: gateway.http.endpoints.keys().cloned().collect(),
        // OpenAI 호환 입구 별칭 → 키 슬롯이 가리키는 주체 집합(승인 매핑의 요청자 검증용)
        openai_endpoints: gateway
            .http
            .openai
            .iter()
            .map(|(alias, endpoint)| {
                (
                    alias.clone(),
                    endpoint
                        .keys
                        .values()
                        .map(|slot| slot.subject.clone())
                        .collect(),
                )
            })
            .collect(),
        egress,
    }
}

pub fn load(path: &Path) -> Result<HostFile, HostFileError> {
    let meta = std::fs::metadata(path).map_err(|_| bad("호스트 설정 파일을 읽지 못했어요"))?;
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(bad("호스트 설정은 1 MiB 이하의 일반 파일이어야 해요"));
    }
    let text =
        std::fs::read_to_string(path).map_err(|_| bad("호스트 설정 파일을 읽지 못했어요"))?;
    parse(&text)
}

/// README 5분 시작의 호스트: 합성 일정 서버·로컬 데모 모델, CLI 계정 `owner`(예시 `personal`)와 `team`(예시 `team`)은 모두 이 프로세스를 실행한 OS 사용자.
pub fn demo(uid: u32) -> HostFile {
    let mut gateway = HostConfig::default();
    for alias in ["owner", "team"] {
        gateway.cli.accounts.insert(
            alias.into(),
            CliAccount {
                os_user: None,
                uid: Some(uid),
            },
        );
    }
    let mut catalog = HostCatalog::demo();
    catalog.cli_accounts = gateway.cli.accounts.keys().cloned().collect();
    HostFile {
        gateway,
        catalog,
        admin: default_admin(),
        servers: BTreeMap::new(),
        remote_servers: BTreeMap::new(),
        // 내장 데모: 데모 모델은 외부 키·VM이 필요 없다
        providers: BTreeMap::from([("assistant".to_owned(), Provider::Demo)]),
        sandbox: Sandbox::default(),
        demo: true,
    }
}

#[cfg(test)]
mod trace_authority_tests;

#[cfg(test)]
mod remote_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "version": 1,
        "gateway": {
            "cli": {"accounts": {"owner": {"uid": 501}}},
            "slack": {"workspaces": {"home": {"team_id": "T1", "bot_token": "bot-token",
                "app_token": "app-token", "dm": true, "channels": {"planning": "C1"}}}},
            "http": {"endpoints": {"reminders": {"issuer": "i", "audience": "a", "key": "k"}}}
        },
        "servers": {"demo-agenda": {"root": "/opt/bot/servers/demo-agenda", "command": ["bin/server", "--stdio"]}},
        "models": {"personal": ["assistant"]},
        "egress": {"personal": ["calendar.example:443"]}
    }"#;

    #[test]
    fn derives_the_profile_catalog_from_the_gateway_section() {
        let host = parse(FULL).unwrap();
        let catalog = &host.catalog;
        assert!(catalog.servers.contains("demo-agenda"));
        assert!(catalog.allows_model("personal", "assistant"));
        assert!(catalog.cli_accounts.contains("owner"));
        assert!(catalog.http_endpoints.contains("reminders"));
        let home = &catalog.slack["home"];
        assert!(home.dm && home.channels.contains("planning"));
        assert_eq!(catalog.egress["personal"].len(), 1);
        assert!(!host.demo);
    }

    #[test]
    fn the_catalog_lists_the_subjects_the_key_slots_of_each_openai_entrance_point_at() {
        let host = parse(
            r#"{"version": 1, "gateway": {"http": {"endpoints": {
                "openai": {"kind": "openai", "keys": {
                    "a": {"secret": "key-a", "subject": "build"},
                    "b": {"secret": "key-b", "subject": "build"},
                    "c": {"secret": "key-c", "subject": "ops"}}},
                "hooks": {"issuer": "i", "audience": "a", "key": "k"}}}}}"#,
        )
        .unwrap();
        // 같은 주체를 가리키는 슬롯(회전용)은 하나로 모이고, 웹훅 입구는 OpenAI 표에 없다
        assert_eq!(
            host.catalog.openai_endpoints,
            BTreeMap::from([(
                "openai".to_owned(),
                BTreeSet::from(["build".to_owned(), "ops".to_owned()])
            )])
        );
        assert!(host.catalog.http_endpoints.contains("hooks"));
        assert!(!host.catalog.http_endpoints.contains("openai"));
    }

    #[test]
    fn the_execution_sections_are_checked_in_shape_but_never_required() {
        let host = parse(
            r#"{"version": 1, "providers": {"assistant": {"kind": "demo"}},
                "sandbox": {"srt": {"binary": "/opt/srt", "work_root": "/var/work"}}}"#,
        )
        .unwrap();
        assert_eq!(host.providers["assistant"], Provider::Demo);
        assert!(host.sandbox.srt.is_some() && host.sandbox.apple.is_none());
        // 게이트웨이만 쓰는 호스트는 providers·sandbox가 없어도 된다
        let bare = parse(r#"{"version": 1}"#).unwrap();
        assert!(bare.providers.is_empty() && bare.sandbox == Sandbox::default());
        // 잘못된 모양은 어느 명령에서든 거절한다(pi는 어댑터 전에는 쓸 수 없다, 릴레이는 전체 인터페이스 금지)
        for bad in [
            r#"{"version": 1, "providers": {"a": {"kind": "pi"}}}"#,
            r#"{"version": 1, "sandbox": {"apple": {"image": "x", "network": "n", "relay_bind": "0.0.0.0:1"}}}"#,
            r#"{"version": 1, "sandbox": {"srt": {"binary": "srt", "work_root": "/w"}}}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        // 내장 데모는 외부 키·VM이 필요 없는 데모 모델을 쓴다
        assert_eq!(demo(501).providers["assistant"], Provider::Demo);
    }

    #[test]
    fn the_demo_host_connects_the_synthetic_agenda_server_next_to_the_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("servers/demo-agenda");
        std::fs::create_dir_all(server.join("bin")).unwrap();
        let profiles = dir.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        let mut host = demo(501);
        assert!(host.servers.is_empty(), "연결 전에는 서버가 없다");
        host.attach_demo_server(Some(&profiles));
        let spec = &host.servers["demo-agenda"];
        assert_eq!(spec.root, std::fs::canonicalize(&server).unwrap());
        assert_eq!(spec.command, ["bin/server", "--stdio"]);
        // 프로필 루트 옆에 없으면 저장소의 examples를 쓴다(이 저장소에는 있다)
        let mut fallback = demo(501);
        fallback.attach_demo_server(Some(&dir.path().join("elsewhere/profiles")));
        let root = &fallback.servers["demo-agenda"].root;
        assert!(root.ends_with("examples/servers/demo-agenda"), "{root:?}");
        // 별칭은 서버 설치와 무관하게 호스트 목록에 있다
        assert!(fallback.catalog.servers.contains("demo-agenda"));
    }

    #[test]
    fn the_demo_host_knows_both_example_profiles() {
        // 예시 폴더의 개인 비서(personal, 계정 owner)와 팀 템플릿(team, 계정 team)이 같은 내장 데모 호스트로 함께 검증된다
        let host = demo(501);
        let accounts: Vec<&str> = host
            .gateway
            .cli
            .accounts
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(accounts, ["owner", "team"]);
        assert!(
            host.gateway
                .cli
                .accounts
                .values()
                .all(|a| a.uid == Some(501))
        );
        assert_eq!(
            host.catalog.cli_accounts,
            ["owner", "team"].map(String::from).into()
        );
        for profile in ["personal", "team"] {
            assert!(host.catalog.allows_model(profile, "assistant"), "{profile}");
        }
    }

    #[test]
    fn account_names_are_resolved_once_into_the_effective_uid() {
        let me = bot_gateway::cli::socket::current_uid().unwrap();
        let name = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        let name = String::from_utf8_lossy(&name).trim().to_owned();
        let text = format!(
            r#"{{"version": 1, "gateway": {{"cli": {{"accounts": {{"owner": {{"os_user": "{name}"}}, "fixed": {{"uid": 4242}}}}}}}}}}"#
        );
        let mut host = parse(&text).unwrap();
        let before = bot_gateway::mapping::fingerprint(&host.gateway).unwrap();
        host.pin_uids().unwrap();
        let accounts = &host.gateway.cli.accounts;
        assert_eq!(
            (accounts["owner"].os_user.clone(), accounts["owner"].uid),
            (None, Some(me))
        );
        assert_eq!(accounts["fixed"].uid, Some(4242));
        // 이름으로 풀든 번호로 고정하든 같은 지문이고, 이후에는 다시 풀지 않아도 같다
        assert_eq!(
            bot_gateway::mapping::fingerprint(&host.gateway).unwrap(),
            before
        );
        // 풀 수 없는 사용자는 고정 단계(시작 전)에서 멈춘다
        let mut ghost = parse(
            r#"{"version": 1, "gateway": {"cli": {"accounts": {"owner": {"os_user": "no-such-user-zzz"}}}}}"#,
        )
        .unwrap();
        assert!(ghost.pin_uids().unwrap_err().contains("owner"));
    }

    #[test]
    fn the_admin_section_is_parsed_by_the_admin_crate_and_defaults_to_local() {
        let default = parse(r#"{"version": 1}"#).unwrap();
        assert_eq!(default.admin.bind.to_string(), "127.0.0.1:8787");
        let custom =
            parse(r#"{"version": 1, "admin": {"bind": "127.0.0.1:9100", "max_failures": 3}}"#)
                .unwrap();
        assert_eq!(
            (custom.admin.bind.to_string(), custom.admin.max_failures),
            ("127.0.0.1:9100".to_owned(), 3)
        );
        // 루프백 밖 주소는 원격 허용을 명시해야 하고, 모르는 키는 거절한다
        for bad_admin in [
            r#"{"bind": "0.0.0.0:8787"}"#,
            r#"{"nope": true}"#,
            r#""text""#,
        ] {
            let text = format!(r#"{{"version": 1, "admin": {bad_admin}}}"#);
            assert!(parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn rejects_unknown_keys_missing_version_and_bad_shapes() {
        for (json, needle) in [
            (r#"{"version": 1, "adnim": {}}"#, "adnim"),
            (r#"{"gateway": {}}"#, "version"),
            (r#"{"version": 2}"#, "version"),
            ("[]", "JSON 객체"),
            (r#"{"version": 1, "servers": "demo"}"#, "객체"),
            (
                r#"{"version": 1, "servers": ["demo-agenda"]}"#,
                "옛 별칭 배열",
            ),
            (
                r#"{"version": 1, "servers": {"Demo": {"root": "/a", "command": ["x"]}}}"#,
                "이름 규칙",
            ),
            (
                r#"{"version": 1, "servers": {"demo": {"root": "/a", "command": ["x"], "revision": "1"}}}"#,
                "자동으로 계산",
            ),
            (r#"{"version": 1, "models": {"Bad": ["a"]}}"#, "프로필 이름"),
            (
                r#"{"version": 1, "models": {"p": ["A B"]}}"#,
                "올바르지 않은",
            ),
            (
                r#"{"version": 1, "egress": {"p": ["no-port"]}}"#,
                "올바르지 않은",
            ),
            (r#"{"version": 1, "gateway": {"slak": {}}}"#, "slak"),
        ] {
            let message = parse(json).err().map(|e| e.to_string()).unwrap_or_default();
            assert!(message.contains(needle), "{json} → {message}");
        }
        assert!(parse(r#"{"version": 1}"#).is_ok());
    }

    #[test]
    fn the_demo_host_maps_the_owner_account_to_the_running_user() {
        let host = demo(501);
        assert!(host.demo);
        assert_eq!(host.gateway.cli.accounts["owner"].uid, Some(501));
        assert!(host.catalog.cli_accounts.contains("owner"));
        assert!(host.catalog.servers.contains("demo-agenda"));
        assert!(host.catalog.allows_model("personal", "assistant"));
    }

    #[test]
    fn load_refuses_missing_and_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&dir.path().join("none.json")).is_err());
        let big = dir.path().join("big.json");
        std::fs::write(&big, vec![b' '; (MAX_BYTES + 1) as usize]).unwrap();
        assert!(load(&big).is_err());
        let ok = dir.path().join("ok.json");
        std::fs::write(&ok, r#"{"version": 1}"#).unwrap();
        assert!(load(&ok).is_ok());
    }
}
