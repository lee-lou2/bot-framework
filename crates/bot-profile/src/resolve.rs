//! 참조·충돌 검사. 파일이 서로 가리키는 것(바인딩→에이전트, 트리거→바인딩·도구·주체, 정책→스코프 …)과
//! 같은 후보군의 순위·민감 DM 분기 규칙을 본다. 바이트만으로 정해지는 검사만 하며 호스트 등록부는 activate가 본다.
use crate::{
    issue::Issues,
    layout::agent_dir,
    model::{
        Agent, Binding, Built, ProfileParts, Source, Tool, Trigger, TriggerTarget, adapter_name,
    },
    names,
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

pub fn resolve(built: &Built, issues: &mut Issues) {
    let (p, agents) = (&built.parts, &built.agents);
    let role_scopes: BTreeSet<String> = p.roles.values().flatten().cloned().collect();
    let mut universe = role_scopes.clone();
    for tool in agents.values().flat_map(Agent::tools) {
        universe.extend(tool.scopes.iter().cloned());
    }
    bindings(p, agents, issues);
    policy(p, agents, &universe, issues);
    tools(p, agents, &role_scopes, issues);
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
                format!("바인딩 {}", binding.id.0),
                format!("에이전트 `{}`가 없어요", binding.agent.0),
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
                    format!("바인딩 {}: CLI 계정 `{account}`에 해당하는 멤버 `local:{account}`가 없어 아무도 쓸 수 없어요", binding.id.0),
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
                format!("어떤 역할·도구에도 없는 스코프예요: {scope}"),
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
                format!("에이전트 `{}`가 없어요", agent.0),
            );
        }
        check(scopes, format!("/policy/agents/{}", agent.0), issues);
    }
}

fn tools(p: &ProfileParts, agents: &Agents, role_scopes: &BTreeSet<String>, issues: &mut Issues) {
    let mut seen = BTreeSet::new();
    for skill in agents.values().flat_map(|a| a.skills.iter()) {
        if !seen.insert(skill.path.clone()) {
            continue;
        }
        let file = format!("{}/tools.yaml", skill.path);
        for tool in &skill.tools {
            for (env, secret) in &tool.secrets {
                if !p.secrets.contains(secret.as_str()) {
                    issues.error_at(
                        "unknown-secret",
                        &file,
                        format!("{} secrets/{env}", tool.name),
                        format!(
                            "비밀 `{}`가 profile.yaml의 secrets에 없어요",
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
                        "{}: 스코프 `{scope}`를 주는 역할이 없어 이 도구는 아무도 쓸 수 없어요",
                        tool.name
                    ),
                );
            }
        }
    }
}

/// 도구가 고른 일반 설정 이름(tools.yaml `env`)의 검사. 값은 호출하는 에이전트가 상속을 마친 env.yaml에서 오므로
/// 그 도구가 보이는 모든 에이전트의 env에 이름이 있어야 한다. 비밀의 환경 이름이나 호스트 예약 이름과는 겹칠 수 없다.
/// (같은 이름을 두 번 적는 것은 스키마가 거절한다.)
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
                                format!("`{key}`는 로더·셸·프록시·호스트가 쓰는 이름이라 도구 환경으로 고를 수 없어요"),
                            );
                        }
                        if tool.secrets.contains_key(key) {
                            issues.error_at(
                                "env-secret-conflict",
                                &file,
                                format!("{} env/{key}", tool.name),
                                format!("`{key}`는 이 도구의 비밀 환경 이름(secrets)이기도 해요"),
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
                "`{key}`가 에이전트 {}의 env.yaml(상속 포함)에 없어요",
                callers.join(" · ")
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
                format!("owner `{}`가 profile.yaml의 members에 없어요", seed.owner.0),
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
                    format!("트리거 {}", c.trigger.id),
                    format!(
                        "같은 후보군에서 순위 {}가 `{}`(에이전트 {})와 겹쳐요",
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
                    format!("트리거 {}", c.trigger.id),
                    format!("일반 트리거 `{}`(순위 {})가 먼저 잡을 수 있어요. private 트리거가 가장 앞서야 해요", early.trigger.id, early.trigger.order),
                );
            }
        }
    }
}

