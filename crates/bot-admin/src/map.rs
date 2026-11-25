//! 구조 지도: 검증된 프로필 스냅숏(`bot_core::profile::Snapshot`)에서 화면이 읽는 카드와 연결을 만든다.
//! 순수·결정적·I/O 없음 — 같은 입력은 같은 `rev`다. 지침·지식 본문과 비밀 값은 읽지 않고 이름·분류·개수만 싣는다.
//!
//! 카드 종류 8개: profile·agent·skill·tool·trigger·binding(항목)과 scope·secret(공통 자원).
//! 연결은 6종: delegates(위임)·uses(사용)·runs(소유)·handles(처리)·triggers(깨움)·needs(필요).

use bot_core::{
    id::{AgentId, ProfileId},
    profile::{
        Adapter, Agent, Builtin, Destination, Snapshot, Tool, Trigger, TriggerSource, TriggerTarget,
    },
    state::Risk,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, collections::BTreeSet, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Profile,
    Agent,
    Skill,
    Tool,
    Trigger,
    Binding,
    Scope,
    Secret,
}

impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Profile => "profile",
            Self::Agent => "agent",
            Self::Skill => "skill",
            Self::Tool => "tool",
            Self::Trigger => "trigger",
            Self::Binding => "binding",
            Self::Scope => "scope",
            Self::Secret => "secret",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rel {
    Delegates,
    Uses,
    Runs,
    Handles,
    Triggers,
    Needs,
}

impl Rel {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delegates => "delegates",
            Self::Uses => "uses",
            Self::Runs => "runs",
            Self::Handles => "handles",
            Self::Triggers => "triggers",
            Self::Needs => "needs",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub profile: String,
    /// 소유 에이전트 경로. 프로필 수준 카드(프로필·바인딩·스코프·비밀)는 `None`이다.
    pub agent: Option<String>,
    pub kind: Kind,
    pub name: String,
    pub summary: String,
    pub facts: Vec<(String, String)>,
    pub links: Vec<(Rel, String)>,
}

/// 카드 ID: `<에이전트>/<종류>:<이름>`(프로필 수준이면 `<종류>:<이름>`). 카드 키는 (프로필, ID)다.
pub fn card_id(agent: Option<&str>, kind: Kind, name: &str) -> String {
    match agent {
        Some(a) => format!("{a}/{}:{name}", kind.as_str()),
        None => format!("{}:{name}", kind.as_str()),
    }
}

impl Card {
    pub fn id(&self) -> String {
        card_id(self.agent.as_deref(), self.kind, &self.name)
    }

    fn new(profile: &str, agent: Option<&str>, kind: Kind, name: &str, summary: String) -> Self {
        Self {
            profile: profile.to_owned(),
            agent: agent.map(str::to_owned),
            kind,
            name: name.to_owned(),
            summary,
            facts: Vec::new(),
            links: Vec::new(),
        }
    }

    fn fact(mut self, key: &str, val: impl ToString) -> Self {
        self.facts.push((key.to_owned(), val.to_string()));
        self
    }

    fn link(mut self, rel: Rel, to: String) -> Self {
        self.links.push((rel, to));
        self
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id(), "profile": self.profile, "agent": self.agent, "kind": self.kind.as_str(),
            "name": self.name, "summary": self.summary,
            "facts": self.facts.iter().map(|(k, v)| json!({"key": k, "val": v})).collect::<Vec<_>>(),
            "links": self.links.iter().map(|(r, to)| json!({"rel": r.as_str(), "to": to})).collect::<Vec<_>>(),
        })
    }
}

/// 조립 결과: 카드(구조)와 구조 버전. `rev`는 카드 전체에서 계산한다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Map {
    pub rev: String,
    pub nodes: Vec<Card>,
}

fn list<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> String {
    let v: Vec<String> = items.into_iter().map(|s| s.as_ref().to_owned()).collect();
    if v.is_empty() {
        "-".to_owned()
    } else {
        v.join(", ")
    }
}

fn short(s: &str) -> String {
    s.chars().take(12).collect()
}

