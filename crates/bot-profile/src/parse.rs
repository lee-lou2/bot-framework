//! 파일 읽기 단계: 텍스트·YAML·스키마·front matter를 검사해 모델 조각으로 만든다.
//! 여러 파일의 오류를 한 번에 모으고, 서로 참조하는 검사는 resolve에서 한다.
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
    /// 이 AGENT.md가 명시한 읽기 전용 내장 도구(상속하지 않는다)
    pub builtin: BTreeSet<Builtin>,
    pub part: String,
    pub env: BTreeMap<String, String>,
    pub skills: Vec<Arc<Skill>>,
    pub knowledge: Vec<Document>,
    pub triggers: Vec<Trigger>,
    pub memory: Vec<MemorySeed>,
}

pub struct Parsed {
    pub profile: ProfileParts,
    pub rules: Option<String>,
    pub knowledge: Vec<Document>,
    pub skills: Vec<Arc<Skill>>,
    /// 부모가 자식보다 앞선다.
    pub agents: Vec<ParsedAgent>,
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

/// 스키마 없이 YAML만 읽는다(번들 스키마가 없는 파일: `tools.lock.yaml`은 코드가 모양을 검사한다).
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

pub fn parse_all(
    tree: &Tree,
    layout: &Layout,
    folder: Option<&str>,
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
    let skills = skills(tree, &layout.skills, limits, issues);
    let agents: Vec<ParsedAgent> = layout
        .agents
        .iter()
        .filter_map(|a| agent(tree, a, limits, issues))
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
    limits: &Limits,
    issues: &mut Issues,
) -> Vec<Arc<Skill>> {
    files
        .iter()
        .filter_map(|f| skill(tree, f, limits, issues))
        .collect()
}

pub(crate) fn skill(
    tree: &Tree,
    files: &SkillFiles,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<Arc<Skill>> {
    let md_path = files.skill_md.as_deref()?;
    let md = text_of(tree, md_path, issues)?;
    let mut found = Vec::new();
    let parsed = frontmatter::parse(md_path, md, &Kind::Skill, limits, &mut found);
    issues.extend(found);
    let parsed = parsed?;
    let mut ok = true;
    let body = parsed.body.trim();
    if body.is_empty() {
        issues.error("empty-file", md_path, "SKILL.md에 지침이 없어요");
        ok = false;
    } else if body.len() > limits.max_skill_bytes {
        issues.error(
            "skill-too-large",
            md_path,
            format!(
                "SKILL.md 본문이 {} 바이트라 한도({} 바이트)를 넘어요. 지침을 줄이거나 긴 참고 자료는 knowledge/로 옮기세요",
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
        && declared != files.name
    {
        issues.error_at(
            "skill-name-mismatch",
            md_path,
            "front matter /name",
            format!(
                "front matter의 name `{declared}`이(가) 스킬 폴더 이름 `{}`와 달라요",
                files.name
            ),
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
            "설명이 비어 있어요",
        );
        ok = false;
    }
    for (key, label) in [("license", "license"), ("compatibility", "compatibility")] {
        if field(key).as_deref() == Some("") {
            issues.error_at(
                "schema",
                md_path,
                format!("front matter /{key}"),
                format!("{label}가 비어 있어요"),
            );
            ok = false;
        }
    }
    if !ok {
        return None;
    }
    // 잠금 파일은 tools.yaml이 `input`을 생략한 도구의 입력 스키마·정의 해시의 정본이다. 틀리면 그것을 쓰는 도구가 모두 막히므로 여기서 멈춘다.
    let lock = match files.lock.as_deref() {
        Some(path) if files.tools.is_none() => {
            issues.error(
                "lock-without-tools",
                path,
                "tools.lock.yaml은 같은 폴더의 tools.yaml이 있어야 해요",
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
            let value = yaml_of(tree, path, &bundled().tools, limits, issues)?;
            tools::parse(&value, &files.name, path, limits, lock.as_ref(), issues)?
        }
        None => Vec::new(),
    };
    Some(Arc::new(Skill {
        name: files.name.clone(),
        path: files.dir.clone(),
        description,
        instructions: Document {
            relative_path: md_path.to_owned(),
            text: body.to_owned(),
        },
        tools,
    }))
}

fn agent(
    tree: &Tree,
    files: &AgentFiles,
    limits: &Limits,
    issues: &mut Issues,
) -> Option<ParsedAgent> {
    let id = AgentId(files.id.clone());
    let md = text_of(tree, &files.agent_md, issues)?;
    let mut found = Vec::new();
    let parsed = frontmatter::parse(&files.agent_md, md, &Kind::Agent, limits, &mut found);
    issues.extend(found);
    let parsed = parsed?;
    if parsed.body.trim().is_empty() {
        issues.error("empty-file", &files.agent_md, "AGENT.md에 지침이 없어요");
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
            "설명이 비어 있어요",
        );
        return None;
    }
    // 값이 read·grep·glob 중 하나이고 중복이 없다는 것은 스키마가 이미 보증했다
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
        .and_then(|p| Some((p, yaml_of(tree, p, &bundled().triggers, limits, issues)?)))
        .map(|(path, value)| triggers::parse(&value, path, limits, issues))
        .unwrap_or_default();
    Some(ParsedAgent {
        part: parsed.body.trim().to_owned(),
        model,
        description,
        builtin,
        env,
        skills: skills(tree, &files.skills, limits, issues),
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
            format!("환경 변수가 너무 많아요(최대 {})", limits.max_env_vars),
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
                "이 이름은 쓸 수 없어요(`BOT_`·`LD_`·`DYLD_` 접두사, `PATH`, 128자 초과)",
            );
            continue;
        }
        let value = v.as_str().unwrap_or_default();
        if value.len() > limits.max_env_value {
            issues.error_at(
                "too-large",
                path,
                format!("/{key}"),
                format!("값이 너무 길어요(최대 {} 바이트)", limits.max_env_value),
            );
            continue;
        }
        if SECRETISH.iter().any(|w| upper.contains(w)) {
            issues.warning("env-looks-secret", path, format!("`{key}`는 비밀처럼 보여요. env.yaml에는 비밀을 넣지 말고 tools.yaml의 secrets를 쓰세요"));
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
            format!("기억 시드가 너무 많아요(최대 {})", limits.max_memory_seeds),
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
                format!("owner는 `<종류>:<이름>` 모양이어야 해요: {owner}"),
            );
            continue;
        }
        if parsed.body.trim().is_empty() {
            issues.error("empty-file", path, "기억 시드에 내용이 없어요");
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
