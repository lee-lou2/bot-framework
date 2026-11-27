//! `bot validate` — DB·Slack에 접속하지 않고 시작 전 점검을 모두 한다: 프로필·호스트 등록부 검증, 입구 표(별칭 해석·유일성·
//! 트리거), 필요한 자격이 보관소에 있는지(값은 읽되 출력하지 않는다). 하나라도 어긋나면 오류로 끝난다.
use crate::{
    execution::Provider,
    host::HostFile,
    load::{check_entrances, load, secret_store},
    settings::{SecretsKind, Settings},
};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, SecretStore},
    profile::{Builtin, Snapshot, TriggerSource},
};
use bot_gateway::{host::HostConfig, http::token::MIN_KEY_BYTES};
use std::collections::BTreeSet;

mod trace;
pub use trace::{all_needed_secrets, host_obs_secrets, trace_secrets};

/// 이 프로필이 쓰는 자격 이름: 프로필이 선언한 것 + 도구가 환경으로 받는 것 + 바인딩한 입구가 쓰는 것(호스트가 정한 이름) + 수집 헤더가 받는 것
/// + 프로필이 허용한 모델 별칭 중 서비스 키가 필요한 것(`claude-cli`·API 직접 호출)의 키(호스트가 정한 이름)
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
                // 원격 서버의 bearer는 호스트가 서버에 묶은 자격이라 그 서버를 쓰는 프로필의 보관소에 있어야 한다
                names.extend(
                    host.remote_servers
                        .get(&tool.server)
                        .and_then(|remote| remote.bearer.as_ref())
                        .map(|name| name.as_str().to_owned()),
                );
            }
        }
    }
    names.extend(entrance_secrets(snapshot, &host.gateway));
    names.extend(model_secrets(snapshot, host));
    names.extend(trace_secrets(snapshot, host));
    names
}

/// 이 프로필이 쓰는 모델 서비스 키: 프로필이 허용한 별칭(`models`) 중 `claude-cli`·API 직접 호출(`anthropic-api`·`openai-api`)로 실행하는 것의 `secret`(프로필 자격 이름).
/// 데모 별칭·키 없는 loopback 로컬 서버·허용하지 않은 별칭은 키가 필요 없다. 이 이름을 보관소에서 못 찾으면 모든 모델 요청이 런타임에 실패하므로 validate와
/// worker 시작이 같이 점검한다.
pub fn model_secrets(snapshot: &Snapshot, host: &HostFile) -> BTreeSet<String> {
    host.catalog
        .models
        .get(&snapshot.id.0)
        .into_iter()
        .flatten()
        .filter_map(|alias| host.providers.get(alias).and_then(Provider::secret))
        .map(str::to_owned)
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
                    names.extend(ws.app_token.clone());
                }
            }
            Adapter::Http { endpoint } => {
                if let Some(config) = gateway.http.endpoints.get(endpoint) {
                    names.insert(config.key.clone());
                }
            }
            Adapter::OpenAi { endpoint, .. } => {
                if let Some(config) = gateway.http.openai.get(endpoint) {
                    names.extend(config.keys.values().map(|slot| slot.secret.clone()));
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
    let uses_claude_cli = host
        .catalog
        .models
        .get(&snapshot.id.0)
        .into_iter()
        .flatten()
        .any(|alias| {
            host.providers
                .get(alias)
                .is_some_and(Provider::needs_model_vm)
        });
    (uses_claude_cli && host.sandbox.apple.is_none()).then(|| {
        format!(
            "{}: claude-cli 별칭을 허용했지만 호스트 설정에 sandbox.apple(다이제스트로 고정한 이미지와 호스트 전용 망)이 없어요 — docs/model-image.md",
            snapshot.id.0
        )
    })
}

fn builtin_name(tool: Builtin) -> &'static str {
    match tool {
        Builtin::Read => "read",
        Builtin::Grep => "grep",
        Builtin::Glob => "glob",
    }
}

