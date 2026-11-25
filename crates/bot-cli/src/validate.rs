//! `bot validate` — DB·Slack에 접속하지 않고 시작 전 점검을 모두 한다: 프로필·호스트 등록부 검증, 입구 표(별칭 해석·유일성·
//! 트리거), 필요한 자격이 보관소에 있는지(값은 읽되 출력하지 않는다). 하나라도 어긋나면 오류로 끝난다.
use crate::{
    execution::Provider,
    host::HostFile,
    load::{check_entrances, load, secret_store},
    secrets::env::variable_name,
    settings::{SecretsKind, Settings},
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, SecretStore},
    profile::{Snapshot, TriggerSource},
};
use bot_gateway::{
    host::{HostConfig, Receive},
    http::token::MIN_KEY_BYTES,
};
use std::collections::BTreeSet;

/// 이 프로필이 쓰는 자격 이름: 프로필이 선언한 것 + 도구가 환경으로 받는 것 + 바인딩한 입구가 쓰는 것(호스트가 정한 이름)
/// + 프로필이 허용한 모델 별칭 중 `claude-cli`로 실행하는 것의 서비스 키(호스트가 정한 이름)
pub fn needed_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = snapshot
        .secrets
        .iter()
        .map(|s| s.as_str().to_owned())
        .collect();
    for agent in snapshot.agents.values() {
        for skill in agent.skills.values() {
            for tool in skill.tools.values() {
                names.extend(tool.secrets.values().map(|s| s.as_str().to_owned()));
            }
        }
    }
    names.extend(entrance_secrets(snapshot, &host.gateway));
    names.extend(model_secrets(snapshot, host));
    names
}

/// 이 프로필이 쓰는 모델 서비스 키: 프로필이 허용한 별칭(`models`) 중 `claude-cli`로 실행하는 것의 `secret`(프로필 자격 이름).
/// 데모 별칭과 허용하지 않은 별칭은 키가 필요 없다. 이 이름을 보관소에서 못 찾으면 모든 모델 요청이 런타임에 실패하므로 validate와
/// worker 시작이 같이 점검한다.
pub fn model_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    host.catalog
        .models
        .get(&snapshot.id.0)
        .into_iter()
        .flatten()
        .filter_map(|alias| match host.providers.get(alias) {
            Some(Provider::ClaudeCli { secret, .. }) => Some(secret.clone()),
            _ => None,
        })
        .collect()
}

/// 바인딩한 입구(Slack 토큰·HTTP 키)가 쓰는 자격 이름. gateway 역할이 시작할 때 필요한 것은 이것뿐이다
/// (도구 자격은 도구를 실행하는 역할의 몫).
pub fn entrance_secrets(snapshot: &Snapshot, gateway: &HostConfig) -> BTreeSet<String> {
    use bot_core::profile::Adapter;
    let mut names = BTreeSet::new();
    for binding in snapshot.bindings.values() {
        match &binding.source {
            Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } => {
                if let Some(ws) = gateway.slack.workspaces.get(workspace) {
                    names.insert(ws.bot_token.clone());
                    match ws.receive {
                        Receive::Socket => names.extend(ws.app_token.clone()),
                        Receive::Http => names.extend(ws.signing_secret.clone()),
                    }
                }
            }
            Adapter::Http { endpoint } => {
                if let Some(config) = gateway.http.endpoints.get(endpoint) {
                    names.insert(config.key.clone());
                }
            }
            Adapter::Cli { .. } => {}
        }
    }
    // 민감 DM 접수는 프로필별 지문 키(`private-intake`)가 있어야 안전하게 중복을 가린다: 없으면 접수하지 않으므로 시작 전에 막는다
    if has_private_triggers(snapshot) {
        names.insert(bot_broker::private::FINGERPRINT_SECRET.to_owned());
    }
    names
}

/// 민감(private) 트리거가 있는 프로필
pub fn has_private_triggers(snapshot: &Snapshot) -> bool {
    snapshot
        .agents
        .values()
        .flat_map(|agent| &agent.triggers)
        .any(|t| t.private && !matches!(t.on, TriggerSource::Cron { .. }))
}

/// 프로필이 허용한 별칭 중 `claude-cli`가 있는데 호스트 설정에 모델 VM(`sandbox.apple`)이 없으면 안내 문구. worker는 이 호스트에서 시작하지 않는다
/// (다른 샌드박스로 대신하지 않는다).
pub fn missing_model_sandbox(snapshot: &Snapshot, host: &HostFile) -> Option<String> {
    (!model_secrets(snapshot, host).is_empty() && host.sandbox.apple.is_none()).then(|| {
        format!(
            "{}: claude-cli 별칭을 허용했지만 호스트 설정에 sandbox.apple(다이제스트로 고정한 이미지와 호스트 전용 망)이 없어요 — docs/model-image.md",
            snapshot.id.0
        )
    })
}

/// 필요한 자격 중 보관소에 없는 것의 안내 문구(값은 읽어 보되 어디에도 남기지 않는다).
pub async fn missing_secrets(
    store: &dyn SecretStore,
    snapshot: &Snapshot,
    host: &HostFile,
    kind: &SecretsKind,
) -> Vec<String> {
    missing_named(store, &snapshot.id, needed_secrets(snapshot, host), kind).await
}

