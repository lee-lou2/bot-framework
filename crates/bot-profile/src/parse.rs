//! Per-file parsing and schema validation into profile IR; cross-file checks run in `resolve`.
use crate::{
    frontmatter::{self, Kind},
    issue::{Issue, Issues},
    layout::{AgentFiles, Layout, SkillFiles},
    limits::Limits,
    lock,
    model::{ProfileParts, Skill, Trigger},
    names, profile,
    scan::Tree,
    schema::Schema,
    schemas::bundled,
    text, tools, triggers, yaml,
};
use bot_core::{
    id::{AgentId, PrincipalId},
    profile::{Builtin, Document, MemorySeed},
    state::Visibility,
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

pub struct ParsedAgent {
    pub id: AgentId,
    pub parent: Option<AgentId>,
    pub model: Option<String>,
    pub description: Option<String>,
    /// Read-only built-in tools declared in this AGENT.md (not inherited).
    pub builtin: BTreeSet<Builtin>,
    pub part: String,
    pub env: BTreeMap<String, String>,
    pub skills: Vec<Arc<Skill>>,
    pub knowledge: Vec<Document>,
    pub triggers: Vec<Trigger>,
    pub memory: Vec<MemorySeed>,
}

/// `SKILL.md` parsing mode: `Legacy` restores `BOTPROFILE1` bundles without front matter stripping or the 64 KiB cap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SkillMode {
    Legacy,
    Strict,
}

pub struct Parsed {
    pub profile: ProfileParts,
    pub rules: Option<String>,
    pub knowledge: Vec<Document>,
    pub skills: Vec<Arc<Skill>>,
    /// Ordered parent-before-child.
    pub agents: Vec<ParsedAgent>,
    /// True when any skill stripped `SKILL.md` front matter in `Strict` mode (selects `BOTPROFILE2`).
    pub has_skill_front_matter: bool,
}

fn text_of<'a>(tree: &'a Tree, path: &str, issues: &mut Issues) -> Option<&'a str> {
    match text::check_text(path, tree.file(path)?) {
        Ok(t) => Some(t),
        Err(issue) => {
            issues.push(issue);
            None
        }
    }
}

fn yaml_value(tree: &Tree, path: &str, limits: &Limits, issues: &mut Issues) -> Option<Value> {
    let text = text_of(tree, path, issues)?;
    match yaml::parse(path, text, limits) {
        Ok(value) => Some(value),
        Err(issue) => {
            issues.push(issue);
            None
        }
    }
}

pub(crate) fn yaml_of(
    tree: &Tree,
    path: &str,
    schema: &Schema,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Value> {
    let value = yaml_value(tree, path, limits, issues)?;
    let found = schema.validate(&value);
    for v in &found {
        issues.error_at("schema", path, v.pointer.clone(), v.message.clone());
    }
    found.is_empty().then_some(value)
}

/// Validates `triggers.yaml`, preserving legacy fixed codes for `preapproved`, `human_approval`, and `on.cron`.
fn triggers_yaml(tree: &Tree, path: &str, limits: &Limits, issues: &mut Issues) -> Option<Value> {
    let value = yaml_value(tree, path, limits, issues)?;
    let found = bundled().triggers.validate(&value);
    for v in &found {
        let code = trigger_schema_code(&value, &v.pointer);
        issues.error_at(code, path, v.pointer.clone(), v.message.clone());
    }
    found.is_empty().then_some(value)
}

fn trigger_schema_code(doc: &Value, pointer: &str) -> &'static str {
    let Some(rest) = pointer.strip_prefix("/triggers/") else {
        return "schema";
    };
    let Some((idx, tail)) = rest.split_once('/') else {
        return "schema";
    };
    if tail == "preapproved" || tail.starts_with("preapproved/") {
        let base = format!("/triggers/{idx}/preapproved");
        if doc.pointer(&base).is_some_and(Value::is_object) {
            return bot_core::grant::reason::INVALID;
        }
    } else if tail == "human_approval" || tail.starts_with("human_approval/") {
        let base = format!("/triggers/{idx}/human_approval");
        if doc.pointer(&base).is_some_and(Value::is_object) {
            return "invalid_human_approval";
        }
    } else if tail == "on/cron" {
        let base = format!("/triggers/{idx}/on/cron");
        if doc.pointer(&base).is_some_and(Value::is_string) {
            return "bad-cron";
        }
    }
    "schema"
}

