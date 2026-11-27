//! profile.yaml 해석. 다른 파일을 봐야 정해지는 참조(에이전트·스코프 존재 등)는 resolve가 맡는다.
use crate::{
    hint,
    issue::Issues,
    limits::Limits,
    model::{Binding, Member, ProfileParts, entrance},
    names,
};
use bot_core::{
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, ApprovalRoute, Policy},
    slack::{SlackEvent, default_channel_events},
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> &'a str {
    map.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn strings(value: Option<&Value>) -> BTreeSet<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 스키마를 통과한 값에서 꺼낸다. 의미 검사 실패는 issues에 쌓고 None을 돌려준다.
pub fn parse(
    value: &Value,
    folder: Option<&str>,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<ProfileParts> {
    let path = "profile.yaml";
    let root = value.as_object()?;
    let before = issues.error_count();
    let id = text(root, "id");
    if let Some(folder) = folder.filter(|f| *f != id) {
        issues.error_at(
            "profile-id-mismatch",
            path,
            "/id",
            format!("id `{id}`는 폴더 이름 `{folder}`와 같아야 해요"),
        );
    }
    let timezone = root
        .get("timezone")
        .and_then(Value::as_str)
        .unwrap_or("UTC")
        .to_owned();
    if jiff_tzdb::get(&timezone).is_none_or(|(canonical, _)| canonical != timezone) {
        issues.error_at(
            "bad-timezone",
            path,
            "/timezone",
            format!("IANA 시간대가 아니에요: {timezone}"),
        );
    }
    let roles: BTreeMap<String, BTreeSet<String>> = root
        .get("roles")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), strings(Some(v))))
                .collect()
        })
        .unwrap_or_default();
    let role_scopes: BTreeSet<String> = roles.values().flatten().cloned().collect();
    let policy = parse_policy(root.get("policy"), &role_scopes);
    let members = parse_members(root.get("members"), &roles, &policy, limits, issues);
    let super_admins = parse_super_admins(root.get("super_admins"), &members, issues);
    let bindings = parse_bindings(root.get("bindings"), limits, issues);
    (issues.error_count() == before).then(|| ProfileParts {
        id: id.to_owned(),
        timezone,
        model: text(root, "model").to_owned(),
        roles,
        members,
        super_admins,
        bindings,
        secrets: strings(root.get("secrets")),
        policy,
    })
}

fn parse_super_admins(
    value: Option<&Value>,
    members: &BTreeMap<PrincipalId, Member>,
    issues: &mut Issues,
) -> BTreeSet<PrincipalId> {
    let list = value
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut out = BTreeSet::new();
    for (i, item) in list.iter().enumerate() {
        let Some(identity) = item.as_str() else {
            continue;
        };
        let at = format!("/super_admins/{i}");
        let Some(kind) = names::identity_kind(identity) else {
            issues.error_at(
                "bad-identity",
                "profile.yaml",
                at,
                format!("신원은 `<종류>:<이름>` 모양이어야 해요: {identity}"),
            );
            continue;
        };
        if kind == names::IdentityKind::Service {
            issues.error_at(
                "super-admin-service",
                "profile.yaml",
                at,
                format!("서비스 주체는 슈퍼 관리자가 될 수 없어요: {identity}"),
            );
            continue;
        }
        let principal = PrincipalId(identity.to_owned());
        if !members.contains_key(&principal) {
            issues.error_at(
                "super-admin-not-member",
                "profile.yaml",
                at,
                format!("슈퍼 관리자 `{identity}`가 members에 없어요"),
            );
            continue;
        }
        out.insert(principal);
    }
    out
}

fn parse_policy(value: Option<&Value>, role_scopes: &BTreeSet<String>) -> Policy {
    let empty = Map::new();
    let obj = value.and_then(Value::as_object).unwrap_or(&empty);
    Policy {
        scopes: obj
            .get("scopes")
            .map_or_else(|| role_scopes.clone(), |v| strings(Some(v))),
        agents: obj
            .get("agents")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (AgentId(k.clone()), strings(Some(v))))
                    .collect()
            })
            .unwrap_or_default(),
        approve: strings(obj.get("approve")),
    }
}

