//! Cross-file reference, conflict, trigger ordering, and private DM checks.
use crate::{
    issue::Issues,
    layout::agent_dir,
    model::{
        Agent, Binding, Built, ProfileParts, Source, Tool, Trigger, TriggerTarget, adapter_name,
    },
    names,
    parse::SkillMode,
};
use bot_core::{
    id::{AgentId, BindingId, PrincipalId},
    profile::{Adapter, forbidden_env_name},
    slack::SlackEvent,
    state::Risk,
};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

type Agents = BTreeMap<AgentId, Agent>;

pub fn resolve(built: &Built, mode: SkillMode, issues: &mut Issues) {
    let (p, agents) = (&built.parts, &built.agents);
    let role_scopes: BTreeSet<String> = p.roles.values().flatten().cloned().collect();
    let mut universe = role_scopes.clone();
    for tool in agents.values().flat_map(Agent::tools) {
        universe.extend(tool.scopes.iter().cloned());
    }
    bindings(p, agents, issues);
    policy(p, agents, &universe, issues);
    tools(p, agents, &role_scopes, mode, issues);
    tool_env(agents, issues);
    memory(p, agents, issues);
    triggers(p, agents, issues);
}

fn bindings(p: &ProfileParts, agents: &Agents, issues: &mut Issues) {
    for binding in p.bindings.values() {
        if !agents.contains_key(&binding.agent) {
            issues.error_at(
                "unknown-agent",
                "profile.yaml",
                format!("binding {}", binding.id.0),
                format!("Agent `{}` does not exist", binding.agent.0),
            );
        }
        match &binding.kind {
            Adapter::Cli { account }
                if !p
                    .members
                    .contains_key(&PrincipalId(format!("local:{account}"))) =>
            {
                issues.warning(
                    "binding-no-member",
                    "profile.yaml",
                    format!(
                        "binding {}: no member `local:{account}` matches CLI account `{account}`, so nobody can use this binding",
                        binding.id.0
                    ),
                );
            }
            Adapter::OpenAi { .. } => crate::openai::check(p, agents, binding, issues),
            _ => {}
        }
    }
}

fn policy(p: &ProfileParts, agents: &Agents, universe: &BTreeSet<String>, issues: &mut Issues) {
    let check = |scopes: &BTreeSet<String>, at: String, issues: &mut Issues| {
        for scope in scopes.iter().filter(|s| !universe.contains(*s)) {
            issues.error_at(
                "unknown-scope",
                "profile.yaml",
                at.clone(),
                format!("Scope is not declared by any role or tool: {scope}"),
            );
        }
    };
    check(&p.policy.scopes, "/policy/scopes".into(), issues);
    check(&p.policy.approve, "/policy/approve".into(), issues);
    for (agent, scopes) in &p.policy.agents {
        if !agents.contains_key(agent) {
            issues.error_at(
                "unknown-agent",
                "profile.yaml",
                format!("/policy/agents/{}", agent.0),
                format!("Agent `{}` does not exist", agent.0),
            );
        }
        check(scopes, format!("/policy/agents/{}", agent.0), issues);
    }
}

fn tools(
    p: &ProfileParts,
    agents: &Agents,
    role_scopes: &BTreeSet<String>,
    mode: SkillMode,
    issues: &mut Issues,
) {
    let mut seen = BTreeSet::new();
    for skill in agents.values().flat_map(|a| a.skills.iter()) {
        if !seen.insert(skill.path.clone()) {
            continue;
        }
        let file = format!("{}/tools.yaml", skill.path);
        for tool in &skill.tools {
            for (env, secret) in &tool.secrets {
                // Legacy (`BOTPROFILE1`) restore allows historical keys; broker still blocks reserved env names at runtime.
                if mode == SkillMode::Strict && forbidden_env_name(env) {
                    issues.error_at(
                        "env-reserved",
                        &file,
                        format!("{} secrets/{env}", tool.name),
                        format!(
                            "`{env}` is reserved by loader/shell/proxy/host and cannot be used as a secret env name"
                        ),
                    );
                }
                if !p.secrets.contains(secret.as_str()) {
                    issues.error_at(
                        "unknown-secret",
                        &file,
                        format!("{} secrets/{env}", tool.name),
                        format!(
                            "Secret `{}` is not declared in profile.yaml secrets",
                            secret.as_str()
                        ),
                    );
                }
            }
            if let Some(scope) = tool.scopes.iter().find(|s| !role_scopes.contains(*s)) {
                issues.warning(
                    "tool-scope-ungranted",
                    &file,
                    format!(
                        "{}: no role grants scope `{scope}`, so nobody can call this tool",
                        tool.name
                    ),
                );
            }
        }
    }
}