fn trigger_refs(p: &ProfileParts, agent: &Agent, t: &Trigger, file: &str, issues: &mut Issues) {
    let at = format!("트리거 {}", t.id);
    let binding =
        t.on.binding()
            .and_then(|id| binding_of(p, id, t, file, &at, issues));
    if t.private
        && t.on.binding().is_some()
        && !matches!(binding.map(|b| &b.kind), Some(Adapter::SlackDm { .. }))
    {
        issues.error_at(
            "private-needs-dm",
            file,
            &at,
            "private 트리거는 Slack DM 바인딩(`dm: true`)에만 쓸 수 있어요",
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
                    format!(
                        "도구 `{}`는 에이전트 {}에게 보이지 않아요",
                        tool.0, agent.id.0
                    ),
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
                "private 트리거의 도구가 승인 대상이면 승인 카드에 민감한 본문이 드러나요. 승인이 필요 없는 도구를 지정하세요",
            );
        }
        if found.effects {
            issues.error_at(
                "private-effects",
                file,
                &at,
                "private 트리거의 도구가 다단계(effects)면 자유형 영수증·단계 payload에 민감한 본문이 남을 수 있어요. effects 없는 단일 실행 도구를 지정하세요",
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
            format!("바인딩 `{}`가 profile.yaml에 없어요", id.0),
        );
        return None;
    };
    if matches!(binding.kind, Adapter::OpenAi { .. }) {
        issues.error_at(
            "openai-binding-no-trigger",
            file,
            at,
            format!("OpenAI 모델 바인딩 `{}`은 트리거가 쓸 수 없어요. 모델을 고르면 연결한 에이전트가 요청을 바로 받아요(웹훅 트리거는 `model`이 없는 http 바인딩에 연결하세요)", id.0),
        );
        return Some(binding);
    }
    let ok = match &t.on {
        Source::Message { .. } => matches!(
            binding.kind,
            Adapter::SlackChannel { .. } | Adapter::SlackDm { .. }
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
            Source::ThreadReply { .. } | Source::Reaction { .. } => "slack 채널",
            _ => "slack",
        };
        issues.error_at(
            "binding-kind-mismatch",
            file,
            at,
            format!(
                "이 사건은 {want} 바인딩이 필요한데 `{}`는 {}예요",
                id.0,
                adapter_name(&binding.kind)
            ),
        );
    } else if let Adapter::SlackChannel { events, .. } = &binding.kind {
        // 트리거의 사건을 이 바인딩이 받아야 한다: `events`에 그 종류가 없으면 트리거는 영영 불리지 않는다
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
                    "바인딩 `{}`은 `events`에 `{}`가 없어 이 트리거의 사건(`{}`)을 받지 못해요 — profile.yaml의 바인딩에 `events: [.., {}]`를 더하세요",
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

/// 도구 인자: 고정 args와 `input` 매핑이 스키마와 어긋나지 않는지를 호출 전에 가려낸다.
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
            format!("고정 args와 input이 같은 인자 `{key}`를 정해요"),
        );
    }
    for key in fixed.keys().chain(t.input.keys()) {
        if !open && !props.is_some_and(|p| p.contains_key(key)) {
            issues.error_at(
                "input-unknown-arg",
                file,
                at,
                format!("도구 {}의 input에 없는 인자 `{key}`예요", tool.id.0),
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
                format!("필수 인자 `{key}`를 정하는 곳이 없어요(args 또는 input)"),
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
                    "고정 args가 도구 input에 맞지 않아요: {} {}",
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
            format!("주체 `{}`가 profile.yaml의 members에 없어요", principal.0),
        );
        return;
    };
    if member.kind != names::IdentityKind::Service {
        issues.error_at(
            "cron-needs-service",
            file,
            at,
            format!(
                "cron은 서비스 주체(`service:*`)로 실행해요: {}",
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
                "주체 `{}`(에이전트 {} 상한 적용)에게 스코프 `{scope}`가 없어요",
                principal.0, agent.id.0
            ),
        );
    }
    // 운영자가 사전 승인한 트리거(`preapproved`)는 이 트리거에 한해 사람 approver를 생략할 수 있다
    let asks = tool.is_some_and(|tool| needs_approval(p, tool)) && t.preapproved.is_none();
    match (approver, asks) {
        (None, true) => issues.error_at(
            "approver-required",
            file,
            at,
            "승인이 필요한 도구를 cron이 부르려면 approver(등록된 사람 신원)를 지정해야 해요",
        ),
        (Some(who), _) => match p.members.get(who) {
            None => issues.error_at(
                "approver-unknown",
                file,
                at,
                format!("승인자 `{}`가 members에 없어요", who.0),
            ),
            Some(m) if m.kind == names::IdentityKind::Service => issues.error_at(
                "approver-service",
                file,
                at,
                "서비스 주체는 승인할 수 없어요",
            ),
            Some(m) if !m.kind.is_human() => issues.error_at(
                "approver-not-human",
                file,
                at,
                format!(
                    "승인자는 사람 신원(`local:*`, `slack:*`)이어야 해요(`{}`는 {:?} 종류)",
                    who.0, m.kind
                ),
            ),
            Some(_) => {}
        },
        (None, false) => {}
    }
}

/// irreversible과 정책이 지정한 write(스코프가 `policy.approve`에 있는 것)는 추가 승인이 필요하다.
fn needs_approval(p: &ProfileParts, tool: &Tool) -> bool {
    match tool.risk() {
        Risk::Irreversible => true,
        Risk::Write => tool.scopes.iter().any(|s| p.policy.approve.contains(s)),
        Risk::Read => false,
    }
}
