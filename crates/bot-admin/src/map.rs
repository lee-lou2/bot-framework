//! Pure, deterministic profile-snapshot-to-topology-map builder exposing names, counts, and edges without instructions or secrets.

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
    /// Owning agent path; `None` for profile-level cards (profile, binding, scope, secret).
    pub agent: Option<String>,
    pub kind: Kind,
    pub name: String,
    pub summary: String,
    pub facts: Vec<(String, String)>,
    pub links: Vec<(Rel, String)>,
}

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

/// Walks ancestors to find the highest agent defining the same inherited skill.
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

fn builtin_name(b: Builtin) -> &'static str {
    match b {
        Builtin::Read => "read",
        Builtin::Grep => "grep",
        Builtin::Glob => "glob",
    }
}

fn agent_summary(id: &str, a: &Agent) -> String {
    let line = a.description.lines().next().unwrap_or_default().trim();
    if line.is_empty() {
        return format!("Agent {id}: model {}", a.model);
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
        format!("Trigger {}: {on} {detail}", t.id),
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

pub fn cards(s: &Snapshot) -> Vec<Card> {
    let p = s.id.0.as_str();
    let mut out = Vec::new();
    out.push(
        Card::new(
            p,
            None,
            Kind::Profile,
            p,
            format!("Profile {p}: {} agent(s)", s.agents.len()),
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
                format!("Skill {name}: {} tool(s)", skill.tools.len()),
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
            Adapter::TelegramDm { bot } => ("telegram", format!("{bot} dm")),
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
                format!("Binding {}: {adapter} → {}", bid.0, b.agent.0),
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
            Card::new(p, None, Kind::Scope, name, format!("Scope {name}"))
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
                "Secret name (value hidden)".to_owned(),
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
        format!("Tool {id}: {class}"),
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

pub fn problems(nodes: &[Card]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut bad = Vec::new();
    for c in nodes {
        if !seen.insert((c.profile.as_str(), c.id())) {
            bad.push(format!("{}/{}: duplicate ID", c.profile, c.id()));
        }
    }
    for c in nodes {
        for (_, to) in &c.links {
            if !seen.contains(&(c.profile.as_str(), to.clone())) {
                bad.push(format!(
                    "{}/{}: missing link target {to}",
                    c.profile,
                    c.id()
                ));
            }
        }
    }
    bad
}

fn revision(nodes: &[Card]) -> String {
    let body: Vec<Value> = nodes.iter().map(Card::json).collect();
    let digest = Sha256::digest(serde_json::to_vec(&body).unwrap_or_default());
    crate::json::hex(&digest[..6])
}

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
