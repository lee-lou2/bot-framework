//! 호스트 설정(host.json) 해석, 모델 제공자, 도구 서버 점검. 읽기만 한다(서버·모델을 시작하지 않는다). 판정은 운영 경로의 함수를 그대로 쓴다 —
//! 호스트 해석 `load_host`, 모델 별칭 `serve::check_model_aliases`(worker 시작이 쓰는 `models::build`), 서버 판본 `install::measure`와 worker가 쓰는
//! `serve::refuse_required_state`.
use super::{Item, State, capped};
use crate::{
    execution::Provider,
    host::HostFile,
    install,
    load::load_host,
    serve::{self, remote::literal_ok},
    settings::Settings,
};
use bot_core::id::Revision;
use bot_gateway::cli::socket::current_uid;
use bot_runtime::model::LimitField;
use std::collections::{BTreeMap, BTreeSet};

/// 해석한 호스트와 도구 서버의 설치 판본(계산하지 못했으면 None)
pub struct Checked {
    pub file: HostFile,
    pub revisions: Option<BTreeMap<String, Revision>>,
}

pub fn check(settings: &Settings, items: &mut Vec<Item>) -> Option<Checked> {
    let Ok(uid) = current_uid() else {
        items.push(Item::new(
            State::Fail,
            "호스트 설정",
            "OS 사용자 번호(id -u)를 확인하지 못했어요",
        ));
        return None;
    };
    let host = match load_host(settings, uid) {
        Ok(host) => host,
        Err(error) => {
            let mut details = Vec::new();
            if let Some(path) = &settings.host_config {
                details.push(format!("파일: {}", path.display()));
            }
            details.push(
                "경로와 JSON 형식을 확인해 주세요(예시는 docs/profile-spec.md, 새로 만들려면 `bot init <이름>`)"
                    .to_owned(),
            );
            items.push(Item::new(State::Fail, "호스트 설정", error).with(details));
            return None;
        }
    };
    items.push(summary(&host, settings));
    items.push(providers(&host));
    let (item, revisions) = servers(&host);
    items.push(item);
    Some(Checked {
        file: host,
        revisions,
    })
}

fn summary(host: &HostFile, settings: &Settings) -> Item {
    let aliases: BTreeSet<&String> = host.catalog.models.values().flatten().collect();
    let counts = format!(
        "모델 별칭 {} · 제공자 {} · 도구 서버 {} · CLI 계정 {} · Slack 워크스페이스 {} · HTTP 입구 {} · OpenAI 입구 {}",
        aliases.len(),
        host.providers.len(),
        host.servers.len() + host.remote_servers.len(),
        host.gateway.cli.accounts.len(),
        host.gateway.slack.workspaces.len(),
        host.gateway.http.endpoints.len(),
        host.gateway.http.openai.len()
    );
    match &settings.host_config {
        Some(path) => {
            Item::new(State::Pass, "호스트 설정", path.display().to_string()).with([counts])
        }
        None => Item::new(
            State::Warn,
            "호스트 설정",
            "BOT_HOST_CONFIG·--host가 없어 내장 데모 호스트를 써요(운영 설정이 아니에요)",
        )
        .with([
            counts,
            "실제 설정은 `--host <host.json>`이나 BOT_HOST_CONFIG로 지정해요".to_owned(),
        ]),
    }
}

