//! `bot add agent|skill` scaffolding for AGENT.md and SKILL.md without modifying profile.yaml caps.
use super::{files::Writer, templates};
use bot_core::id::is_name;
use bot_profile::Limits;
use std::path::Path;

pub(super) const NAME_RULE: &str =
    "lowercase letters, digits, and hyphens starting with a lowercase letter, at most 63 chars";

pub(super) fn lineage(path: &str) -> Result<Vec<&str>, String> {
    let parts: Vec<&str> = path.split('/').collect();
    match parts.iter().find(|part| !is_name(part)) {
        Some(bad) => Err(format!(
            "agent path segment {bad:?} in {path:?} violates naming rules ({NAME_RULE}) — e.g. main/cs"
        )),
        None => Ok(parts),
    }
}

pub(super) fn agent_dir<'a>(lineage: &[&'a str]) -> Vec<&'a str> {
    lineage.iter().flat_map(|part| ["agents", *part]).collect()
}

pub(super) fn open(dir: &Path) -> Result<Writer, String> {
    let writer = Writer::new(dir)?;
    if !writer.file_exists(&["profile.yaml"])? {
        return Err(format!(
            "not a profile directory (missing profile.yaml): {} — run inside a profile directory or pass --dir <profile-dir>",
            dir.display()
        ));
    }
    Ok(writer)
}

pub(super) fn has_agent(writer: &Writer, lineage: &[&str]) -> Result<bool, String> {
    let mut parts = agent_dir(lineage);
    parts.push("AGENT.md");
    writer.file_exists(&parts)
}

/// Writes one file; on failure removes only empty directories created by this call and reports any partial files.
pub(super) fn finish(mut writer: Writer, rel: &str, text: &str) -> Result<(), String> {
    writer.write(rel, text).map_err(|error| {
        let left = writer.undo();
        if left.is_empty() {
            error
        } else {
            format!(
                "{error}\npartially created files remain (inspect and remove manually): {}",
                left.join(", ")
            )
        }
    })
}

pub fn agent(dir: &Path, path: &str) -> Result<String, String> {
    let lineage = lineage(path)?;
    let max = Limits::default().max_agent_depth;
    if lineage.len() > max {
        return Err(format!(
            "agent lineage depth cannot exceed {max} (got {})",
            lineage.len()
        ));
    }
    let writer = open(dir)?;
    let (leaf, parents) = lineage.split_last().ok_or("agent path is empty")?;
    if parents.is_empty() {
        // Only one root agent is allowed; duplicate of the same name is rejected by require_absent below.
        let roots = writer.child_dirs(&["agents"])?;
        if !roots.is_empty() && !roots.iter().any(|root| root == leaf) {
            return Err(format!(
                "only one root agent is allowed (existing: {}) — for a sub-agent use a path like {}/{leaf}",
                roots.join(", "),
                roots[0]
            ));
        }
    } else if !has_agent(&writer, parents)? {
        let parent = parents.join("/");
        return Err(format!(
            "parent agent {parent} does not exist — run `bot add agent {parent}` first"
        ));
    }
    let target = agent_dir(&lineage);
    writer.require_absent(&target)?;
    let rel = format!("{}/AGENT.md", target.join("/"));
    finish(writer, &rel, &templates::agent_md(leaf))?;
    let permission = if parents.is_empty() {
        "The root agent follows the `policy.scopes` cap in profile.yaml".to_owned()
    } else {
        format!(
            "This agent inherits its parent ({}) permission cap; narrow it in profile.yaml `policy.agents` (e.g. `{path}: [run]`)",
            parents.join("/")
        )
    };
    Ok(format!(
        "Created: {rel}\nNext:\n  - Fill in description and instructions in AGENT.md.\n  - Permissions were not changed. {permission}.\n  - Mention this agent in its parent's AGENT.md or skill so the parent knows when to delegate.\n  - Run `bot validate` to verify."
    ))
}

/// Checks whether a skill with the same name is already visible at profile, ancestor, or descendant scope.
fn skill_conflict(writer: &Writer, scope: &[&str], name: &str) -> Option<String> {
    let exists = |parts: &[&str]| writer.exists(parts).unwrap_or(false);
    if !scope.is_empty() && exists(&["skills", name]) {
        return Some(format!("skills/{name}"));
    }
    let limits = Limits::default();
    let mut budget = limits.max_dirs;
    let mut stack: Vec<Vec<String>> = writer
        .child_dirs(&["agents"])
        .ok()?
        .into_iter()
        .map(|root| vec![root])
        .collect();
    while let Some(lineage) = stack.pop() {
        budget = budget.checked_sub(1)?;
        let names: Vec<&str> = lineage.iter().map(String::as_str).collect();
        let common = names.len().min(scope.len());
        let here = agent_dir(&names);
        if names[..common] == scope[..common] {
            let mut skill = here.clone();
            skill.extend(["skills", name]);
            if exists(&skill) {
                return Some(skill.join("/"));
            }
        }
        if lineage.len() >= limits.max_agent_depth {
            continue;
        }
        let mut children = here;
        children.push("agents");
        for child in writer.child_dirs(&children).unwrap_or_default() {
            let mut next = lineage.clone();
            next.push(child);
            stack.push(next);
        }
    }
    None
}

pub fn skill(dir: &Path, name: &str, agent: Option<&str>) -> Result<String, String> {
    if !is_name(name) {
        return Err(format!(
            "skill name {name:?} violates naming rules ({NAME_RULE}) — e.g. triage"
        ));
    }
    let scope = agent.map(lineage).transpose()?.unwrap_or_default();
    let writer = open(dir)?;
    if !scope.is_empty() && !has_agent(&writer, &scope)? {
        return Err(format!(
            "agent {} does not exist — run `bot add agent {}` first",
            scope.join("/"),
            scope.join("/")
        ));
    }
    let mut target = agent_dir(&scope);
    target.extend(["skills", name]);
    writer.require_absent(&target)?;
    if let Some(found) = skill_conflict(&writer, &scope, name) {
        return Err(format!(
            "a skill with the same name is already visible at {found} — validation rejects duplicate names; choose another name or edit the existing skill"
        ));
    }
    let rel = format!("{}/SKILL.md", target.join("/"));
    finish(writer, &rel, &templates::skill_md(name))?;
    if name == "skill-creator" {
        return Ok(format!(
            "Created: {rel}\nNext:\n  - Populated the super-admin live skill/MCP management skill.\n  - Add super-admin identities to `super_admins` in profile.yaml and allow scopes `skill.inspect`, `skill.create`, `skill.update`, `skill.delete`, `mcp.add`, and `mcp.remove` in roles and policy.\n  - Run `bot validate` to verify."
        ));
    }
    Ok(format!(
        "Created: {rel}\nNext:\n  - Fill in description and instructions in SKILL.md (top `---` front matter is validated as metadata; only the body is sent to the model).\n  - No tools.yaml or permissions were changed. Use `bot add tool {name}.<tool>` to declare tools, then grant required scopes manually in profile.yaml roles and policy.\n  - Run `bot validate` to verify."
    ))
}