/// 이름 목록 중 이 프로필의 보관소에 없는 것의 안내 문구.
pub async fn missing_named(
    store: &dyn SecretStore,
    profile: &ProfileId,
    names: BTreeSet<String>,
    kind: &SecretsKind,
) -> Vec<String> {
    let profile = profile.clone();
    let mut missing = Vec::new();
    let handle = store.for_profile(profile.clone()).await;
    for name in names {
        let Some(secret) = SecretName::parse(&name) else {
            missing.push(format!("{}: 이름 {name}이 올바르지 않아요", profile.0));
            continue;
        };
        let found = match &handle {
            Ok(handle) => handle.resolve(secret).await,
            Err(_) => Err(Error::Unavailable),
        };
        match found {
            Ok(_) => {}
            Err(Error::Missing) => {
                let hint = match kind {
                    SecretsKind::Env => {
                        format!(" (환경 변수 {})", variable_name(&profile.0, &name))
                    }
                    _ => String::new(),
                };
                missing.push(format!("{}: 자격 {name}이(가) 없어요{hint}", profile.0));
            }
            Err(_) => missing.push(format!(
                "{}: 자격 {name}을(를) 읽지 못했어요(보관소 오류)",
                profile.0
            )),
        }
    }
    missing
}

/// HTTP 입구 키가 HS256 하한(`MIN_KEY_BYTES`)보다 짧은 것의 안내 문구. 없는 키는 `missing_named`가 알린다.
/// 짧은 키로 시작하면 모든 요청이 런타임에 거절되므로 시작 전에 막는다(값은 길이만 보고 남기지 않는다).
pub async fn weak_http_keys(
    store: &dyn SecretStore,
    snapshot: &Snapshot,
    gateway: &HostConfig,
) -> Vec<String> {
    use bot_core::profile::Adapter;
    let Ok(handle) = store.for_profile(snapshot.id.clone()).await else {
        return Vec::new();
    };
    let mut weak = Vec::new();
    for binding in snapshot.bindings.values() {
        let Adapter::Http { endpoint } = &binding.source else {
            continue;
        };
        let Some(config) = gateway.http.endpoints.get(endpoint) else {
            continue;
        };
        let Some(name) = SecretName::parse(&config.key) else {
            continue;
        };
        if let Ok(value) = handle.resolve(name).await
            && value.0.len() < MIN_KEY_BYTES
        {
            weak.push(format!(
                "{}: HTTP 입구 {endpoint}의 키 {}이(가) 너무 짧아요({MIN_KEY_BYTES}바이트 이상이어야 해요)",
                snapshot.id.0, config.key
            ));
        }
    }
    weak
}

pub async fn run(settings: &Settings) -> Result<(), String> {
    let loaded = load(settings)?;
    let active = loaded.set.active().clone();
    check_entrances(active.clone(), &loaded.host)?;
    let store = secret_store(settings)?;
    let mut problems = Vec::new();
    // 점검이 모두 끝나기 전에는 «통과»를 내보내지 않는다. 결과 줄은 모아 두었다가 문제가 없을 때만 찍는다.
    let mut report = Vec::new();
    // 도구 서버: 설치 폴더를 읽어 판본을 계산한다(복사 없이). 위치·시각이 달라도 내용이 같으면 같은 판본이다.
    let revisions = match crate::install::measure(&loaded.host.servers) {
        Ok(revisions) => {
            for (alias, revision) in &revisions {
                report.push(format!(
                    "서버 {alias}: 판본 {}",
                    revision.0.get(..12).unwrap_or(&revision.0)
                ));
            }
            revisions
        }
        Err(problem) => {
            problems.push(problem);
            Default::default()
        }
    };
    for snapshot in active.values() {
        problems.extend(missing_model_sandbox(snapshot, &loaded.host));
        problems.extend(missing_secrets(&*store, snapshot, &loaded.host, &settings.secrets).await);
        problems.extend(weak_http_keys(&*store, snapshot, &loaded.host.gateway).await);
        problems.extend(crate::serve::seed::problems(snapshot));
    }
    if !problems.is_empty() {
        return Err(problems.join("\n"));
    }
    let publication = crate::serve::Publication::of(&loaded.set, &loaded.host, &revisions)?;
    if loaded.host.demo {
        println!(
            "BOT_HOST_CONFIG가 없어 내장 데모 호스트를 써요(CLI 계정 owner·team = 지금 사용자)"
        );
    }
    println!("{}", loaded.set.render());
    for line in report {
        println!("{line}");
    }
    println!(
        "활성화 판본 {} (프로필 집합 + 호스트 권한 설정)",
        publication
            .revision
            .0
            .get(..12)
            .unwrap_or(&publication.revision.0)
    );
    println!("입구·자격 점검 통과");
    Ok(())
}

