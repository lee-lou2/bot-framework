//! Parsing and local validation of `profile.yaml` (cross-file references are checked in `resolve`).
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
            format!("id `{id}` must match the folder name `{folder}`"),
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
            format!("not a canonical IANA timezone: {timezone}"),
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
                format!("identity must have the form `<kind>:<name>`: {identity}"),
            );
            continue;
        };
        if kind == names::IdentityKind::Service {
            issues.error_at(
                "super-admin-service",
                "profile.yaml",
                at,
                format!("service principals cannot be super admins: {identity}"),
            );
            continue;
        }
        let principal = PrincipalId(identity.to_owned());
        if !members.contains_key(&principal) {
            issues.error_at(
                "super-admin-not-member",
                "profile.yaml",
                at,
                format!("super admin `{identity}` is not listed in members"),
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
            format!("too many members (max {})", limits.max_members),
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
                format!("identity must have the form `<kind>:<name>`: {identity}"),
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
                    format!("undeclared role: {role}"),
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
                format!("duplicate member identity: {identity}"),
            );
        }
    }
    out
}

/// Maps HTTP requester principals to approval routes (`binding` selects Slack DM; otherwise local).
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

/// Parses channel `events`, defaulting to mention-only when omitted.
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
            format!("too many bindings (max {})", limits.max_bindings),
        );
        return BTreeMap::new();
    }
    let mut out: BTreeMap<BindingId, Binding> = BTreeMap::new();
    let mut entrances: BTreeMap<String, String> = BTreeMap::new();
    // Endpoint alias -> (is_openai, first_binding_id); webhook and OpenAI modes cannot share an alias.
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
            ("telegram", true, _) => Adapter::TelegramDm {
                bot: text(obj, "bot").to_owned(),
            },
            // `http` with `model` is an OpenAI-compatible binding; without `model` it is a JWT webhook.
            ("http", _, true) => Adapter::OpenAi {
                endpoint: text(obj, "endpoint").to_owned(),
                model: text(obj, "model").to_owned(),
                approvers: parse_approvers(obj.get("approvers")),
            },
            ("http", _, false) => Adapter::Http {
                endpoint: text(obj, "endpoint").to_owned(),
            },
            // Guard against schema/parser drift instead of silently falling back to webhook.
            (other, ..) => {
                issues.error_at(
                    "unknown-adapter",
                    "profile.yaml",
                    format!("{at}/adapter"),
                    format!(
                        "unknown adapter `{other}` (expected cli, slack, telegram, or http); schema and parser are out of sync"
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
                        "HTTP endpoint `{endpoint}` mixes webhook and OpenAI model bindings (`{first}` and `{}`). {}",
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
                    "entrance `{key}` is shared by bindings `{first}` and `{}`. {}",
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
                format!("duplicate binding id: {}", id.0),
            );
        }
    }
    out
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
