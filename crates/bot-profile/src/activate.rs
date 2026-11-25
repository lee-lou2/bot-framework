//! 활성화 검사: 프로필이 가리키는 별칭이 운영자가 호스트에 등록한 것인지 본다.
//! 바이트만으로 정해지지 않는 사실이라 판본에는 넣지 않고, 활성화할 때마다 확인한다.
use crate::{hint, host::HostCatalog, issue::Issues};
use bot_core::profile::{Adapter, Snapshot};
use std::collections::BTreeSet;

/// 에이전트 하나의 AGENT.md 경로(`main/research` → `agents/main/agents/research/AGENT.md`).
fn agent_file(id: &str) -> String {
    let parts: Vec<&str> = id.split('/').collect();
    format!("agents/{}/AGENT.md", parts.join("/agents/"))
}

/// 허용되지 않은 모델 별칭을 어디서 정했는가: 프로필의 `model`이면 `profile.yaml`, 에이전트가 자기 AGENT.md로 바꾼 것이면 그 파일(부모에게서 물려받은 값은 건너뛴다).
fn model_places(snap: &Snapshot, alias: &str) -> Vec<String> {
    let mut places = Vec::new();
    if snap.model == alias {
        places.push("profile.yaml".to_owned());
    }
    for (id, agent) in &snap.agents {
        let inherited = match &agent.parent {
            Some(parent) => snap.agents.get(parent).is_some_and(|p| p.model == alias),
            None => snap.model == alias,
        };
        if agent.model == alias && !inherited {
            places.push(agent_file(&id.0));
        }
    }
    if places.is_empty() {
        places.push("profile.yaml".to_owned());
    }
    places
}

/// HTTP 입구 별칭이 호스트에 등록돼 있고 종류(웹훅/OpenAI)가 바인딩과 맞는지 본다.
/// 새 API의 `/v1`과 경로가 겹치는 웹훅 별칭 `v1`은 OpenAI 입구와 함께 켜지 못한다(단독으로는 옛 그대로 쓴다).
fn http_endpoint(host: &HostCatalog, endpoint: &str, openai: bool, at: &str, issues: &mut Issues) {
    let (is_webhook, is_openai) = (
        host.http_endpoints.contains(endpoint),
        host.openai_endpoints.contains_key(endpoint),
    );
    if !is_webhook && !is_openai {
        issues.error_at(
            "unknown-endpoint",
            "profile.yaml",
            at,
            format!("호스트에 등록하지 않은 HTTP 입구예요: {endpoint}"),
        );
    } else if openai && !is_openai {
        issues.error_at(
            "endpoint-kind-mismatch",
            "profile.yaml",
            at,
            format!("HTTP 입구 `{endpoint}`는 호스트에서 웹훅(`kind: webhook`)으로 등록돼 있어요. `model`이 있는 OpenAI 바인딩은 `kind: openai` 입구에만 연결해요"),
        );
    } else if !openai && !is_webhook {
        issues.error_at(
            "endpoint-kind-mismatch",
            "profile.yaml",
            at,
            format!("HTTP 입구 `{endpoint}`는 호스트에서 OpenAI 호환(`kind: openai`)으로 등록돼 있어요. `model`이 없는 웹훅 바인딩은 웹훅 입구에만 연결해요"),
        );
    }
    if !openai && endpoint == "v1" && !host.openai_endpoints.is_empty() {
        issues.error_at(
            "webhook-v1-conflict",
            "profile.yaml",
            at,
            "웹훅 입구 별칭 `v1`은 OpenAI 호환 API의 `/v1` 경로와 겹쳐서 함께 켤 수 없어요. 웹훅 별칭을 다른 이름(예: `hooks`)으로 바꾸고 호스트 설정과 프로필의 `endpoint`를 함께 고쳐 주세요",
        );
    }
}

