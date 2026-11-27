//! Inheritance assembly for agent instructions, skills, knowledge, env, model, and scope ceilings.
use crate::{
    issue::Issues,
    model::{Agent, Built, Skill},
    parse::Parsed,
};
use bot_core::id::AgentId;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Expects `parsed.agents` ordered parent-before-child with the root first.
pub fn assemble(parsed: Parsed, issues: &mut Issues) -> Option<Built> {
    let profile = &parsed.profile;
    let mut built: BTreeMap<AgentId, Agent> = BTreeMap::new();
    let mut root = None;
    for src in &parsed.agents {
        let parent = src.parent.as_ref().and_then(|p| built.get(p));
        if src.parent.is_some() && parent.is_none() {
            continue;
        }
        let mut skills: Vec<Arc<Skill>> =
            parent.map_or_else(|| parsed.skills.clone(), |p| p.skills.clone());
        let mut names: BTreeSet<String> = skills.iter().map(|s| s.name.clone()).collect();
        for skill in &src.skills {
            if names.insert(skill.name.clone()) {
                skills.push(skill.clone());
            } else if let Some(first) = skills.iter().find(|s| s.name == skill.name) {
                issues.error(
                    "skill-conflict",
                    &skill.path,
                    format!(
                        "skill name `{}` conflicts with `{}` (both visible to agent {})",
                        skill.name, first.path, src.id.0
                    ),
                );
            }
        }
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        let mut parts = parent.map_or_else(
            || parsed.rules.iter().cloned().collect(),
            |p| p.parts.clone(),
        );
        parts.push(src.part.clone());
        let mut env = parent.map(|p| p.env.clone()).unwrap_or_default();
        env.extend(src.env.clone());
        let mut knowledge =
            parent.map_or_else(|| parsed.knowledge.clone(), |p| p.knowledge.clone());
        knowledge.extend(src.knowledge.iter().cloned());
        let outer = parent.map_or_else(|| profile.policy.scopes.clone(), |p| p.ceiling.clone());
        let ceiling = match profile.policy.agents.get(&src.id) {
            Some(own) => outer.intersection(own).cloned().collect(),
            None => outer,
        };
        let description = src
            .description
            .clone()
            .unwrap_or_else(|| src.id.0.rsplit('/').next().unwrap_or(&src.id.0).to_owned());
        let model = src
            .model
            .clone()
            .or_else(|| parent.map(|p| p.model.clone()))
            .unwrap_or_else(|| profile.model.clone());
        if parent.is_none() {
            root = Some(src.id.clone());
        }
        built.insert(
            src.id.clone(),
            Agent {
                id: src.id.clone(),
                parent: src.parent.clone(),
                model,
                description,
                builtin: src.builtin.clone(),
                parts,
                env,
                skills,
                knowledge,
                triggers: src.triggers.clone(),
                memory: src.memory.clone(),
                ceiling,
            },
        );
    }
    Some(Built {
        root: root?,
        agents: built,
        parts: parsed.profile,
    })
}