/// Tool `env` keys come from each calling agent's inherited `env.yaml`, so every agent that sees the tool must define them.
fn tool_env(agents: &Agents, issues: &mut Issues) {
    let mut checked = BTreeSet::new();
    let mut missing: BTreeMap<(String, String, String), Vec<&str>> = BTreeMap::new();
    for agent in agents.values() {
        for skill in &agent.skills {
            for tool in skill.tools.iter().filter(|t| !t.env.is_empty()) {
                if checked.insert((skill.path.clone(), tool.name.clone())) {
                    let file = format!("{}/tools.yaml", skill.path);
                    for key in &tool.env {
                        if forbidden_env_name(key) {
                            issues.error_at(
                                "env-reserved",
                                &file,
                                format!("{} env/{key}", tool.name),
                                format!(
                                    "`{key}` is reserved by loader/shell/proxy/host and cannot be selected in tool env"
                                ),
                            );
                        }
                        if tool.secrets.contains_key(key) {
                            issues.error_at(
                                "env-secret-conflict",
                                &file,
                                format!("{} env/{key}", tool.name),
                                format!(
                                    "`{key}` conflicts with a secret env name declared on this tool"
                                ),
                            );
                        }
                    }
                }
                for key in tool.env.iter().filter(|k| !agent.env.contains_key(*k)) {
                    missing
                        .entry((skill.path.clone(), tool.name.clone(), key.clone()))
                        .or_default()
                        .push(agent.id.0.as_str());
                }
            }
        }
    }
    for ((path, tool, key), callers) in missing {
        issues.error_at(
            "env-missing",
            &format!("{path}/tools.yaml"),
            format!("{tool} env/{key}"),
            format!(
                "`{key}` is missing from env.yaml (including inherited env) for agent(s) {}",
                callers.join(", ")
            ),
        );
    }
}

fn memory(p: &ProfileParts, agents: &Agents, issues: &mut Issues) {
    for seed in agents.values().flat_map(|a| a.memory.iter()) {
        if !p.members.contains_key(&seed.owner) {
            issues.error_at(
                "memory-owner-unknown",
                &seed.document.relative_path,
                "front matter /owner",
                format!(
                    "owner `{}` is not declared in profile.yaml members",
                    seed.owner.0
                ),
            );
        }
    }
}

struct Candidate<'a> {
    trigger: &'a Trigger,
    agent: &'a AgentId,
    file: String,
}

fn triggers(p: &ProfileParts, agents: &Agents, issues: &mut Issues) {
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for agent in agents.values() {
        let file = format!("{}/triggers.yaml", agent_dir(&agent.id.0));
        for trigger in &agent.triggers {
            trigger_refs(p, agent, trigger, &file, issues);
            if let Some(group) = trigger.on.group() {
                groups.entry(group).or_default().push(Candidate {
                    trigger,
                    agent: &agent.id,
                    file: file.clone(),
                });
            }
        }
    }
    for list in groups.values() {
        let mut by_order: BTreeMap<i64, &Candidate> = BTreeMap::new();
        for c in list {
            if let Some(first) = by_order.insert(c.trigger.order, c) {
                issues.error_at(
                    "duplicate-order",
                    &c.file,
                    format!("trigger {}", c.trigger.id),
                    format!(
                        "order {} collides with `{}` (agent {}) in the same candidate group",
                        c.trigger.order, first.trigger.id, first.agent.0
                    ),
                );
            }
        }
        for c in list.iter().filter(|c| c.trigger.private) {
            if let Some(early) = list
                .iter()
                .find(|o| !o.trigger.private && o.trigger.order < c.trigger.order)
            {
                issues.error_at(
                    "private-preempted",
                    &c.file,
                    format!("trigger {}", c.trigger.id),
                    format!(
                        "non-private trigger `{}` (order {}) may match first; private triggers must have the lowest order",
                        early.trigger.id, early.trigger.order
                    ),
                );
            }
        }
    }
}