/// 이 프로필 이름이 활성 집합에 있는가(명령이 `--profile`을 확인할 때)
pub fn known_profile(active: &bot_gateway::routes::Active, name: &str) -> bool {
    active.contains_key(&ProfileId(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host;
    use bot_gateway::testing::profile;

    fn host_with(providers: &str) -> HostFile {
        host::parse(&format!(
            r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{providers}}}"#
        ))
        .unwrap()
    }

    const PROVIDERS: &str = r#", "providers": {
        "assistant": {"kind": "claude-cli", "model": "m1", "endpoint": "https://api.example", "secret": "model-key"},
        "plain": {"kind": "demo"},
        "other": {"kind": "claude-cli", "model": "m2", "endpoint": "https://api.example", "secret": "work-key"}}"#;

    #[test]
    fn a_profile_needs_the_service_keys_of_the_claude_aliases_it_is_allowed_to_use() {
        let snapshot = profile("personal").build();
        let host = host_with(PROVIDERS);
        // 허용한 별칭 중 claude-cli인 것(assistant)의 키만이다: 데모 별칭(plain)과 다른 프로필의 별칭(other: work-key)은 아니다
        assert_eq!(
            model_secrets(&snapshot, &host),
            BTreeSet::from(["model-key".to_owned()])
        );
        // validate와 관측(알려진 비밀 가리기)이 쓰는 목록에 들어간다
        let needed = needed_secrets(&snapshot, &host);
        assert!(needed.contains("model-key"), "{needed:?}");
        assert!(!needed.contains("work-key"), "{needed:?}");
    }

    #[test]
    fn a_profile_with_a_private_trigger_needs_its_fingerprint_key_and_one_without_does_not() {
        use bot_gateway::testing::{event_trigger, tool_target};
        let gateway = HostConfig::from_json(
            r#"{"slack": {"workspaces": {"home": {"team_id": "T1", "dm": true, "bot_token": "bot-token", "app_token": "app-token"}}}}"#,
        )
        .unwrap();
        let dm = bot_core::profile::Adapter::SlackDm {
            workspace: "home".into(),
        };
        let mut secure = event_trigger("secure", "direct", tool_target("inbox.receive", "{}"));
        secure.private = true;
        let private = profile("personal")
            .binding("direct", dm.clone(), "main")
            .trigger("main", secure)
            .build();
        let names = entrance_secrets(&private, &gateway);
        assert!(names.contains("private-intake"), "{names:?}");
        // 입구 자격은 그대로 있고, 민감 트리거가 없으면 지문 키를 요구하지 않는다
        assert!(
            names.contains("bot-token") && names.contains("app-token"),
            "{names:?}"
        );
        let plain = profile("personal").binding("direct", dm, "main").build();
        assert!(!entrance_secrets(&plain, &gateway).contains("private-intake"));
        // 지문 키는 validate·관측이 쓰는 필요한 자격 목록에도 들어간다
        let host = host_with(
            r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#,
        );
        assert!(needed_secrets(&private, &host).contains("private-intake"));
    }

    #[test]
    fn a_claude_alias_needs_the_model_vm_in_the_host_config() {
        let snapshot = profile("personal").build();
        let apple = format!(
            r#", "sandbox": {{"apple": {{"image": "img@sha256:{}", "network": "bot-internal"}}}}"#,
            "a".repeat(64)
        );
        // claude-cli를 허용했는데 VM 설정이 없으면 막는다(srt만 있어도 마찬가지다)
        let without = host_with(PROVIDERS);
        let message = missing_model_sandbox(&snapshot, &without).unwrap();
        assert!(
            message.contains("personal") && message.contains("sandbox.apple"),
            "{message}"
        );
        let srt = host_with(&format!(
            r#"{PROVIDERS}, "sandbox": {{"srt": {{"binary": "/usr/bin/true", "work_root": "/tmp/w"}}}}"#
        ));
        assert!(missing_model_sandbox(&snapshot, &srt).is_some());
        // VM 설정이 있으면 통과, 데모 별칭만 쓰는 호스트는 VM이 필요 없다
        assert!(
            missing_model_sandbox(&snapshot, &host_with(&format!("{PROVIDERS}{apple}"))).is_none()
        );
        let demo = host_with(
            r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#,
        );
        assert!(missing_model_sandbox(&snapshot, &demo).is_none());
    }

    #[test]
    fn hosts_without_claude_aliases_or_without_the_profile_in_the_model_table_need_no_model_key() {
        let snapshot = profile("personal").build();
        let demo = host_with(
            r#", "providers": {"assistant": {"kind": "demo"}, "plain": {"kind": "demo"}}"#,
        );
        assert!(model_secrets(&snapshot, &demo).is_empty());
        // 호스트가 이 프로필에 별칭을 허용하지 않았다
        let other = host::parse(&format!(
            r#"{{"version": 1, "models": {{"work": ["other"]}}{PROVIDERS}}}"#
        ))
        .unwrap();
        assert!(model_secrets(&snapshot, &other).is_empty());
        // providers 절이 없는 호스트(게이트웨이만 쓰는 호스트)
        assert!(model_secrets(&snapshot, &host::parse(r#"{"version": 1}"#).unwrap()).is_empty());
    }
}