/// 승인 요건: 되돌릴 수 없는 작업은 항상, 정책이 지정한 스코프를 쓰는 쓰기는 정책에 따라, 그 밖에는 없음.
fn approval(s: &Snapshot, t: &Tool) -> &'static str {
    match t.class {
        Risk::Irreversible => "always",
        Risk::Write if t.scopes.iter().any(|x| s.policy.approve.contains(x)) => "policy",
        _ => "none",
    }
}

fn destination(d: &Destination) -> String {
    match d {
        Destination::Reply => "reply".to_owned(),
        Destination::Explicit { recipient, content } => format!("{recipient} <- {content}"),
    }
}

/// 같은 스킬(지침 경로·도구 목록이 같음)을 부모도 보는가.
fn inherits(s: &Snapshot, child: &AgentId, parent: &AgentId, skill: &str) -> bool {
    let get = |a: &AgentId| s.agents.get(a).and_then(|x| x.skills.get(skill));
    match (get(child), get(parent)) {
        (Some(x), Some(y)) => {
            x.instructions.relative_path == y.instructions.relative_path
                && x.tools.keys().eq(y.tools.keys())
        }
        _ => false,
    }
}

/// 이 에이전트가 보는 스킬을 실제로 정의한 조상(같은 스킬이 이어지는 가장 위 에이전트).
fn owner(s: &Snapshot, agent: &AgentId, skill: &str) -> AgentId {
    let mut at = agent.clone();
    while let Some(parent) = s.agents.get(&at).and_then(|a| a.parent.clone()) {
        if !inherits(s, &at, &parent, skill) {
            break;
        }
        at = parent;
    }
    at
}

/// 모델 실행기가 이 에이전트에 열어 준 읽기 전용 내장 도구의 이름.
fn builtin_name(b: Builtin) -> &'static str {
    match b {
        Builtin::Read => "read",
        Builtin::Grep => "grep",
        Builtin::Glob => "glob",
    }
}

/// 카드 요약: 에이전트가 스스로 밝힌 설명의 첫 줄(140자까지), 없으면 모델을 보인다.
fn agent_summary(id: &str, a: &Agent) -> String {
    let line = a.description.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return format!("에이전트 {id}: 모델 {}", a.model);
    }
    line.chars().take(140).collect()
}

fn skill_of(tool: &str) -> &str {
    tool.split('.').next().unwrap_or(tool)
}

fn trigger_card(s: &Snapshot, agent: &AgentId, t: &Trigger) -> Card {
    let profile = s.id.0.as_str();
    let (on, detail) = match &t.on {
        TriggerSource::Cron { schedule, .. } => ("cron", schedule.clone()),
        TriggerSource::Event { binding, event } => ("event", format!("{event} @{}", binding.0)),
        TriggerSource::Http {
            binding,
            method,
            path,
            ..
        } => ("http", format!("{method} {path} @{}", binding.0)),
    };
    let mut c = Card::new(
        profile,
        Some(&agent.0),
        Kind::Trigger,
        &t.id,
        format!("트리거 {}: {on} {detail}", t.id),
    )
    .fact("on", on)
    .fact("when", detail)
    .fact("order", t.order)
    .fact("private", t.private);
    if let TriggerSource::Cron {
        beat,
        principal,
        approver,
        ..
    } = &t.on
    {
        c = c.fact("principal", &principal.0).fact("beat", beat);
        if let Some(a) = approver {
            c = c.fact("approver", &a.0);
        }
    }
    if let TriggerSource::Http { response, .. } = &t.on {
        c = c.fact("response", format!("{response:?}").to_lowercase());
    }
    match &t.target {
        TriggerTarget::Instruction(_) => c
            .fact("target", "agent")
            .link(Rel::Triggers, card_id(None, Kind::Agent, &agent.0)),
        TriggerTarget::Tool { tool, .. } => {
            let at = owner(s, agent, skill_of(&tool.0));
            c.fact("target", format!("tool {}", tool.0))
                .link(Rel::Triggers, card_id(Some(&at.0), Kind::Tool, &tool.0))
        }
    }
}

