//! Pure model-context assembly from a profile snapshot, history, and visible memory.
//!
//! Security boundaries: recalled memory is placed only in an untrusted user reference block
//! (`memory.rs`), visible tools are advisory (the broker re-authorizes every call), and
//! authenticated API root runs wrap the entire caller history as untrusted data (`chat.rs`).

use crate::model::{
    Exchange, Limits, Message, ModelRequest, RelayChannel, Role, Session, ToolKind, ToolSpec,
};
use bot_core::{
    command::{JsonPayload, MemoryHit, Target, Turn, Work},
    id::{AgentId, Attempt, ToolId},
    profile::{Agent, Builtin, Document, Snapshot},
    state::{Risk, RunContext},
};
use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

mod chat;
pub use chat::is_chat;
mod memory;
pub use memory::REFERENCE_HEADER;
mod request;

pub const MAX_KNOWLEDGE_DOCS: usize = 8;
pub const MAX_KNOWLEDGE_BYTES: usize = 32 * 1024;
pub const MAX_MEMORY_ITEMS: usize = 16;
pub const MAX_MEMORY_BYTES: usize = 8 * 1024;
pub const MAX_HISTORY_BYTES: usize = 64 * 1024;
pub use bot_core::builtin::DELEGATE_TOOL;

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

pub struct ContextInput<'a> {
    pub snapshot: &'a Snapshot,
    pub run: &'a RunContext,
    /// Previous turns in the same conversation (oldest first).
    pub history: &'a [Turn],
    /// Visibility-filtered memory hits from the store.
    pub memory: &'a [MemoryHit],
    pub now: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextError {
    UnknownAgent(AgentId),
    NotAnAgentRun,
    /// Authenticated API root run whose body is not a verified `ChatBody` (fails closed).
    InvalidChat,
}

#[derive(Clone, Debug)]
pub struct Assembled {
    pub model: String,
    pub system: String,
    pub conversation: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub delegates: Vec<AgentId>,
    /// Read-only built-in tools declared directly on this agent (not inherited).
    pub builtin: BTreeSet<Builtin>,
    pub knowledge: Vec<String>,
}

impl Assembled {
    pub fn into_request(
        self,
        attempt: Attempt,
        exchanges: Vec<Exchange>,
        limits: Limits,
        cancel: watch::Receiver<bool>,
        relay: Option<RelayChannel>,
        session: Option<Session>,
    ) -> ModelRequest {
        ModelRequest {
            attempt,
            model: self.model,
            system: self.system,
            conversation: self.conversation,
            tools: self.tools,
            exchanges,
            limits,
            cancel,
            builtin: self.builtin,
            relay,
            session,
        }
    }
}

/// Alphanumeric tokens (2+ chars); paired with character bigrams below to match inflected forms.
fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

fn pairs(tokens: &BTreeSet<String>) -> BTreeSet<(char, char)> {
    let mut out = BTreeSet::new();
    for token in tokens {
        let chars: Vec<char> = token.chars().collect();
        out.extend(chars.windows(2).map(|w| (w[0], w[1])));
    }
    out
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

fn score(query: &BTreeSet<String>, query_pairs: &BTreeSet<(char, char)>, doc: &Document) -> usize {
    let words = tokens(&format!("{} {}", stem(&doc.relative_path), doc.text));
    let shared_words = query.intersection(&words).count();
    let shared_pairs = query_pairs.intersection(&pairs(&words)).count();
    3 * shared_words + shared_pairs
}

/// Picks up to `MAX_KNOWLEDGE_DOCS` / `MAX_KNOWLEDGE_BYTES` matching docs by score (path tie-break),
/// skipping oversized docs whole rather than truncating.
pub fn select_knowledge<'a>(query: &str, docs: &'a [Document]) -> Vec<&'a Document> {
    let words = tokens(query);
    if words.is_empty() {
        return vec![];
    }
    let query_pairs = pairs(&words);
    let mut scored: Vec<(usize, &Document)> = docs
        .iter()
        .filter(|d| !d.text.trim().is_empty())
        .filter_map(|d| Some((score(&words, &query_pairs, d), d)).filter(|(s, _)| *s > 0))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.relative_path.cmp(&b.1.relative_path))
    });
    let (mut picked, mut used) = (Vec::new(), 0usize);
    for (_, doc) in scored {
        if picked.len() >= MAX_KNOWLEDGE_DOCS {
            break;
        }
        if used + doc.text.len() > MAX_KNOWLEDGE_BYTES {
            continue;
        }
        used += doc.text.len();
        picked.push(doc);
    }
    picked
}

