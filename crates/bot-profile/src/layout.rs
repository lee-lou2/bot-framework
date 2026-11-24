//! 폴더 구조 검사. 알려진 이름만 허용한다 — 오타 난 `trigger.yaml`이 조용히 무시되면 안 된다.
use crate::{issue::Issues, limits::Limits, names, scan::Tree};

#[derive(Debug, Default)]
pub struct SkillFiles {
    pub name: String,
    pub dir: String,
    pub skill_md: Option<String>,
    pub tools: Option<String>,
}

#[derive(Debug, Default)]
pub struct AgentFiles {
    /// 계보 경로(`main/research`)
    pub id: String,
    pub parent: Option<String>,
    pub agent_md: String,
    pub env: Option<String>,
    pub triggers: Option<String>,
    pub memory: Vec<String>,
    pub knowledge: Vec<String>,
    pub skills: Vec<SkillFiles>,
}

#[derive(Debug, Default)]
pub struct Layout {
    pub rules: Option<String>,
    pub knowledge: Vec<String>,
    pub skills: Vec<SkillFiles>,
    /// 부모가 자식보다 앞선다.
    pub agents: Vec<AgentFiles>,
}

/// 계보 경로(`main/research`)의 폴더(`agents/main/agents/research`)
pub fn agent_dir(id: &str) -> String {
    format!("agents/{}", id.replace('/', "/agents/"))
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn discover(tree: &Tree, limits: &Limits, issues: &mut Issues) -> Layout {
    let mut layout = Layout::default();
    for file in tree.child_files("") {
        match file {
            "profile.yaml" => {}
            "rules.md" => layout.rules = Some(file.to_owned()),
            _ => unknown(
                issues,
                "unknown-file",
                file,
                "프로필 폴더에는 profile.yaml·rules.md만 둘 수 있어요",
            ),
        }
    }
    if tree.file("profile.yaml").is_none() {
        issues.error("missing-file", "profile.yaml", "profile.yaml이 필요해요");
    }
    for dir in tree.child_dirs("") {
        if !matches!(dir, "agents" | "knowledge" | "skills") {
            unknown(
                issues,
                "unknown-dir",
                dir,
                "프로필 폴더에는 agents·knowledge·skills만 둘 수 있어요",
            );
        }
    }
    layout.knowledge = documents(tree, "knowledge", &["md", "txt"], issues);
    layout.skills = skills(tree, "skills", limits, issues);
    agents(tree, limits, issues, &mut layout);
    layout
}

fn unknown(issues: &mut Issues, code: &'static str, path: &str, hint: &str) {
    issues.error(code, path, format!("알 수 없는 항목이에요. {hint}"));
}

/// `dir/` 아래 모든 문서. 확장자는 소문자 `exts`만 허용한다.
fn documents(tree: &Tree, dir: &str, exts: &[&str], issues: &mut Issues) -> Vec<String> {
    let mut found = Vec::new();
    for path in tree.files_under(dir) {
        let ext = path.rsplit_once('.').map(|(_, e)| e);
        if ext.is_some_and(|e| exts.contains(&e)) {
            found.push(path.to_owned());
        } else {
            let allowed = exts
                .iter()
                .map(|e| format!(".{e}"))
                .collect::<Vec<_>>()
                .join("·");
            issues.error(
                "bad-document",
                path,
                format!("이 폴더에는 {allowed} 파일만 둘 수 있어요"),
            );
        }
    }
    found
}

fn skills(tree: &Tree, dir: &str, limits: &Limits, issues: &mut Issues) -> Vec<SkillFiles> {
    for file in tree.child_files(dir) {
        unknown(
            issues,
            "unknown-file",
            &join(dir, file),
            "스킬은 폴더로 만들어요(`skills/<이름>/SKILL.md`)",
        );
    }
    let names = tree.child_dirs(dir);
    if names.len() > limits.max_skills {
        issues.error(
            "too-many",
            dir,
            format!("스킬이 너무 많아요(최대 {})", limits.max_skills),
        );
    }
    let mut out = Vec::new();
    for name in names {
        let path = join(dir, name);
        if !names::is_name(name) {
            issues.error(
                "bad-name",
                &path,
                "스킬 이름은 소문자로 시작하는 소문자·숫자·`-` 63자 이하예요",
            );
            continue;
        }
        let mut skill = SkillFiles {
            name: name.to_owned(),
            dir: path.clone(),
            ..SkillFiles::default()
        };
        for file in tree.child_files(&path) {
            match file {
                "SKILL.md" => skill.skill_md = Some(join(&path, file)),
                "tools.yaml" => skill.tools = Some(join(&path, file)),
                _ => unknown(
                    issues,
                    "unknown-file",
                    &join(&path, file),
                    "스킬 폴더에는 SKILL.md·tools.yaml만 둘 수 있어요",
                ),
            }
        }
        for sub in tree.child_dirs(&path) {
            unknown(
                issues,
                "unknown-dir",
                &join(&path, sub),
                "스킬 폴더에는 하위 폴더를 둘 수 없어요",
            );
        }
        if skill.skill_md.is_none() {
            issues.error(
                "missing-file",
                &join(&path, "SKILL.md"),
                "스킬에는 SKILL.md가 필요해요",
            );
        }
        out.push(skill);
    }
    out
}

fn agents(tree: &Tree, limits: &Limits, issues: &mut Issues, layout: &mut Layout) {
    for file in tree.child_files("agents") {
        unknown(
            issues,
            "unknown-file",
            &join("agents", file),
            "agents에는 에이전트 폴더만 둘 수 있어요",
        );
    }
    let roots = tree.child_dirs("agents");
    match roots.len() {
        0 => issues.error(
            "no-root-agent",
            "agents",
            "agents 아래에 루트 에이전트 폴더가 하나 필요해요",
        ),
        1 => {}
        _ => issues.error(
            "multiple-root-agents",
            "agents",
            format!(
                "루트 에이전트는 하나예요(지금 {}개: {})",
                roots.len(),
                roots.join(", ")
            ),
        ),
    }
    let mut stack: Vec<(String, String, Option<String>, usize)> = roots
        .iter()
        .rev()
        .map(|r| (join("agents", r), (*r).to_owned(), None, 1))
        .collect();
    while let Some((dir, id, parent, depth)) = stack.pop() {
        let leaf = id.rsplit('/').next().unwrap_or_default();
        if !names::is_name(leaf) {
            issues.error(
                "bad-name",
                &dir,
                "에이전트 이름은 소문자로 시작하는 소문자·숫자·`-` 63자 이하예요",
            );
            continue;
        }
        if depth > limits.max_agent_depth {
            issues.error(
                "too-deep",
                &dir,
                format!(
                    "에이전트 계보가 너무 깊어요(최대 {}단계)",
                    limits.max_agent_depth
                ),
            );
            continue;
        }
        if layout.agents.len() >= limits.max_agents {
            issues.error(
                "too-many",
                &dir,
                format!("에이전트가 너무 많아요(최대 {})", limits.max_agents),
            );
            return;
        }
        let mut agent = AgentFiles {
            id: id.clone(),
            parent,
            ..AgentFiles::default()
        };
        agent_entries(tree, &dir, &mut agent, issues);
        agent.memory = documents(tree, &join(&dir, "memory"), &["md"], issues);
        agent.knowledge = documents(tree, &join(&dir, "knowledge"), &["md", "txt"], issues);
        agent.skills = skills(tree, &join(&dir, "skills"), limits, issues);
        let children = join(&dir, "agents");
        for file in tree.child_files(&children) {
            unknown(
                issues,
                "unknown-file",
                &join(&children, file),
                "agents에는 에이전트 폴더만 둘 수 있어요",
            );
        }
        for child in tree.child_dirs(&children).into_iter().rev() {
            stack.push((
                join(&children, child),
                format!("{id}/{child}"),
                Some(id.clone()),
                depth + 1,
            ));
        }
        layout.agents.push(agent);
    }
}

fn agent_entries(tree: &Tree, dir: &str, agent: &mut AgentFiles, issues: &mut Issues) {
    for file in tree.child_files(dir) {
        let path = join(dir, file);
        match file {
            "AGENT.md" => agent.agent_md = path,
            "env.yaml" => agent.env = Some(path),
            "triggers.yaml" => agent.triggers = Some(path),
            _ => unknown(
                issues,
                "unknown-file",
                &path,
                "에이전트 폴더에는 AGENT.md·env.yaml·triggers.yaml만 둘 수 있어요",
            ),
        }
    }
    if agent.agent_md.is_empty() {
        issues.error(
            "missing-file",
            &join(dir, "AGENT.md"),
            "에이전트에는 AGENT.md가 필요해요",
        );
    }
    for sub in tree.child_dirs(dir) {
        if !matches!(sub, "memory" | "knowledge" | "skills" | "agents") {
            unknown(
                issues,
                "unknown-dir",
                &join(dir, sub),
                "에이전트 폴더에는 memory·knowledge·skills·agents만 둘 수 있어요",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(paths: &[&str]) -> Tree {
        Tree::from_files(paths.iter().map(|p| (*p, b"x".to_vec())))
    }

    fn run(paths: &[&str]) -> (Layout, Vec<&'static str>) {
        let mut issues = Issues::new(100);
        let layout = discover(&tree(paths), &Limits::default(), &mut issues);
        let codes = issues.items().iter().map(|i| i.code).collect();
        (layout, codes)
    }

    #[test]
    fn agent_dirs() {
        assert_eq!(agent_dir("main"), "agents/main");
        assert_eq!(
            agent_dir("main/research/deep"),
            "agents/main/agents/research/agents/deep"
        );
    }

    #[test]
    fn finds_nested_agents_parent_first() {
        let (layout, codes) = run(&[
            "profile.yaml",
            "agents/main/AGENT.md",
            "agents/main/agents/research/AGENT.md",
            "agents/main/agents/research/agents/deep/AGENT.md",
            "agents/main/agents/notes/AGENT.md",
        ]);
        assert!(codes.is_empty(), "{codes:?}");
        let ids: Vec<_> = layout.agents.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["main", "main/notes", "main/research", "main/research/deep"]
        );
        assert_eq!(layout.agents[3].parent.as_deref(), Some("main/research"));
    }

    #[test]
    fn rejects_unknown_names_and_missing_pieces() {
        let (_, codes) = run(&[
            "profile.yaml",
            "agents/main/AGENT.md",
            "agents/main/trigger.yaml",
        ]);
        assert_eq!(codes, ["unknown-file"]);
        let (_, codes) = run(&[
            "profile.yaml",
            "agents/main/AGENT.md",
            "agents/main/skills/x/tools.yaml",
        ]);
        assert_eq!(codes, ["missing-file"]);
        let (_, codes) = run(&["agents/main/AGENT.md", "extra.txt", "secrets/key"]);
        assert!(
            codes.contains(&"missing-file")
                && codes.contains(&"unknown-file")
                && codes.contains(&"unknown-dir")
        );
        let (_, codes) = run(&["profile.yaml", "agents/main/agents/x/memory/a.md"]);
        assert!(codes.contains(&"missing-file"), "{codes:?}");
    }

    #[test]
    fn exactly_one_root_agent() {
        assert_eq!(run(&["profile.yaml"]).1, ["no-root-agent"]);
        let (_, codes) = run(&["profile.yaml", "agents/a/AGENT.md", "agents/b/AGENT.md"]);
        assert_eq!(codes, ["multiple-root-agents"]);
    }

    #[test]
    fn documents_have_fixed_extensions() {
        let (layout, codes) = run(&[
            "profile.yaml",
            "agents/main/AGENT.md",
            "knowledge/guide.md",
            "knowledge/sub/회의 규칙.txt",
            "knowledge/data.csv",
            "agents/main/memory/a.md",
            "agents/main/memory/b.txt",
        ]);
        assert_eq!(codes, ["bad-document", "bad-document"]);
        assert_eq!(
            layout.knowledge,
            ["knowledge/guide.md", "knowledge/sub/회의 규칙.txt"]
        );
        assert_eq!(layout.agents[0].memory, ["agents/main/memory/a.md"]);
    }

    #[test]
    fn bad_names_and_depth() {
        let (_, codes) = run(&["profile.yaml", "agents/Main/AGENT.md"]);
        assert_eq!(codes, ["bad-name"]);
        let mut paths = vec!["profile.yaml".to_owned(), "agents/a/AGENT.md".to_owned()];
        let mut dir = "agents/a".to_owned();
        for level in 0..9 {
            dir = format!("{dir}/agents/n{level}");
            paths.push(format!("{dir}/AGENT.md"));
        }
        let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        assert!(run(&refs).1.contains(&"too-deep"));
    }
}