/// 한 프로필의 카드.
pub fn cards(s: &Snapshot) -> Vec<Card> {
    let p = s.id.0.as_str();
    let mut out = Vec::new();
    out.push(
        Card::new(
            p,
            None,
            Kind::Profile,
            p,
            format!("프로필 {p}: 에이전트 {}개", s.agents.len()),
        )
        .fact("revision", short(&s.revision.0))
        .fact("policy_revision", short(&s.policy_revision.0))
        .fact("timezone", &s.timezone)
        .fact("model", &s.model)
        .fact("agents", s.agents.len())
        .fact("root", &s.root.0)
        .link(Rel::Delegates, card_id(None, Kind::Agent, &s.root.0)),
    );
    let mut scopes: BTreeSet<&str> = s.policy.scopes.iter().map(String::as_str).collect();
    scopes.extend(s.roles.values().flatten().map(String::as_str));
    for (id, a) in &s.agents {
        let mut c = Card::new(p, None, Kind::Agent, &id.0, agent_summary(&id.0, a))
            .fact("model", &a.model)
            .fact("scopes", list(&a.scopes))
            .fact("builtin", list(a.builtin.iter().map(|b| builtin_name(*b))))
            .fact("skills", a.skills.len())
            .fact("triggers", a.triggers.len())
            .fact("root", a.parent.is_none());
        for (child, _) in s
            .agents
            .iter()
            .filter(|(_, x)| x.parent.as_ref() == Some(id))
        {
            c = c.link(Rel::Delegates, card_id(None, Kind::Agent, &child.0));
        }
        for name in a.skills.keys() {
            let at = owner(s, id, name);
            c = c.link(Rel::Uses, card_id(Some(&at.0), Kind::Skill, name));
        }
        for t in &a.triggers {
            c = c.link(Rel::Runs, card_id(Some(&id.0), Kind::Trigger, &t.id));
        }
        out.push(c);
        for (name, skill) in &a.skills {
            if owner(s, id, name) != *id {
                continue;
            }
            let server = skill
                .tools
                .values()
                .next()
                .map_or("-", |t| t.server.as_str());
            let mut sc = Card::new(
                p,
                Some(&id.0),
                Kind::Skill,
                name,
                format!("스킬 {name}: 도구 {}개", skill.tools.len()),
            )
            .fact("server", server)
            .fact("tools", skill.tools.len());
            for tid in skill.tools.keys() {
                sc = sc.link(Rel::Uses, card_id(Some(&id.0), Kind::Tool, &tid.0));
            }
            out.push(sc);
            for (tid, t) in &skill.tools {
                scopes.extend(t.scopes.iter().map(String::as_str));
                out.push(tool_card(s, &id.0, &tid.0, t));
            }
        }
        out.extend(a.triggers.iter().map(|t| trigger_card(s, id, t)));
    }
    for (bid, b) in &s.bindings {
        let (adapter, detail) = match &b.source {
            Adapter::Cli { account } => ("cli", format!("account {account}")),
            Adapter::SlackChannel {
                workspace, channel, ..
            } => ("slack", format!("{workspace} #{channel}")),
            Adapter::SlackDm { workspace } => ("slack", format!("{workspace} dm")),
            Adapter::Http { endpoint } => ("http", format!("endpoint {endpoint}")),
            Adapter::OpenAi {
                endpoint, model, ..
            } => ("openai", format!("endpoint {endpoint} model {model}")),
        };
        out.push(
            Card::new(
                p,
                None,
                Kind::Binding,
                &bid.0,
                format!("입구 {}: {adapter} → {}", bid.0, b.agent.0),
            )
            .fact("adapter", adapter)
            .fact("detail", detail)
            .fact("agent", &b.agent.0)
            .link(Rel::Handles, card_id(None, Kind::Agent, &b.agent.0)),
        );
    }
    for name in &scopes {
        let roles: Vec<&str> = s
            .roles
            .iter()
            .filter(|(_, v)| v.contains(*name))
            .map(|(k, _)| k.as_str())
            .collect();
        out.push(
            Card::new(p, None, Kind::Scope, name, format!("스코프 {name}"))
                .fact("granted", s.policy.scopes.contains(*name))
                .fact("approval", s.policy.approve.contains(*name))
                .fact("roles", list(roles)),
        );
    }
    for name in &s.secrets {
        out.push(
            Card::new(
                p,
                None,
                Kind::Secret,
                name.as_str(),
                "비밀 이름(값은 표시하지 않는다)".to_owned(),
            )
            .fact("declared", true),
        );
    }
    out
}