fn trigger_refs(p: &ProfileParts, agent: &Agent, t: &Trigger, file: &str, issues: &mut Issues) {
    let at = format!("trigger {}", t.id);
    let binding =
        t.on.binding()
            .and_then(|id| binding_of(p, id, t, file, &at, issues));
    if t.private_pass && !t.private {
        issues.error_at(
            "private-pass-needs-private",
            file,
            &at,
            "private_pass can only be set on private triggers",
        );
    }
    if t.private
        && t.on.binding().is_some()
        && !matches!(binding.map(|b| &b.kind), Some(Adapter::SlackDm { .. }))
    {
        issues.error_at(
            "private-needs-dm",
            file,
            &at,
            "private triggers can only use Slack DM bindings (`dm: true`)",
        );
    }
    let tool = match &t.target {
        TriggerTarget::Tool { tool, args } => match agent.tool(tool) {
            Some(found) => {
                static_args(found, args, t, file, &at, issues);
                Some(found.clone())
            }
            None => {
                issues.error_at(
                    "unknown-tool",
                    file,
                    &at,
                    format!("Tool `{}` is not visible to agent {}", tool.0, agent.id.0),
                );
                None
            }
        },
        TriggerTarget::Instruction(_) => None,
    };
    crate::approval::check(p, t, tool.as_deref(), binding, (file, &at), issues);
    if t.private
        && let Some(found) = &tool
    {
        if needs_approval(p, found) {
            issues.error_at(
                "private-approval",
                file,
                &at,
                "private trigger tool requires approval, which would expose sensitive content on an approval card; use a tool that does not require approval",
            );
        }
        if found.effects {
            issues.error_at(
                "private-effects",
                file,
                &at,
                "private trigger tool has `effects: true`, which may record sensitive content in receipts or step payloads; use a single-step tool without effects",
            );
        }
    }
    if let Source::Cron {
        principal,
        approver,
        ..
    } = &t.on
    {
        cron_principal(
            p,
            agent,
            (principal, approver.as_ref()),
            (t, tool.as_deref()),
            file,
            &at,
            issues,
        );
    }
}

fn binding_of<'a>(
    p: &'a ProfileParts,
    id: &BindingId,
    t: &Trigger,
    file: &str,
    at: &str,
    issues: &mut Issues,
) -> Option<&'a Binding> {
    let Some(binding) = p.bindings.get(id) else {
        issues.error_at(
            "unknown-binding",
            file,
            at,
            format!("Binding `{}` is not declared in profile.yaml", id.0),
        );
        return None;
    };
    if matches!(binding.kind, Adapter::OpenAi { .. }) {
        issues.error_at(
            "openai-binding-no-trigger",
            file,
            at,
            format!(
                "OpenAI model binding `{}` cannot be used by triggers; selecting the model routes directly to its agent (use an http webhook binding without `model` for webhook triggers)",
                id.0
            ),
        );
        return Some(binding);
    }
    let ok = match &t.on {
        Source::Message { .. } => matches!(
            binding.kind,
            Adapter::SlackChannel { .. } | Adapter::SlackDm { .. } | Adapter::TelegramDm { .. }
        ),
        Source::ThreadReply { .. } | Source::Reaction { .. } => {
            matches!(binding.kind, Adapter::SlackChannel { .. })
        }
        Source::Http { .. } => matches!(binding.kind, Adapter::Http { .. }),
        Source::Cron { .. } => true,
    };
    if !ok {
        let want = match t.on {
            Source::Http { .. } => "http",
            Source::ThreadReply { .. } | Source::Reaction { .. } => "slack channel",
            _ => "slack or telegram",
        };
        issues.error_at(
            "binding-kind-mismatch",
            file,
            at,
            format!(
                "this event requires a {want} binding, but `{}` is {}",
                id.0,
                adapter_name(&binding.kind)
            ),
        );
    } else if let Adapter::SlackChannel { events, .. } = &binding.kind {
        let needs = match &t.on {
            Source::Message { .. } => Some(SlackEvent::Mention),
            Source::ThreadReply { .. } => Some(SlackEvent::ThreadReply),
            Source::Reaction { .. } => Some(SlackEvent::Reaction),
            _ => None,
        };
        if let Some(need) = needs.filter(|need| !events.contains(need)) {
            issues.error_at(
                "event-not-enabled",
                file,
                at,
                format!(
                    "binding `{}` does not enable `{}` in `events` for trigger event `{}`; add `{}` to `events` in profile.yaml",
                    id.0,
                    need.as_str(),
                    need.trigger_event(),
                    need.as_str()
                ),
            );
        }
    }
    Some(binding)
}