pub fn parse_all_with(
    tree: &Tree,
    layout: &Layout,
    folder: Option<&str>,
    mode: SkillMode,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Parsed> {
    let before = issues.error_count();
    let profile = yaml_of(tree, "profile.yaml", &bundled().profile, limits, issues)
        .and_then(|v| profile::parse(&v, folder, limits, issues));
    let rules = layout
        .rules
        .as_deref()
        .and_then(|p| text_of(tree, p, issues))
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim_end().to_owned());
    let knowledge = docs(tree, &layout.knowledge, issues);
    let mut has_skill_front_matter = false;
    let skills = skills(
        tree,
        &layout.skills,
        mode,
        limits,
        &mut has_skill_front_matter,
        issues,
    );
    let agents: Vec<ParsedAgent> = layout
        .agents
        .iter()
        .filter_map(|a| agent(tree, a, mode, limits, &mut has_skill_front_matter, issues))
        .collect();
    if issues.error_count() > before {
        return None;
    }
    Some(Parsed {
        profile: profile?,
        rules,
        knowledge,
        skills,
        agents,
        has_skill_front_matter,
    })
}

fn docs(tree: &Tree, paths: &[String], issues: &mut Issues) -> Vec<Document> {
    paths
        .iter()
        .filter_map(|path| {
            let text = text_of(tree, path, issues)?;
            Some(Document {
                relative_path: path.clone(),
                text: text.to_owned(),
            })
        })
        .collect()
}

fn skills(
    tree: &Tree,
    files: &[SkillFiles],
    mode: SkillMode,
    limits: &Limits,
    has_skill_front_matter: &mut bool,
    issues: &mut Issues,
) -> Vec<Arc<Skill>> {
    files
        .iter()
        .filter_map(|f| {
            let (skill, has_fm) = skill_with_mode(tree, f, mode, limits, issues)?;
            *has_skill_front_matter |= has_fm;
            Some(skill)
        })
        .collect()
}

/// Validates a single `SKILL.md` under `Strict` rules; returns `(description, body, has_front_matter)`.
pub(crate) fn check_skill_md(
    skill_name: &str,
    md_path: &str,
    bytes: &[u8],
    limits: &Limits,
    issues: &mut Issues,
) -> Option<(Option<String>, String, bool)> {
    let md = match text::check_text(md_path, bytes) {
        Ok(t) => t,
        Err(issue) => {
            issues.push(issue);
            return None;
        }
    };
    if md == "---" || md == "---\r" {
        issues.error(
            "front-matter-unclosed",
            md_path,
            "Front matter opened with `---` is missing a closing `---` line",
        );
        return None;
    }
    let mut found = Vec::new();
    let parsed = frontmatter::parse(md_path, md, &Kind::Skill, limits, &mut found);
    issues.extend(found);
    let parsed = parsed?;
    let mut ok = true;
    let has_front_matter = parsed.meta.is_some();
    // Plain skills without front matter trim only trailing whitespace for byte-level V1 compatibility.
    let body = if has_front_matter {
        parsed.body.trim()
    } else {
        parsed.body.trim_end()
    };
    if body.trim().is_empty() {
        issues.error("empty-file", md_path, "SKILL.md has no instructions");
        ok = false;
    } else if body.len() > limits.max_skill_bytes {
        issues.error(
            "skill-too-large",
            md_path,
            format!(
                "SKILL.md body is {} bytes, exceeding limit ({} bytes); shorten instructions or move reference material to knowledge/",
                body.len(),
                limits.max_skill_bytes
            ),
        );
        ok = false;
    }
    if let Some(declared) = parsed
        .meta
        .as_ref()
        .and_then(|m| m.get("name"))
        .and_then(Value::as_str)
        && declared != skill_name
    {
        issues.error_at(
            "skill-name-mismatch",
            md_path,
            "front matter /name",
            format!("front matter name `{declared}` does not match skill directory `{skill_name}`"),
        );
        ok = false;
    }
    let field = |name: &str| {
        let text = parsed.meta.as_ref()?.get(name)?.as_str()?;
        Some(text.trim().to_owned())
    };
    let description = field("description");
    if description.as_deref() == Some("") {
        issues.error_at(
            "schema",
            md_path,
            "front matter /description",
            "description is empty",
        );
        ok = false;
    }
    for (key, label) in [("license", "license"), ("compatibility", "compatibility")] {
        if field(key).as_deref() == Some("") {
            issues.error_at(
                "schema",
                md_path,
                format!("front matter /{key}"),
                format!("{label} is empty"),
            );
            ok = false;
        }
    }
    ok.then(|| (description, body.to_owned(), has_front_matter))
}

