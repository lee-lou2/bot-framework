//! Markdown YAML front matter parsing and schema validation for AGENT.md, SKILL.md, and memory seeds.
use crate::{
    issue::Issue,
    limits::Limits,
    schema::Schema,
    schemas::bundled,
    text::{self, FrontMatter},
    yaml,
};
use serde_json::{Value, json};
use std::sync::OnceLock;

fn memory_schema() -> &'static Schema {
    static CELL: OnceLock<Schema> = OnceLock::new();
    CELL.get_or_init(|| {
        Schema::compile_bundled(json!({
            "type": "object",
            "properties": {"owner": {"type": "string", "minLength": 1}, "visibility": {"enum": ["private", "profile"]}},
            "required": ["owner"],
            "additionalProperties": false
        }))
        .unwrap_or_else(|e| panic!("front matter schema: {e:?}"))
    })
}

pub enum Kind {
    Agent,
    Skill,
    Memory,
}

pub struct Parsed<'a> {
    pub meta: Option<Value>,
    pub body: &'a str,
}

pub fn parse<'a>(
    path: &str,
    text: &'a str,
    kind: &Kind,
    limits: &Limits,
    issues: &mut Vec<Issue>,
) -> Option<Parsed<'a>> {
    let FrontMatter { yaml, body } = match text::split_front_matter(path, text) {
        Ok(fm) => fm,
        Err(issue) => {
            issues.push(issue);
            return None;
        }
    };
    let Some(block) = yaml else {
        let required = matches!(kind, Kind::Memory);
        if required {
            issues.push(Issue::error(
                "memory-owner-missing",
                path,
                "memory seeds require YAML front matter (`---`) with an owner",
            ));
            return None;
        }
        return Some(Parsed { meta: None, body });
    };
    let value = match yaml::parse(path, block, limits) {
        Ok(v) => v,
        Err(issue) => {
            issues.push(Issue {
                at: format!("front matter {}", issue.at),
                ..issue
            });
            return None;
        }
    };
    let schema = match kind {
        Kind::Agent => &bundled().agent,
        Kind::Skill => &bundled().skill,
        Kind::Memory => memory_schema(),
    };
    let found = schema.validate(&value);
    for v in &found {
        if matches!(kind, Kind::Skill) && v.pointer == "/allowed-tools" {
            issues.push(
                Issue::error(
                    "skill-allowed-tools",
                    path,
                    "`allowed-tools` is not allowed; skill docs cannot grant permissions — declare tool access via `tools.yaml` and `profile.yaml` (`roles` and `policy`)",
                )
                .at("front matter /allowed-tools"),
            );
        } else {
            issues.push(
                Issue::error("schema", path, v.message.clone())
                    .at(format!("front matter {}", v.pointer)),
            );
        }
    }
    found.is_empty().then_some(Parsed {
        meta: Some(value),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, kind: &Kind) -> (Option<Value>, Vec<&'static str>) {
        let mut issues = Vec::new();
        let parsed = parse("a.md", text, kind, &Limits::default(), &mut issues);
        (
            parsed.and_then(|p| p.meta),
            issues.iter().map(|i| i.code).collect(),
        )
    }

    #[test]
    fn agent_front_matter_allows_only_model_description_and_builtin() {
        assert_eq!(
            run("---\nmodel: fast\n---\nbody", &Kind::Agent).0,
            Some(json!({"model": "fast"}))
        );
        assert_eq!(run("body", &Kind::Agent), (None, vec![]));
        assert_eq!(
            run("---\nmodel: Fast\n---\nbody", &Kind::Agent).1,
            ["schema"]
        );
        assert_eq!(
            run("---\nmodel: fast\ntools: x\n---\n", &Kind::Agent).1,
            ["schema"]
        );
        assert_eq!(
            run("---\nmodel: &a fast\n---\n", &Kind::Agent).1,
            ["yaml-anchor"]
        );
        assert_eq!(
            run("---\nbuiltin: [read, grep, glob]\n---\nbody", &Kind::Agent).0,
            Some(json!({"builtin": ["read", "grep", "glob"]}))
        );
        for bad in [
            "builtin: [read, read]",
            "builtin: [bash]",
            "builtin: [Read]",
            "builtin: read",
            "builtin: [read, grep, glob, read]",
        ] {
            let codes = run(&format!("---\n{bad}\n---\nbody"), &Kind::Agent).1;
            // A single case may violate multiple rules (e.g. maxItems and uniqueItems).
            assert!(
                !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
                "{bad}: {codes:?}"
            );
        }
    }

    #[test]
    fn memory_requires_owner() {
        assert_eq!(run("body", &Kind::Memory).1, ["memory-owner-missing"]);
        assert_eq!(
            run("---\nvisibility: profile\n---\nx", &Kind::Memory).1,
            ["schema"]
        );
        assert_eq!(
            run(
                "---\nowner: local:owner\nvisibility: shared\n---\nx",
                &Kind::Memory
            )
            .1,
            ["schema"]
        );
        assert!(
            run("---\nowner: local:owner\n---\nx", &Kind::Memory)
                .0
                .is_some()
        );
    }

    #[test]
    fn skill_front_matter_follows_agent_skills_fields_and_rejects_allowed_tools() {
        assert_eq!(run("body", &Kind::Skill), (None, vec![]));
        let full = "---\nname: pdf-processing\ndescription: Reads and summarizes PDF documents\nlicense: Apache-2.0\ncompatibility: Python 3.12+\nmetadata:\n  author: example-org\n  version: \"1.0\"\n---\nbody";
        let (meta, codes) = run(full, &Kind::Skill);
        assert!(codes.is_empty(), "{codes:?}");
        assert_eq!(
            meta,
            Some(json!({
                "name": "pdf-processing",
                "description": "Reads and summarizes PDF documents",
                "license": "Apache-2.0",
                "compatibility": "Python 3.12+",
                "metadata": {"author": "example-org", "version": "1.0"}
            }))
        );
        let mut issues = Vec::new();
        assert!(
            parse(
                "skills/x/SKILL.md",
                "---\nname: x\nallowed-tools: Bash(git:*) Read\n---\nbody",
                &Kind::Skill,
                &Limits::default(),
                &mut issues
            )
            .is_none()
        );
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert_eq!(issues[0].code, "skill-allowed-tools");
        assert_eq!(issues[0].at, "front matter /allowed-tools");
        assert!(
            issues[0].message.contains("tools.yaml") && issues[0].message.contains("policy"),
            "{}",
            issues[0].message
        );
        assert_eq!(
            run("---\nname: x\nbody", &Kind::Skill).1,
            ["front-matter-unclosed"]
        );
        assert_eq!(
            run("---\nname: &a x\n---\nbody", &Kind::Skill).1,
            ["yaml-anchor"]
        );
        // Max character boundaries (name: 64, description: 1024, compatibility: 500) using 3-byte UTF-8 chars.
        let boundary = format!(
            "---\nname: {}\ndescription: {}\ncompatibility: {}\n---\nbody",
            "a".repeat(64),
            "€".repeat(1024),
            "★".repeat(500),
        );
        assert!(run(&boundary, &Kind::Skill).1.is_empty());
        let bad_cases = [
            "unknown: 1".to_owned(),
            "name: PDF-Processing".to_owned(),
            "name: -pdf".to_owned(),
            "name: pdf-".to_owned(),
            "name: pdf--processing".to_owned(),
            "name: pdf_processing".to_owned(),
            format!("name: {}", "a".repeat(65)),
            "description: \"\"".to_owned(),
            format!("description: {}", "€".repeat(1025)),
            "compatibility: \"\"".to_owned(),
            format!("compatibility: {}", "★".repeat(501)),
            "license: \"\"".to_owned(),
            "metadata:\n  version: 1.0".to_owned(),
            "metadata:\n  \"\": x".to_owned(),
            "metadata: [a]".to_owned(),
        ];
        for bad in bad_cases {
            let codes = run(&format!("---\n{bad}\n---\nbody"), &Kind::Skill).1;
            assert!(
                !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
                "{bad}: {codes:?}"
            );
        }
    }
}