fn body_text(body: &JsonPayload) -> String {
    match serde_json::from_str::<Value>(&body.0) {
        Ok(Value::Null) => String::new(),
        Ok(Value::Object(map)) if map.is_empty() => String::new(),
        Ok(Value::Object(map)) => match map.get("text") {
            Some(Value::String(text)) => text.clone(),
            _ => Value::Object(map).to_string(),
        },
        Ok(Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => body.0.trim().to_owned(),
    }
}

/// Query string for memory and knowledge lookup; API root runs use only the last user turn.
pub fn query_text(run: &RunContext) -> String {
    if chat::is_chat(run) {
        return chat::read(run)
            .and_then(|body| chat::question(&body).map(|turn| turn.content.clone()))
            .unwrap_or_default();
    }
    current_input(&run.work).unwrap_or_default()
}

pub(crate) fn current_input(work: &Work) -> Result<String, ContextError> {
    let Target::Agent { instruction, .. } = &work.target else {
        return Err(ContextError::NotAnAgentRun);
    };
    let (instruction, body) = (instruction.trim(), body_text(&work.input));
    let body = body.trim();
    Ok(match (instruction.is_empty(), body.is_empty()) {
        (true, _) => body.to_owned(),
        (false, true) => instruction.to_owned(),
        (false, false) if instruction == body => body.to_owned(),
        (false, false) => format!("{instruction}\n\nInput:\n{body}"),
    })
}

fn clock(now: SystemTime, zone: &str) -> String {
    let tz: Tz = zone.parse().unwrap_or(chrono_tz::UTC);
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let local = DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_default()
        .with_timezone(&tz);
    let weekday = WEEKDAYS[local.weekday().num_days_from_monday() as usize];
    format!(
        "{}({weekday}) {} ({})",
        local.format("%Y-%m-%d"),
        local.format("%H:%M"),
        tz.name()
    )
}

/// One-line child summary from declared `description`, falling back to the child's own final paragraph.
fn hint(agent: &Agent) -> String {
    let declared = agent.description.trim();
    if !declared.is_empty() {
        return declared
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect::<String>()
            .trim()
            .to_owned();
    }
    let last = agent
        .instructions
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or_default();
    let line: String = last
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    line.trim().to_owned()
}

/// Request scopes intersected with the agent ceiling and policy ceilings (including per-agent caps).
pub(crate) fn effective_scopes(
    snapshot: &Snapshot,
    agent: &Agent,
    run: &RunContext,
) -> BTreeSet<String> {
    let ceiling = bot_core::scope::agent_ceiling(&snapshot.policy.agents, &run.agent);
    run.scopes
        .iter()
        .filter(|s| {
            agent.scopes.contains(*s)
                && snapshot.policy.scopes.contains(*s)
                && ceiling.as_ref().is_none_or(|upper| upper.contains(*s))
        })
        .cloned()
        .collect()
}

fn visible_tools(agent: &Agent, effective: &BTreeSet<String>) -> Vec<(String, ToolSpec)> {
    let mut out = Vec::new();
    for (name, skill) in &agent.skills {
        for (id, tool) in &skill.tools {
            if bot_core::builtin::reserved(&id.0)
                || tool.scopes.is_empty()
                || !tool.scopes.iter().all(|s| effective.contains(s))
            {
                continue;
            }
            let spec = ToolSpec {
                id: id.clone(),
                kind: ToolKind::Skill,
                class: tool.class,
                description: tool.model_description().unwrap_or_else(|| id.0.clone()),
                input: tool.input.clone(),
            };
            out.push((name.clone(), spec));
        }
    }
    out
}

fn delegate_tool(children: &[(AgentId, String)]) -> ToolSpec {
    ToolSpec {
        id: ToolId(DELEGATE_TOOL.into()),
        kind: ToolKind::Delegate,
        class: Risk::Read,
        description: bot_core::builtin::DELEGATE_DESCRIPTION.into(),
        input: JsonPayload(
            bot_core::builtin::delegate_schema(children.iter().map(|(id, _)| id)).to_string(),
        ),
    }
}

fn history_messages(history: &[Turn]) -> Vec<Message> {
    let (mut kept, mut used) = (Vec::new(), 0usize);
    for turn in history.iter().rev() {
        if used + turn.text.len() > MAX_HISTORY_BYTES {
            break;
        }
        used += turn.text.len();
        kept.push(Message {
            role: if turn.from.is_some() {
                Role::User
            } else {
                Role::Assistant
            },
            speaker: turn.from.clone(),
            text: turn.text.clone(),
        });
    }
    kept.reverse();
    kept
}

pub fn assemble(input: &ContextInput<'_>) -> Result<Assembled, ContextError> {
    let (snapshot, run) = (input.snapshot, input.run);
    let agent = snapshot
        .agents
        .get(&run.agent)
        .ok_or_else(|| ContextError::UnknownAgent(run.agent.clone()))?;
    // Authenticated API root runs treat the caller's entire conversation as untrusted data,
    // including `instruction` (which mirrors the last user turn).
    let chat = if chat::is_chat(run) {
        if !matches!(run.work.target, Target::Agent { .. }) {
            return Err(ContextError::NotAnAgentRun);
        }
        Some(chat::read(run)?)
    } else {
        None
    };
    let question = match &chat {
        Some(body) => chat::question(body)?.content.clone(),
        None => current_input(&run.work)?,
    };

    let effective = effective_scopes(snapshot, agent, run);
    let super_admin = snapshot.is_super_admin(&run.principal);
    let visible = visible_tools(agent, &effective);
    let mut tools: Vec<ToolSpec> = visible.iter().map(|(_, spec)| spec.clone()).collect();
    tools.extend(
        bot_core::builtin::model_tools_for(&effective, super_admin).map(|spec| ToolSpec {
            id: ToolId(spec.id.into()),
            kind: ToolKind::Skill,
            class: spec.class,
            description: spec.description.into(),
            input: JsonPayload(spec.schema.into()),
        }),
    );
    let children: Vec<(AgentId, String)> = snapshot
        .agents
        .iter()
        .filter(|(_, child)| child.parent.as_ref() == Some(&run.agent))
        .map(|(id, child)| (id.clone(), hint(child)))
        .collect();
    if !children.is_empty() {
        tools.push(delegate_tool(&children));
    }

    let query = match (&chat, &run.work.target) {
        (Some(_), _) | (None, Target::Tool { .. }) => question.clone(),
        (None, Target::Agent { instruction, .. }) => format!("{instruction}\n{question}"),
    };
    let docs = select_knowledge(&query, &agent.knowledge);

    let mut sections = Vec::new();
    if !agent.instructions.trim().is_empty() {
        sections.push(agent.instructions.trim().to_owned());
    }
    sections.push(format!(
        "## Current Time\n{}",
        clock(input.now, &snapshot.timezone)
    ));
    sections.push(request::section(snapshot, run));
    if chat.is_some() {
        sections.push(chat::NOTICE.to_owned());
    }
    let skills: Vec<String> = agent
        .skills
        .iter()
        .filter(|(name, skill)| {
            skill.tools.is_empty() || visible.iter().any(|(owner, _)| owner == *name)
        })
        .map(|(name, skill)| format!("### {name}\n{}", skill.instructions.text.trim()))
        .collect();
    if !skills.is_empty() {
        sections.push(format!("## Skills\n{}", skills.join("\n\n")));
    }
    if !children.is_empty() {
        let lines: Vec<String> = children
            .iter()
            .map(|(id, hint)| {
                if hint.is_empty() {
                    format!("- {}", id.0)
                } else {
                    format!("- {}: {hint}", id.0)
                }
            })
            .collect();
        sections.push(format!(
            "## Delegation\nYou may delegate work only to direct child agents using the `{DELEGATE_TOOL}` tool.\n{}",
            lines.join("\n")
        ));
    }
    if !docs.is_empty() {
        let body: Vec<String> = docs
            .iter()
            .map(|d| format!("### {}\n{}", d.relative_path, d.text.trim_end()))
            .collect();
        sections.push(format!("## Reference Documents\n{}", body.join("\n\n")));
    }
    if effective.contains(bot_core::builtin::MEMORY_SAVE_TOOL) {
        sections.push(memory::TOOL_GUIDANCE.to_owned());
    }
    // Keep recalled memory out of system instructions; place bodies in the untrusted user block below.
    let reference = memory::reference(input.memory);
    if reference.is_some() {
        sections.push(memory::SYSTEM_NOTICE.to_owned());
    }

    let mut conversation = match &chat {
        // API root runs ignore server history; each request is closed over the caller's turns.
        Some(body) => vec![Message {
            role: Role::User,
            speaker: Some(run.principal.clone()),
            text: chat::render(body)?,
        }],
        None => {
            let mut conversation = history_messages(input.history);
            conversation.push(Message {
                role: Role::User,
                speaker: Some(run.principal.clone()),
                text: question,
            });
            conversation
        }
    };
    memory::place(&mut conversation, reference);
    Ok(Assembled {
        model: agent.model.clone(),
        system: sections.join("\n\n"),
        conversation,
        tools,
        delegates: children.into_iter().map(|(id, _)| id).collect(),
        builtin: agent.builtin.clone(),
        knowledge: docs.iter().map(|d| d.relative_path.clone()).collect(),
    })
}

#[cfg(test)]
mod tests;
