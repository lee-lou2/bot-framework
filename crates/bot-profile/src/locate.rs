//! Resolves and validates a target profile and skill directory for `bot mcp sync` and `bot add tool`.
//!
//! Enforces the same root scan, tree read, layout, `profile.yaml` (`id == folder`), and target `SKILL.md`
//! rules as `validate`, while skipping whole-profile semantic checks so an unlocked skill can run its first sync.
use crate::{
    issue::{Issue, Issues},
    layout::{self, SkillFiles},
    limits::Limits,
    lock, parse, profile,
    scan::{self, Tree},
    schemas::bundled,
    text, tools, yaml,
};
use bot_core::id::ProfileId;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Validated profile and skill bytes captured in a single read (never re-read from disk).
#[derive(Debug)]
pub struct Located {
    pub profile: ProfileId,
    pub profile_dir: PathBuf,
    /// Skill directory path relative to `profile_dir` (e.g. `agents/main/skills/weather`).
    pub skill: String,
    pub name: String,
    pub skill_md: Vec<u8>,
    pub tools_yaml: Vec<u8>,
    total: u64,
    old_lock: u64,
}

fn refuse(code: &'static str, path: &Path, message: &str) -> Vec<Issue> {
    vec![Issue::error(code, path.display().to_string(), message)]
}

/// Computes `dir` relative to `root` without following symlinks; returns `None` if outside `root` or containing `..`.
fn below_root(root: &Path, dir: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(dir).ok()?;
    let canonical = std::fs::canonicalize(root).ok()?;
    let given = std::path::absolute(root).ok()?;
    // Check `given` before `canonical` so a symlinked root does not produce a leading `..` path.
    let rel = absolute
        .strip_prefix(&given)
        .or_else(|_| absolute.strip_prefix(&canonical))
        .ok()?;
    rel.components()
        .all(|part| matches!(part, Component::Normal(_)))
        .then(|| rel.to_path_buf())
}

