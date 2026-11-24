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
    profile::{Adapter, Policy},
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
    let bindings = parse_bindings(root.get("bindings"), limits, issues);
    (issues.error_count() == before).then(|| ProfileParts {
        id: id.to_owned(),
        timezone,
        model: text(root, "model").to_owned(),
        roles,
        members,
        bindings,
        secrets: strings(root.get("secrets")),
        policy,
    })
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
    for (i, item) in list.iter().enumerate() {
        let at = format!("/bindings/{i}");
        let Some(obj) = item.as_object() else {
            continue;
        };
        let kind = match (text(obj, "adapter"), obj.contains_key("dm")) {
            ("cli", _) => Adapter::Cli {
                account: text(obj, "account").to_owned(),
            },
            ("slack", true) => Adapter::SlackDm {
                workspace: text(obj, "workspace").to_owned(),
            },
            ("slack", false) => Adapter::SlackChannel {
                workspace: text(obj, "workspace").to_owned(),
                channel: text(obj, "channel").to_owned(),
            },
            _ => Adapter::Http {
                endpoint: text(obj, "endpoint").to_owned(),
            },
        };
        let id = BindingId(text(obj, "id").to_owned());
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