fn parse_members(
    value: Option<&Value>,
    roles: &BTreeMap<String, BTreeSet<String>>,
    policy: &Policy,
    limits: &Limits,
    issues: &mut Issues,
) -> BTreeMap<PrincipalId, Member> {
    let list = value
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if list.len() > limits.max_members {
        issues.error_at(
            "too-many",
            "profile.yaml",
            "/members",
            format!("멤버가 너무 많아요(최대 {})", limits.max_members),
        );
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    for (i, item) in list.iter().enumerate() {
        let at = format!("/members/{i}");
        let Some(obj) = item.as_object() else {
            continue;
        };
        let identity = text(obj, "identity");
        let Some(kind) = names::identity_kind(identity) else {
            issues.error_at(
                "bad-identity",
                "profile.yaml",
                format!("{at}/identity"),
                format!("신원은 `<종류>:<이름>` 모양이어야 해요: {identity}"),
            );
            continue;
        };
        let member_roles = strings(obj.get("roles"));
        let mut scopes = BTreeSet::new();
        for role in &member_roles {
            match roles.get(role) {
                Some(granted) => scopes.extend(granted.iter().cloned()),
                None => issues.error_at(
                    "unknown-role",
                    "profile.yaml",
                    format!("{at}/roles"),
                    format!("선언하지 않은 역할이에요: {role}"),
                ),
            }
        }
        let principal = PrincipalId(identity.to_owned());
        let member = Member {
            identity: principal.clone(),
            kind,
            roles: member_roles,
            scopes: scopes.intersection(&policy.scopes).cloned().collect(),
        };
        if out.insert(principal, member).is_some() {
            issues.error_at(
                "duplicate-member",
                "profile.yaml",
                at,
                format!("같은 신원이 두 번 나와요: {identity}"),
            );
        }
    }
    out
}

/// OpenAI 바인딩의 `approvers`: HTTP 주체 → 승인 경로. `binding`이 있으면 Slack DM 경로, 없으면 로컬 경로(스키마가 둘을 가른다).
fn parse_approvers(value: Option<&Value>) -> BTreeMap<PrincipalId, ApprovalRoute> {
    let Some(map) = value.and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    map.iter()
        .filter_map(|(requester, route)| {
            let route = route.as_object()?;
            let person = PrincipalId(text(route, "principal").to_owned());
            let route = match route.get("binding").and_then(Value::as_str) {
                Some(binding) => ApprovalRoute::Slack {
                    person,
                    binding: BindingId(binding.to_owned()),
                },
                None => ApprovalRoute::Local { person },
            };
            Some((PrincipalId(requester.clone()), route))
        })
        .collect()
}

/// 채널 바인딩의 `events`. 스키마가 값(`mention`·`thread_reply`·`reaction`)·중복·빈 목록을 이미 거른다. 생략하면 멘션만(기존 동작).
fn channel_events(obj: &Map<String, Value>) -> BTreeSet<SlackEvent> {
    let parsed: BTreeSet<SlackEvent> = obj
        .get("events")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(SlackEvent::parse)
        .filter(|event| *event != SlackEvent::Dm)
        .collect();
    if parsed.is_empty() {
        default_channel_events()
    } else {
        parsed
    }
}

fn parse_bindings(
    value: Option<&Value>,
    limits: &Limits,
    issues: &mut Issues,
) -> BTreeMap<BindingId, Binding> {
    let list = value
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if list.len() > limits.max_bindings {
        issues.error_at(
            "too-many",
            "profile.yaml",
            "/bindings",
            format!("바인딩이 너무 많아요(최대 {})", limits.max_bindings),
        );
        return BTreeMap::new();
    }
    let mut out: BTreeMap<BindingId, Binding> = BTreeMap::new();
    let mut entrances: BTreeMap<String, String> = BTreeMap::new();
    // HTTP 별칭 → (OpenAI 모델 바인딩인가, 처음 쓴 바인딩 id). 한 별칭에는 두 방식을 섞지 않는다.
    let mut http_kinds: BTreeMap<String, (bool, String)> = BTreeMap::new();
    for (i, item) in list.iter().enumerate() {
        let at = format!("/bindings/{i}");
        let Some(obj) = item.as_object() else {
            continue;
        };
        let kind = match (
            text(obj, "adapter"),
            obj.contains_key("dm"),
            obj.contains_key("model"),
        ) {
            ("cli", ..) => Adapter::Cli {
                account: text(obj, "account").to_owned(),
            },
            ("slack", true, _) => Adapter::SlackDm {
                workspace: text(obj, "workspace").to_owned(),
            },
            ("slack", false, _) => Adapter::SlackChannel {
                workspace: text(obj, "workspace").to_owned(),
                channel: text(obj, "channel").to_owned(),
                events: channel_events(obj),
            },
            // `http`는 웹훅(JWT)이고, `model`이 있으면 OpenAI 호환 모델 바인딩이다.
            ("http", _, true) => Adapter::OpenAi {
                endpoint: text(obj, "endpoint").to_owned(),
                model: text(obj, "model").to_owned(),
                approvers: parse_approvers(obj.get("approvers")),
            },
            ("http", _, false) => Adapter::Http {
                endpoint: text(obj, "endpoint").to_owned(),
            },
            // 스키마가 먼저 거르는 값이다. 스키마에 새 입구를 더하고 여기를 빠뜨려도 조용히 웹훅으로 읽지 않고 이름을 밝혀 거절한다.
            (other, ..) => {
                issues.error_at(
                    "unknown-adapter",
                    "profile.yaml",
                    format!("{at}/adapter"),
                    format!(
                        "입구 종류 `{other}`를 알 수 없어요(cli · slack · http). 스키마가 먼저 거르는 값이라, 여기까지 왔다면 스키마와 이 해석기가 어긋난 거예요"
                    ),
                );
                continue;
            }
        };
        let id = BindingId(text(obj, "id").to_owned());
        if let Adapter::Http { endpoint } | Adapter::OpenAi { endpoint, .. } = &kind {
            let openai = matches!(kind, Adapter::OpenAi { .. });
            let (first_openai, first) = http_kinds
                .entry(endpoint.clone())
                .or_insert_with(|| (openai, id.0.clone()));
            if *first_openai != openai {
                issues.error_at(
                    "endpoint-kind-mixed",
                    "profile.yaml",
                    at,
                    format!(
                        "HTTP 입구 `{endpoint}`를 웹훅 바인딩과 OpenAI 모델 바인딩(`model` 있음)이 함께 써요(`{first}`와 `{}`). {}",
                        id.0,
                        hint::MIXED_ENDPOINT
                    ),
                );
                continue;
            }
        }
        let key = entrance(&kind);
        if let Some(first) = entrances.get(&key) {
            issues.error_at(
                "duplicate-entrance",
                "profile.yaml",
                at,
                format!(
                    "같은 입구 `{key}`를 바인딩 `{first}`와 `{}`가 함께 써요. {}",
                    id.0,
                    hint::entrance_fix(&key, false)
                ),
            );
            continue;
        }
        entrances.insert(key, id.0.clone());
        let binding = Binding {
            id: id.clone(),
            agent: AgentId(text(obj, "agent").to_owned()),
            kind,
        };
        if out.insert(id.clone(), binding).is_some() {
            issues.error_at(
                "duplicate-binding-id",
                "profile.yaml",
                format!("/bindings/{i}/id"),
                format!("바인딩 id가 겹쳐요: {}", id.0),
            );
        }
    }
    out
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
