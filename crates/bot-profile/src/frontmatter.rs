//! Markdown 맨 위 YAML front matter. AGENT.md는 번들 스키마(`model`·`description`·`builtin`), 기억 시드는 `owner`(필수)·`visibility`.
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
        .unwrap_or_else(|e| panic!("front matter 스키마: {e:?}"))
    })
}

pub enum Kind {
    Agent,
    Memory,
}

pub struct Parsed<'a> {
    /// front matter(없으면 None)
    pub meta: Option<Value>,
    pub body: &'a str,
}

/// 글 맨 위의 front matter를 읽고 종류별 스키마로 검사한다.
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
                "기억 시드에는 owner가 있는 front matter(`---`)가 필요해요",
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
        Kind::Memory => memory_schema(),
    };
    let found = schema.validate(&value);
    for v in &found {
        issues.push(
            Issue::error("schema", path, v.message.clone())
                .at(format!("front matter {}", v.pointer)),
        );
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
            run("---\nmodel: fast\n---\n본문", &Kind::Agent).0,
            Some(json!({"model": "fast"}))
        );
        assert_eq!(run("본문", &Kind::Agent), (None, vec![]));
        assert_eq!(
            run("---\nmodel: Fast\n---\n본문", &Kind::Agent).1,
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
            run("---\nbuiltin: [read, grep, glob]\n---\n본문", &Kind::Agent).0,
            Some(json!({"builtin": ["read", "grep", "glob"]}))
        );
        for bad in [
            "builtin: [read, read]",
            "builtin: [bash]",
            "builtin: [Read]",
            "builtin: read",
            "builtin: [read, grep, glob, read]",
        ] {
            let codes = run(&format!("---\n{bad}\n---\n본문"), &Kind::Agent).1;
            // 한 사례가 여러 규칙(maxItems·uniqueItems)을 어길 수 있다
            assert!(
                !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
                "{bad}: {codes:?}"
            );
        }
    }

    #[test]
    fn memory_requires_owner() {
        assert_eq!(run("본문", &Kind::Memory).1, ["memory-owner-missing"]);
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
}