fn tool_card(s: &Snapshot, agent: &str, id: &str, t: &Tool) -> Card {
    let class = match t.class {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    };
    let mut c = Card::new(
        &s.id.0,
        Some(agent),
        Kind::Tool,
        id,
        format!("도구 {id}: {class}"),
    )
    .fact("class", class)
    .fact("scopes", list(&t.scopes))
    .fact("approval", approval(s, t))
    .fact("server", &t.server)
    .fact("revision", short(&t.revision.0))
    .fact("external", t.external)
    .fact("effects", t.effects)
    .fact("egress", list(&t.egress));
    if let Some(d) = &t.destination {
        c = c.fact("destination", destination(d));
    }
    for name in t.secrets.values() {
        c = c.link(Rel::Needs, card_id(None, Kind::Secret, name.as_str()));
    }
    for scope in &t.scopes {
        c = c.link(Rel::Needs, card_id(None, Kind::Scope, scope));
    }
    c
}

/// 끊긴 연결·중복 카드. 비어 있어야 정상이다.
pub fn problems(nodes: &[Card]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut bad = Vec::new();
    for c in nodes {
        if !seen.insert((c.profile.as_str(), c.id())) {
            bad.push(format!("{}/{}: ID가 중복이다", c.profile, c.id()));
        }
    }
    for c in nodes {
        for (_, to) in &c.links {
            if !seen.contains(&(c.profile.as_str(), to.clone())) {
                bad.push(format!(
                    "{}/{}: 연결 대상 {to}이(가) 없다",
                    c.profile,
                    c.id()
                ));
            }
        }
    }
    bad
}

/// 구조 버전: 카드 직렬화의 sha256 앞 12자. 같은 구조는 입력 순서와 상관없이 같은 값이다.
fn revision(nodes: &[Card]) -> String {
    let body: Vec<Value> = nodes.iter().map(Card::json).collect();
    let digest = Sha256::digest(serde_json::to_vec(&body).unwrap_or_default());
    crate::json::hex(&digest[..6])
}

/// 고른 프로필들의 구조를 합친다. 카드는 (프로필, ID)순으로 정규화한다.
pub fn build(profiles: &BTreeMap<ProfileId, Arc<Snapshot>>) -> Map {
    let mut nodes: Vec<Card> = profiles.values().flat_map(|s| cards(s)).collect();
    nodes.sort_by_cached_key(|c| (c.profile.clone(), c.id()));
    Map {
        rev: revision(&nodes),
        nodes,
    }
}

fn set(v: &BTreeSet<String>) -> Vec<&String> {
    v.iter().collect()
}

/// 권한 보기용: 역할·멤버·정책(이름만, 자격 값 없음).
pub fn access(s: &Snapshot) -> Value {
    json!({
        "revision": short(&s.revision.0), "policy_revision": short(&s.policy_revision.0),
        "timezone": s.timezone, "model": s.model, "root": s.root.0,
        "roles": s.roles.iter().map(|(k, v)| (k.clone(), set(v))).collect::<BTreeMap<_, _>>(),
        "members": s.members.iter().map(|(k, v)| json!({"identity": k.0, "roles": set(v)})).collect::<Vec<_>>(),
        "policy": {
            "scopes": set(&s.policy.scopes), "approve": set(&s.policy.approve),
            "agents": s.policy.agents.iter().map(|(k, v)| (k.0.clone(), set(v))).collect::<BTreeMap<_, _>>(),
        },
    })
}

pub fn to_json(map: &Map, with_graph: bool) -> Value {
    let mut v = json!({"rev": map.rev});
    if with_graph && let Some(o) = v.as_object_mut() {
        o.insert(
            "graph".into(),
            json!({"nodes": map.nodes.iter().map(Card::json).collect::<Vec<_>>()}),
        );
    }
    v
}