pub fn check_host(snap: &Snapshot, host: &HostCatalog, issues: &mut Issues) {
    let profile = snap.id.0.as_str();
    let mut models: BTreeSet<&str> = BTreeSet::from([snap.model.as_str()]);
    models.extend(snap.agents.values().map(|a| a.model.as_str()));
    let allowed = host.models_of(profile);
    for model in models
        .into_iter()
        .filter(|m| !host.allows_model(profile, m))
    {
        let fix = if allowed.is_empty() {
            format!(
                "호스트 설정(BOT_HOST_CONFIG)의 `models.{profile}`이 비어 있어요. 예: `\"models\": {{\"{profile}\": [\"{model}\"]}}`"
            )
        } else {
            let meant = hint::nearest(model, allowed.iter().copied())
                .map_or(String::new(), |m| format!(" 혹시 `{m}`인가요?"));
            format!(
                "이 프로필에 허용된 별칭: {}.{meant} 새 별칭을 쓰려면 호스트 설정(BOT_HOST_CONFIG)의 `models.{profile}` 목록에 `{model}`을(를) 더하세요",
                allowed.join(", ")
            )
        };
        let message =
            format!("모델 별칭 `{model}`는 프로필 `{profile}`에 허용되지 않았어요. {fix}");
        for place in model_places(snap, model) {
            issues.error_at("model-not-allowed", &place, "/model", message.clone());
        }
    }
    for (id, binding) in &snap.bindings {
        let at = format!("바인딩 {}", id.0);
        match &binding.source {
            Adapter::Cli { account } if !host.cli_accounts.contains(account) => {
                issues.error_at(
                    "unknown-account",
                    "profile.yaml",
                    &at,
                    format!("호스트에 등록하지 않은 CLI 계정이에요: {account}"),
                );
            }
            Adapter::SlackChannel { workspace, channel } => match host.slack.get(workspace) {
                None => issues.error_at(
                    "unknown-workspace",
                    "profile.yaml",
                    &at,
                    format!("호스트에 등록하지 않은 Slack 워크스페이스예요: {workspace}"),
                ),
                Some(ws) if !ws.channels.contains(channel) => {
                    issues.error_at(
                        "unknown-channel",
                        "profile.yaml",
                        &at,
                        format!("워크스페이스 {workspace}에 등록하지 않은 채널이에요: {channel}"),
                    );
                }
                Some(_) => {}
            },
            Adapter::SlackDm { workspace } => match host.slack.get(workspace) {
                None => issues.error_at(
                    "unknown-workspace",
                    "profile.yaml",
                    &at,
                    format!("호스트에 등록하지 않은 Slack 워크스페이스예요: {workspace}"),
                ),
                Some(ws) if !ws.dm => {
                    issues.error_at(
                        "dm-not-allowed",
                        "profile.yaml",
                        &at,
                        format!("워크스페이스 {workspace}는 DM 입구를 허용하지 않아요"),
                    );
                }
                Some(_) => {}
            },
            Adapter::Cli { .. } => {}
            Adapter::Http { endpoint } => http_endpoint(host, endpoint, false, &at, issues),
            Adapter::OpenAi {
                endpoint,
                approvers,
                ..
            } => {
                http_endpoint(host, endpoint, true, &at, issues);
                // 승인 매핑은 프로필의 지정이고 키 슬롯은 호스트의 자격 발급이다. 슬롯이 없는 주체의 매핑은 인증 권한을 주지 않으므로 오류가 아니라 경고다
                // (한 멤버의 마지막 슬롯을 빼면서 프로필 수정까지 요구하면 철회하려던 옛 설정이 활성화 실패로 계속 살아 있다). 오타는 이 경고로 알린다.
                if let Some(subjects) = host.openai_endpoints.get(endpoint) {
                    let prefix = format!("http:{endpoint}/");
                    for requester in approvers.keys() {
                        let subject = requester.0.strip_prefix(&prefix);
                        if !subject.is_some_and(|s| subjects.contains(s)) {
                            issues.warning(
                                "approver-key-unknown",
                                "profile.yaml",
                                format!(
                                    "{at}: 승인 매핑의 HTTP 주체 `{}`를 가리키는 키 슬롯이 호스트 입구 `{endpoint}`에 없어요. 이 주체는 키가 있어야 호출할 수 있어요(호스트 설정의 `keys.<슬롯>.subject`를 확인하세요)",
                                    requester.0
                                ),
                            );
                        }
                    }
                }
            }
        }
    }
    let mut seen = BTreeSet::new();
    for skill in snap.agents.values().flat_map(|a| a.skills.values()) {
        let file = skill
            .instructions
            .relative_path
            .strip_suffix("SKILL.md")
            .map_or_else(String::new, |dir| format!("{dir}tools.yaml"));
        if !seen.insert(file.clone()) {
            continue;
        }
        for (id, tool) in &skill.tools {
            if !host.servers.contains(&tool.server) {
                issues.error(
                    "missing-server",
                    &file,
                    format!("{}: 호스트에 설치하지 않은 서버예요: {}", id.0, tool.server),
                );
            }
            let kept = host.effective_egress(profile, &tool.egress);
            if kept.len() < tool.egress.len() {
                let dropped: Vec<&String> = tool.egress.difference(&kept).collect();
                issues.warning(
                    "egress-narrowed",
                    &file,
                    format!("{}: 운영자 상한 밖이라 막히는 호스트 {dropped:?}", id.0),
                );
            }
        }
    }
}