fn static_args(
    tool: &Tool,
    fixed: &Map<String, Value>,
    t: &Trigger,
    file: &str,
    at: &str,
    issues: &mut Issues,
) {
    let doc = tool.schema.doc();
    let props = doc.get("properties").and_then(Value::as_object);
    let open = doc.get("additionalProperties") != Some(&Value::Bool(false));
    for key in t.input.keys().filter(|k| fixed.contains_key(*k)) {
        issues.error_at(
            "input-key-conflict",
            file,
            at,
            format!("static args and input both set argument `{key}`"),
        );
    }
    for key in fixed.keys().chain(t.input.keys()) {
        if !open && !props.is_some_and(|p| p.contains_key(key)) {
            issues.error_at(
                "input-unknown-arg",
                file,
                at,
                format!("argument `{key}` is not in tool {} input schema", tool.id.0),
            );
        }
    }
    let required = doc
        .get("required")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for key in required.iter().filter_map(Value::as_str) {
        if !fixed.contains_key(key) && !t.input.contains_key(key) {
            issues.error_at(
                "input-missing-required",
                file,
                at,
                format!("required argument `{key}` is not provided by args or input"),
            );
        }
    }
    if t.input.is_empty() {
        for v in tool.schema.validate(&Value::Object(fixed.clone())) {
            issues.error_at(
                "args-invalid",
                file,
                at,
                format!(
                    "static args do not match tool input schema: {} {}",
                    v.pointer, v.message
                ),
            );
        }
    }
}

fn cron_principal(
    p: &ProfileParts,
    agent: &Agent,
    (principal, approver): (&PrincipalId, Option<&PrincipalId>),
    (t, tool): (&Trigger, Option<&Tool>),
    file: &str,
    at: &str,
    issues: &mut Issues,
) {
    let Some(member) = p.members.get(principal) else {
        issues.error_at(
            "trigger-principal-unknown",
            file,
            at,
            format!(
                "principal `{}` is not declared in profile.yaml members",
                principal.0
            ),
        );
        return;
    };
    if member.kind != names::IdentityKind::Service {
        issues.error_at(
            "cron-needs-service",
            file,
            at,
            format!(
                "cron triggers must run as a service principal (`service:*`): {}",
                principal.0
            ),
        );
    }
    let effective: BTreeSet<&String> = member.scopes.intersection(&agent.ceiling).collect();
    let needed: Vec<&str> = match (tool, &t.target) {
        (Some(tool), _) => tool.scopes.iter().map(String::as_str).collect(),
        (None, TriggerTarget::Instruction(_)) => vec!["run"],
        (None, TriggerTarget::Tool { .. }) => vec![],
    };
    for scope in needed
        .into_iter()
        .filter(|s| !effective.contains(&(*s).to_owned()))
    {
        issues.error_at(
            "trigger-scope",
            file,
            at,
            format!(
                "principal `{}` (capped by agent {}) lacks scope `{scope}`",
                principal.0, agent.id.0
            ),
        );
    }
    // Operator-preapproved cron triggers may omit a human approver.
    let asks = tool.is_some_and(|tool| needs_approval(p, tool)) && t.preapproved.is_none();
    match (approver, asks) {
        (None, true) => issues.error_at(
            "approver-required",
            file,
            at,
            "cron triggers calling a tool that requires approval must specify an approver (registered human principal)",
        ),
        (Some(who), _) => match p.members.get(who) {
            None => issues.error_at(
                "approver-unknown",
                file,
                at,
                format!("approver `{}` is not declared in members", who.0),
            ),
            Some(m) if m.kind == names::IdentityKind::Service => issues.error_at(
                "approver-service",
                file,
                at,
                "service principals cannot approve actions",
            ),
            Some(m) if !m.kind.is_human() => issues.error_at(
                "approver-not-human",
                file,
                at,
                format!(
                    "approver must be a human principal (`local:*` or `slack:*`); `{}` is {:?}",
                    who.0, m.kind
                ),
            ),
            Some(_) => {}
        },
        (None, false) => {}
    }
}

fn needs_approval(p: &ProfileParts, tool: &Tool) -> bool {
    match tool.risk() {
        Risk::Irreversible => true,
        Risk::Write => tool.scopes.iter().any(|s| p.policy.approve.contains(s)),
        Risk::Read => false,
    }
}