pub fn locate_skill(root: &Path, dir: &Path, limits: &Limits) -> Result<Located, Vec<Issue>> {
    let mut issues = Issues::new(limits.max_issues);
    let listed = scan::list_profiles(root, limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    let not_a_skill = || {
        refuse(
            "not-a-skill",
            dir,
            "not a skill directory under BOT_PROFILES (`<profile>/skills/<name>` or `<profile>/agents/.../skills/<name>`)",
        )
    };
    let rel = below_root(root, dir).ok_or_else(not_a_skill)?;
    let mut parts = rel
        .components()
        .map(|part| part.as_os_str().to_str().unwrap_or_default().to_owned());
    let folder = parts.next().unwrap_or_default();
    let inside = parts.collect::<Vec<_>>().join("/");
    let (name, profile_dir) = listed
        .into_iter()
        .find(|(name, _)| *name == folder)
        .ok_or_else(not_a_skill)?;
    let tree = scan::read_tree(&profile_dir, "", limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    let found = layout::discover(&tree, limits, &mut issues);
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    // Validate `profile.yaml` and `id == folder` before reading credentials from this profile's secret namespace.
    let value = parse::yaml_of(
        &tree,
        "profile.yaml",
        &bundled().profile,
        limits,
        &mut issues,
    );
    let parts = value.and_then(|value| profile::parse(&value, Some(&name), limits, &mut issues));
    let Some(parts) = parts.filter(|_| !issues.has_errors()) else {
        return Err(issues.into_vec());
    };
    let skill: &SkillFiles = found
        .skills
        .iter()
        .chain(found.agents.iter().flat_map(|agent| agent.skills.iter()))
        .find(|skill| skill.dir == inside)
        .ok_or_else(not_a_skill)?;
    let bytes = |path: &Option<String>| path.as_deref().and_then(|p| tree.file(p));
    let (Some(skill_md_path), Some(skill_md), Some(tools_yaml)) = (
        skill.skill_md.as_deref(),
        bytes(&skill.skill_md),
        bytes(&skill.tools),
    ) else {
        return Err(refuse(
            "missing-file",
            dir,
            "skill directory must contain both SKILL.md and tools.yaml",
        ));
    };
    let _ = parse::check_skill_md(&skill.name, skill_md_path, skill_md, limits, &mut issues);
    if issues.has_errors() {
        return Err(issues
            .into_vec()
            .into_iter()
            .filter(Issue::is_error)
            .collect());
    }
    Ok(Located {
        profile: ProfileId(parts.id),
        profile_dir,
        skill: skill.dir.clone(),
        name: skill.name.clone(),
        skill_md: skill_md.to_vec(),
        tools_yaml: tools_yaml.to_vec(),
        total: tree.files.values().map(|bytes| bytes.len() as u64).sum(),
        old_lock: bytes(&skill.lock).map_or(0, |lock| lock.len() as u64),
    })
}

impl Located {
    /// Validates candidate `tools.lock.yaml` bytes against the same size, YAML, hash, and schema checks as profile loading.
    pub fn verify_lock(&self, lock: &[u8], limits: &Limits) -> Result<(), Vec<Issue>> {
        let path = format!("{}/{}", self.skill, crate::lock::FILE);
        if lock.len() as u64 > limits.max_yaml_bytes {
            return Err(vec![Issue::error(
                "yaml-too-large",
                path,
                "tools.lock.yaml exceeds the size limit — reduce the number of declared tools",
            )]);
        }
        if self.total.saturating_sub(self.old_lock) + lock.len() as u64 > limits.max_profile_bytes {
            return Err(vec![Issue::error(
                "too-large-total",
                path,
                "profile exceeds the total size limit",
            )]);
        }
        check_skill(&self.name, &self.skill_md, &self.tools_yaml, lock, limits)
    }
}

pub fn check_skill(
    name: &str,
    skill_md: &[u8],
    tools: &[u8],
    lock: &[u8],
    limits: &Limits,
) -> Result<(), Vec<Issue>> {
    let dir = format!("skills/{name}");
    let mut tree = Tree::default();
    tree.dirs.insert("skills".to_owned());
    tree.dirs.insert(dir.clone());
    let place = |file: &str| format!("{dir}/{file}");
    for (file, bytes) in [
        ("SKILL.md", skill_md),
        ("tools.yaml", tools),
        (crate::lock::FILE, lock),
    ] {
        tree.files.insert(place(file), bytes.to_vec());
    }
    let files = SkillFiles {
        name: name.to_owned(),
        dir: dir.clone(),
        skill_md: Some(place("SKILL.md")),
        tools: Some(place("tools.yaml")),
        lock: Some(place(crate::lock::FILE)),
    };
    let mut issues = Issues::new(limits.max_issues);
    let parsed = parse::skill(&tree, &files, limits, &mut issues);
    if issues.has_errors() || parsed.is_none() {
        let errors: Vec<Issue> = issues
            .into_vec()
            .into_iter()
            .filter(Issue::is_error)
            .collect();
        return Err(if errors.is_empty() {
            vec![Issue::error(
                "skill-unreadable",
                dir,
                "failed to read skill",
            )]
        } else {
            errors
        });
    }
    Ok(())
}

#[derive(Debug)]
pub struct ToolsDoc {
    pub server: String,
    pub tool_ids: Vec<String>,
    pub value: Value,
}

/// Parses and validates `tools.yaml` bytes against the bundled schema and optionally full `tools::parse` rules.
pub fn parse_tools_yaml(
    skill: &str,
    path: &str,
    bytes: &[u8],
    lock_bytes: Option<&[u8]>,
    check_rules: bool,
    limits: &Limits,
) -> Result<ToolsDoc, Vec<Issue>> {
    let mut issues = Issues::new(limits.max_issues);
    let raw = match text::check_text(path, bytes) {
        Ok(text) => text,
        Err(issue) => return Err(vec![issue]),
    };
    let value = match yaml::parse(path, raw, limits) {
        Ok(value) => value,
        Err(issue) => return Err(vec![issue]),
    };
    for v in bundled().tools.validate(&value) {
        issues.error_at("schema", path, v.pointer, v.message);
    }
    if issues.has_errors() {
        return Err(issues.into_vec());
    }
    if check_rules {
        let parsed_lock = match lock_bytes {
            Some(raw_lock) => {
                let lock_path = format!("{skill}/{}", crate::lock::FILE);
                let lock_text = match text::check_text(&lock_path, raw_lock) {
                    Ok(t) => t,
                    Err(issue) => return Err(vec![issue]),
                };
                let lock_val = match yaml::parse(&lock_path, lock_text, limits) {
                    Ok(v) => v,
                    Err(issue) => return Err(vec![issue]),
                };
                match lock::parse(&lock_val, &lock_path, limits, &mut issues) {
                    Some(parsed) => Some(parsed),
                    None => return Err(issues.into_vec()),
                }
            }
            None => None,
        };
        let _ = tools::parse(
            &value,
            skill,
            path,
            limits,
            parsed_lock.as_ref(),
            &mut issues,
        );
        if issues.has_errors() {
            return Err(issues
                .into_vec()
                .into_iter()
                .filter(Issue::is_error)
                .collect());
        }
    }
    let server = value
        .get("server")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tool_ids = value
        .get("tools")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.get("id").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(ToolsDoc {
        server,
        tool_ids,
        value,
    })
}

/// Reads non-secret `key: string` entries from an optional `env.yaml` file.
pub fn read_env_yaml(path: &Path, limits: &Limits) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return out;
    };
    let Ok(val) = yaml::parse("env.yaml", &raw, limits) else {
        return out;
    };
    let Some(map) = val.as_object() else {
        return out;
    };
    for (k, v) in map {
        if let Some(s) = v.as_str() {
            out.insert(k.clone(), s.to_owned());
        }
    }
    out
}