/// 에이전트가 선언한 읽기 전용 내장 도구(`builtin`)를 해당 모델 실행기가 지원하지 않으면 안내 문구.
/// API 직접 호출(`anthropic-api`·`openai-api`)은 모든 내장 도구를 실행 시 `unsupported_builtin`으로 거절하고,
/// `claude-cli`는 `--bare` VM에서 `read`만 지원한다. 데모 모델이나 `providers`가 없는(게이트웨이 전용) 호스트는 막지 않는다.
pub fn unsupported_builtin_tools(snapshot: &Snapshot, host: &HostFile) -> Vec<String> {
    let mut problems = Vec::new();
    for (agent_id, agent) in &snapshot.agents {
        if agent.builtin.is_empty() {
            continue;
        }
        match host.providers.get(&agent.model) {
            Some(Provider::Api(_)) => {
                let list = agent
                    .builtin
                    .iter()
                    .copied()
                    .map(builtin_name)
                    .collect::<Vec<_>>()
                    .join(", ");
                problems.push(format!(
                    "{}: 에이전트 {}이(가) 내장 도구({})를 선언했지만 모델 별칭 {}은(는) API 직접 호출이라 내장 도구를 지원하지 않아요",
                    snapshot.id.0, agent_id.0, list, agent.model
                ));
            }
            Some(Provider::ClaudeCli { .. }) => {
                let bad: Vec<_> = agent
                    .builtin
                    .iter()
                    .copied()
                    .filter(|b| matches!(b, Builtin::Grep | Builtin::Glob))
                    .map(builtin_name)
                    .collect();
                if !bad.is_empty() {
                    problems.push(format!(
                        "{}: 에이전트 {}이(가) 내장 도구({})를 선언했지만 claude-cli(--bare VM)는 read만 지원해요",
                        snapshot.id.0,
                        agent_id.0,
                        bad.join(", ")
                    ));
                }
            }
            Some(Provider::Demo) | None => {}
        }
    }
    problems
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
                let hint = kind.missing_hint(&profile.0, &name);
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

/// OpenAI 호환 입구의 키 슬롯 값 점검(값은 모양과 겹침만 보고 어디에도 남기지 않는다). 인증이 슬롯 값을 요청마다 읽어 대조하므로,
/// 모양이 틀린 값은 아무도 인증하지 못하고, 한 값이 슬롯 둘 이상에(입구·프로필이 달라도) 있으면 그 키가 모든 요청에서 거절된다 — 시작 전에 막는다.
/// 없는 값은 `missing_named`가 알린다.
pub async fn weak_openai_keys(
    store: &dyn SecretStore,
    snapshots: &[std::sync::Arc<Snapshot>],
    gateway: &HostConfig,
) -> Vec<String> {
    use bot_core::profile::Adapter;
    use bot_gateway::host::openai_key_shaped;
    use sha2::{Digest, Sha256};
    let mut problems = Vec::new();
    // 값의 지문 → 처음 쓴 (프로필, 입구, 슬롯)
    let mut seen: std::collections::BTreeMap<[u8; 32], (String, String, String)> =
        Default::default();
    for snapshot in snapshots {
        let Ok(handle) = store.for_profile(snapshot.id.clone()).await else {
            continue;
        };
        let endpoints: BTreeSet<&String> = snapshot
            .bindings
            .values()
            .filter_map(|binding| match &binding.source {
                Adapter::OpenAi { endpoint, .. } => Some(endpoint),
                _ => None,
            })
            .collect();
        for endpoint in endpoints {
            let Some(config) = gateway.http.openai.get(endpoint) else {
                continue;
            };
            for (slot, key) in &config.keys {
                let Some(name) = SecretName::parse(&key.secret) else {
                    continue;
                };
                let Ok(value) = handle.resolve(name).await else {
                    continue;
                };
                let value = value.0.trim_ascii();
                if !openai_key_shaped(value) {
                    problems.push(format!(
                        "{}: OpenAI 입구 {endpoint}의 키 슬롯 {slot}의 값이 키 모양이 아니에요(공백 없는 ASCII 32~256바이트여야 해요)",
                        snapshot.id.0
                    ));
                    continue;
                }
                let here = (snapshot.id.0.clone(), endpoint.clone(), slot.clone());
                if let Some(first) = seen.insert(Sha256::digest(value).into(), here.clone()) {
                    problems.push(format!(
                        "{}: OpenAI 입구 {endpoint}의 키 슬롯 {slot}이(가) {}/{}의 슬롯 {}과(와) 같은 값을 써요 — 슬롯마다 다른 키여야 해요(같으면 그 키는 인증되지 않아요)",
                        here.0, first.0, first.1, first.2
                    ));
                }
            }
        }
    }
    problems
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
    let revisions =
        match crate::install::measure_all(&loaded.host.servers, &loaded.host.remote_servers) {
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
        problems.extend(unsupported_builtin_tools(snapshot, &loaded.host));
        problems.extend(weak_http_keys(&*store, snapshot, &loaded.host.gateway).await);
        problems.extend(crate::serve::seed::problems(snapshot));
    }
    for (profile, names) in all_needed_secrets(active.values().map(AsRef::as_ref), &loaded.host) {
        problems.extend(missing_named(&*store, &profile, names, &settings.secrets).await);
    }
    let snapshots: Vec<_> = active.values().cloned().collect();
    problems.extend(weak_openai_keys(&*store, &snapshots, &loaded.host.gateway).await);
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
    publication.print_grants();
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
mod api_tests;
#[cfg(test)]
mod tests;