/// 상류 주소에서 호스트(권한부)만 보여 준다. 경로·쿼리는 출력하지 않는다.
fn authority(endpoint: &str) -> &str {
    let rest = endpoint
        .split_once("://")
        .map_or(endpoint, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or_default()
}

/// API 직접 호출 별칭의 한 줄: 키 이름(값 아님)과 상류 호스트만 보인다. 키는 loopback 주소에서만 생략할 수 있다(규칙은 `check_model_aliases`가 판정한다).
/// 출력 한도는 적었을 때만 보이고, 기본 필드(`max_tokens`)가 아니면 상류에 보내는 이름도 함께 보인다.
fn api_line(
    alias: &str,
    kind: &str,
    (model, endpoint): (&str, &str),
    secret: Option<&str>,
    (max_tokens, field): (Option<u32>, LimitField),
) -> String {
    let key = secret.map_or_else(
        || "서비스 키 없음(loopback 전용)".to_owned(),
        |name| format!("자격 이름 {name}"),
    );
    let limit = max_tokens.map_or_else(String::new, |tokens| match field {
        LimitField::MaxTokens => format!(" · 최대 토큰 {tokens}"),
        other => format!(" · 최대 토큰 {tokens}({})", other.name()),
    });
    format!(
        "{alias}: {kind} · 모델 {model} · 상류 {} · {key}{limit}",
        authority(endpoint)
    )
}

fn providers(host: &HostFile) -> Item {
    let mut lines = Vec::new();
    for (alias, provider) in &host.providers {
        // 새 제공자 종류는 여기에 줄을 더한다(빠뜨리면 컴파일이 막는다)
        lines.push(match provider {
            Provider::Demo => format!("{alias}: demo(외부 호출 없는 데모 모델)"),
            Provider::ClaudeCli {
                model,
                endpoint,
                secret,
            } => format!(
                "{alias}: claude-cli · 모델 {model} · 상류 {} · 자격 이름 {secret}",
                authority(endpoint)
            ),
            Provider::AnthropicApi {
                model,
                endpoint,
                secret,
                max_tokens,
            } => api_line(
                alias,
                "anthropic-api",
                (model, endpoint),
                secret.as_deref(),
                (*max_tokens, LimitField::MaxTokens),
            ),
            Provider::OpenAiApi {
                model,
                endpoint,
                secret,
                max_tokens,
                limit_field,
            } => api_line(
                alias,
                "openai-api",
                (model, endpoint),
                secret.as_deref(),
                (*max_tokens, *limit_field),
            ),
        });
    }
    // 프로필이 허용한 별칭에 실행 방법이 없으면 worker가 시작하지 않는다(그 판정을 그대로 쓴다)
    let problems = serve::check_model_aliases(host).err();
    let state = if problems.is_none() {
        State::Pass
    } else {
        State::Fail
    };
    Item::new(state, "모델 제공자", format!("{}개", host.providers.len()))
        .with(problems)
        .with(capped(lines, 20))
}

/// 도구 서버: 설치 폴더(로컬)와 주소(원격)에서 판본을 계산하고(`serve`·`bot validate`가 시작 전에 하는 `measure_all`), worker가 거절하는 설정
/// (`state: required`)과 원격 서버의 IP 리터럴 정책(연결 때 쓰는 `serve::remote::literal_ok`)도 같은 함수로 거른다. 접속하지 않으므로 이름 풀이와
/// 연결은 확인하지 않는다.
fn servers(host: &HostFile) -> (Item, Option<BTreeMap<String, Revision>>) {
    if host.servers.is_empty() && host.remote_servers.is_empty() {
        return (
            Item::new(State::Pass, "도구 서버", "등록된 서버가 없어요"),
            Some(BTreeMap::new()),
        );
    }
    let measured = install::measure_all(&host.servers, &host.remote_servers);
    let mut problems = Vec::new();
    if let Err(problem) = &measured {
        problems.push(problem.clone());
        problems.push(
            "설치 폴더(root)가 절대 경로의 실제 폴더이고 실행 파일이 그 안에 있는지 확인해 주세요"
                .to_owned(),
        );
    }
    if let Err(problem) = serve::refuse_required_state(&host.servers) {
        problems.push(problem);
    }
    let mut lines = Vec::new();
    for (alias, remote) in &host.remote_servers {
        let literal = remote.host.parse::<std::net::IpAddr>().is_ok();
        if literal && !literal_ok(&remote.host, remote.tls, false) {
            problems.push(format!(
                "servers.{alias}: 연결할 수 없는 IP 주소예요 — https는 공개 주소여야 하고 http는 loopback 주소만 돼요(사설·예약 주소는 거절해요)"
            ));
        }
        let auth = remote.bearer.as_ref().map_or_else(
            || "인증 없음".to_owned(),
            |name| format!("bearer 자격 이름 {}", name.as_str()),
        );
        let scheme = if remote.tls { "https" } else { "http" };
        let resolve = if literal {
            ""
        } else {
            " · 이름 풀이는 접속할 때 검사해요"
        };
        lines.push(format!(
            "{alias}: 원격 {scheme} · {}:{} · {auth}{resolve}",
            remote.host, remote.port
        ));
    }
    if !problems.is_empty() {
        let item =
            Item::new(State::Fail, "도구 서버", "시작할 수 없는 설정이 있어요").with(problems);
        return (item, measured.ok());
    }
    let revisions = measured.unwrap_or_default();
    lines.extend(
        host.servers
            .keys()
            .filter_map(|alias| revisions.get(alias).map(|revision| (alias, revision)))
            .map(|(alias, revision)| {
                format!(
                    "{alias}: 로컬 판본 {}",
                    revision.0.get(..12).unwrap_or(&revision.0)
                )
            }),
    );
    let item = Item::new(
        State::Pass,
        "도구 서버",
        format!(
            "{}개 — 판본을 계산했어요(서버는 시작하지 않고 접속하지 않아요)",
            revisions.len()
        ),
    )
    .with(capped(lines, 20));
    (item, Some(revisions))
}