pub(crate) fn skill(
    tree: &Tree,
    files: &SkillFiles,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Arc<Skill>> {
    skill_with_mode(tree, files, SkillMode::Strict, limits, issues).map(|(s, _)| s)
}

fn skill_with_mode(
    tree: &Tree,
    files: &SkillFiles,
    mode: SkillMode,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<(Arc<Skill>, bool)> {
    let md_path = files.skill_md.as_deref()?;
    let (description, body, has_front_matter) = match mode {
        SkillMode::Strict => {
            let bytes = tree.file(md_path)?;
            check_skill_md(&files.name, md_path, bytes, limits, issues)?
        }
        SkillMode::Legacy => {
            let md = text_of(tree, md_path, issues)?;
            if md.trim().is_empty() {
                issues.error("empty-file", md_path, "SKILL.md is empty");
                return None;
            }
            (None, md.trim_end().to_owned(), false)
        }
    };
    let lock = match files.lock.as_deref() {
        Some(path) if files.tools.is_none() => {
            issues.error(
                "lock-without-tools",
                path,
                "tools.lock.yaml requires a sibling tools.yaml",
            );
            return None;
        }
        Some(path) => {
            let value = yaml_value(tree, path, limits, issues)?;
            Some(lock::parse(&value, path, limits, issues)?)
        }
        None => None,
    };
    let tools = match files.tools.as_deref() {
        Some(path) => {
            let schema = match mode {
                SkillMode::Strict => &bundled().tools,
                SkillMode::Legacy => &bundled().tools_legacy,
            };
            let value = yaml_of(tree, path, schema, limits, issues)?;
            tools::parse(&value, &files.name, path, limits, lock.as_ref(), issues)?
        }
        None => Vec::new(),
    };
    Some((
        Arc::new(Skill {
            name: files.name.clone(),
            path: files.dir.clone(),
            description,
            instructions: Document {
                relative_path: md_path.to_owned(),
                text: body,
            },
            tools,
        }),
        has_front_matter,
    ))
}

fn agent(
    tree: &Tree,
    files: &AgentFiles,
    mode: SkillMode,
    limits: &Limits,
    has_skill_front_matter: &mut bool,
    issues: &mut Issues,
) -> Option<ParsedAgent> {
    let id = AgentId(files.id.clone());
    let md = text_of(tree, &files.agent_md, issues)?;
    let mut found = Vec::new();
    let parsed = frontmatter::parse(&files.agent_md, md, &Kind::Agent, limits, &mut found);
    issues.extend(found);
    let parsed = parsed?;
    if parsed.body.trim().is_empty() {
        issues.error(
            "empty-file",
            &files.agent_md,
            "AGENT.md has no instructions",
        );
        return None;
    }
    let field = |name: &str| {
        let text = parsed.meta.as_ref()?.get(name)?.as_str()?;
        Some(text.trim().to_owned())
    };
    let model = field("model");
    let description = field("description");
    if description.as_deref() == Some("") {
        issues.error_at(
            "schema",
            &files.agent_md,
            "front matter /description",
            "description is empty",
        );
        return None;
    }
    let builtin = parsed
        .meta
        .as_ref()
        .and_then(|m| m.get("builtin"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|name| match name {
                    "read" => Some(Builtin::Read),
                    "grep" => Some(Builtin::Grep),
                    "glob" => Some(Builtin::Glob),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let env = files
        .env
        .as_deref()
        .and_then(|p| Some((p, yaml_of(tree, p, &bundled().env, limits, issues)?)))
        .map(|(path, value)| env_map(&value, path, limits, issues))
        .unwrap_or_default();
    let triggers = files
        .triggers
        .as_deref()
        .and_then(|p| Some((p, triggers_yaml(tree, p, limits, issues)?)))
        .map(|(path, value)| triggers::parse(&value, path, limits, issues))
        .unwrap_or_default();
    Some(ParsedAgent {
        part: parsed.body.trim().to_owned(),
        model,
        description,
        builtin,
        env,
        skills: skills(
            tree,
            &files.skills,
            mode,
            limits,
            has_skill_front_matter,
            issues,
        ),
        knowledge: docs(tree, &files.knowledge, issues),
        triggers,
        memory: seeds(tree, &files.memory, &id, limits, issues),
        parent: files.parent.clone().map(AgentId),
        id,
    })
}

const SECRETISH: &[&str] = &[
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "PRIVATE_KEY",
    "API_KEY",
];

fn env_map(
    value: &Value,
    path: &str,
    limits: &Limits,
    issues: &mut Issues,
) -> BTreeMap<String, String> {
    let Some(map) = value.as_object() else {
        return BTreeMap::new();
    };
    if map.len() > limits.max_env_vars {
        issues.error(
            "too-many",
            path,
            format!(
                "Too many environment variables (max {})",
                limits.max_env_vars
            ),
        );
        return BTreeMap::new();
    }
    let mut out = BTreeMap::new();
    for (key, v) in map {
        let upper = key.to_ascii_uppercase();
        let reserved = key.starts_with("BOT_")
            || upper.starts_with("LD_")
            || upper.starts_with("DYLD_")
            || upper == "PATH";
        if key.len() > 128 || reserved {
            issues.error_at(
                "reserved-env",
                path,
                format!("/{key}"),
                "Reserved or invalid env name (`BOT_`, `LD_`, `DYLD_` prefixes, `PATH`, or > 128 chars)",
            );
            continue;
        }
        let value = v.as_str().unwrap_or_default();
        if value.len() > limits.max_env_value {
            issues.error_at(
                "too-large",
                path,
                format!("/{key}"),
                format!("Value is too long (max {} bytes)", limits.max_env_value),
            );
            continue;
        }
        if SECRETISH.iter().any(|w| upper.contains(w)) {
            issues.warning(
                "env-looks-secret",
                path,
                format!("`{key}` looks like a secret; use tools.yaml secrets instead of env.yaml"),
            );
        }
        out.insert(key.clone(), value.to_owned());
    }
    out
}

fn seeds(
    tree: &Tree,
    paths: &[String],
    agent: &AgentId,
    limits: &Limits,
    issues: &mut Issues,
) -> Vec<MemorySeed> {
    if paths.len() > limits.max_memory_seeds {
        issues.error(
            "too-many",
            &format!("agents/{}/memory", agent.0),
            format!("Too many memory seeds (max {})", limits.max_memory_seeds),
        );
        return Vec::new();
    }
    let mut out = Vec::new();
    for path in paths {
        let Some(md) = text_of(tree, path, issues) else {
            continue;
        };
        let mut found: Vec<Issue> = Vec::new();
        let parsed = frontmatter::parse(path, md, &Kind::Memory, limits, &mut found);
        issues.extend(found);
        let Some(parsed) = parsed else { continue };
        let meta = parsed.meta.unwrap_or(Value::Null);
        let owner = meta
            .get("owner")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if names::identity_kind(owner).is_none() {
            issues.error_at(
                "bad-identity",
                path,
                "front matter /owner",
                format!("owner must have `<kind>:<name>` format: {owner}"),
            );
            continue;
        }
        if parsed.body.trim().is_empty() {
            issues.error("empty-file", path, "Memory seed has no content");
            continue;
        }
        let visibility = match meta.get("visibility").and_then(Value::as_str) {
            Some("profile") => Visibility::Profile,
            _ => Visibility::Private,
        };
        out.push(MemorySeed {
            document: Document {
                relative_path: path.clone(),
                text: parsed.body.trim().to_owned(),
            },
            owner: PrincipalId(owner.to_owned()),
            visibility,
        });
    }
    out
}
