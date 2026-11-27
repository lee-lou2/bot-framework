//! Inheritance assembly tests: instructions, env, model, scopes, skill/knowledge visibility, memory seeds, and trigger ownership.

use crate::support::{Files, agent, append, edit, family, snapshot};
use bot_core::{profile::Skill, state::Visibility};

fn skill(files: Files, dir: &str, name: &str) -> Files {
    let tools = "version: 1\nserver: demo-notes\ntools:\n  - id: ping\n    class: read\n    scopes: [calendar.read]\n    input: {type: object, properties: {}, additionalProperties: false}\n".to_owned();
    let root = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let files = edit(
        files,
        &format!("{root}skills/{name}/SKILL.md"),
        "",
        &format!("{name} skill guide\n"),
    );
    edit(
        files,
        &format!("{root}skills/{name}/tools.yaml"),
        "",
        &tools,
    )
}

fn names<T>(map: &std::collections::BTreeMap<String, T>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
}

#[test]
fn inherits_instructions_env_model_and_scopes() {
    let mut files = family();
    files = edit(
        files,
        "agents/main/env.yaml",
        "AGENDA_VIEW: today",
        "A: root\nB: root",
    );
    files = edit(
        files,
        "agents/main/agents/research/env.yaml",
        "",
        "B: child\nC: child\n",
    );
    files = edit(
        files,
        "agents/main/agents/research/AGENT.md",
        "Handle",
        "---\nmodel: fast\n---\nHandle",
    );
    files = edit(
        files,
        "profile.yaml",
        "    main/research: [run, calendar.read]",
        "    main/research: [run, calendar.read]\n    main/research/deep: [calendar.read, calendar.delete]",
    );
    let snap = snapshot(&files);
    let (main, research, deep, other) = (
        &snap.agents[&agent("main")],
        &snap.agents[&agent("main/research")],
        &snap.agents[&agent("main/research/deep")],
        &snap.agents[&agent("main/other")],
    );
    assert_eq!(
        main.instructions,
        "- Keep answers brief and in English.\n\nCheck today's schedule and confirm the exact event before making changes."
    );
    assert!(
        deep.instructions.starts_with(&main.instructions),
        "child instructions append after parent instructions"
    );
    assert!(deep.instructions.ends_with("Dig deeper."));
    assert_eq!(
        other
            .instructions
            .matches("Handle schedule lookups")
            .count(),
        0,
        "sibling instructions must not leak"
    );
    let env = |a: &bot_core::profile::Agent| {
        a.env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
    };
    assert_eq!(env(main), ["A=root", "B=root"]);
    assert_eq!(
        env(research),
        ["A=root", "B=child", "C=child"],
        "child env overrides parent"
    );
    assert_eq!(env(deep), env(research));
    assert_eq!(env(other), env(main));
    assert_eq!(
        (
            main.model.as_str(),
            research.model.as_str(),
            deep.model.as_str(),
            other.model.as_str()
        ),
        ("assistant", "fast", "fast", "assistant")
    );
    let scopes = |a: &bot_core::profile::Agent| a.scopes.iter().cloned().collect::<Vec<_>>();
    assert_eq!(scopes(main), ["calendar.delete", "calendar.read", "run"]);
    assert_eq!(scopes(research), ["calendar.read", "run"]);
    assert_eq!(
        scopes(deep),
        ["calendar.read"],
        "child scopes are intersected with parent ceiling"
    );
    assert_eq!(
        scopes(other),
        scopes(main),
        "omitted policy.agents entries inherit parent ceiling"
    );
}

#[test]
fn skills_and_knowledge_are_visible_only_from_ancestors_and_self() {
    let mut files = family();
    files = skill(files, "", "shared");
    files = skill(files, "agents/main/agents/research", "notes");
    files = skill(files, "agents/main/agents/other", "mail");
    files = edit(
        files,
        "agents/main/agents/research/knowledge/own.md",
        "",
        "Research notes\n",
    );
    files = edit(
        files,
        "agents/main/agents/other/knowledge/mail.md",
        "",
        "Mail rules\n",
    );
    let snap = snapshot(&files);
    let seen = |id: &str| {
        names(&snap.agents[&agent(id)].skills)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(seen("main"), ["agenda", "shared"]);
    assert_eq!(seen("main/research"), ["agenda", "notes", "shared"]);
    assert_eq!(seen("main/research/deep"), ["agenda", "notes", "shared"]);
    assert_eq!(seen("main/other"), ["agenda", "mail", "shared"]);
    let docs = |id: &str| {
        snap.agents[&agent(id)]
            .knowledge
            .iter()
            .map(|d| d.relative_path.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        docs("main"),
        ["knowledge/guide.md", "agents/main/knowledge/habits.md"]
    );
    assert_eq!(
        docs("main/research")[2],
        "agents/main/agents/research/knowledge/own.md"
    );
    assert_eq!(docs("main/research/deep"), docs("main/research"));
    assert!(
        docs("main/other").iter().all(|p| !p.contains("research")) && docs("main/other").len() == 3
    );
    let notes: &Skill = &snap.agents[&agent("main/research")].skills["notes"];
    assert_eq!(
        notes.tools.keys().map(|t| t.0.as_str()).collect::<Vec<_>>(),
        ["notes.ping"]
    );
    assert_eq!(
        notes.instructions.relative_path,
        "agents/main/agents/research/skills/notes/SKILL.md"
    );
    assert!(!snap.agents[&agent("main")].skills.contains_key("notes"));
}

#[test]
fn same_skill_name_in_sibling_branches_is_not_a_conflict() {
    let mut files = family();
    files = skill(files, "agents/main/agents/research", "notes");
    files = skill(files, "agents/main/agents/other", "notes");
    let snap = snapshot(&files);
    assert!(
        snap.agents[&agent("main/research")]
            .skills
            .contains_key("notes")
    );
    assert!(
        snap.agents[&agent("main/other")]
            .skills
            .contains_key("notes")
    );
}

#[test]
fn memory_seeds_stay_with_their_agent() {
    let mut files = family();
    files = edit(
        files,
        "agents/main/agents/research/memory/note.md",
        "",
        "---\nowner: \"local:owner\"\nvisibility: profile\n---\nResearch direction\n",
    );
    let snap = snapshot(&files);
    let main = &snap.agents[&agent("main")];
    assert_eq!(main.memory_seeds.len(), 1);
    assert_eq!(
        (
            main.memory_seeds[0].visibility,
            main.memory_seeds[0].document.text.as_str()
        ),
        (Visibility::Private, "Check the schedule every morning.")
    );
    assert_eq!(
        main.memory_seeds[0].document.relative_path,
        "agents/main/memory/routine.md"
    );
    let research = &snap.agents[&agent("main/research")];
    assert_eq!(research.memory_seeds.len(), 1);
    assert_eq!(
        (
            research.memory_seeds[0].visibility,
            research.memory_seeds[0].owner.0.as_str()
        ),
        (Visibility::Profile, "local:owner")
    );
    assert!(snap.agents[&agent("main/other")].memory_seeds.is_empty());
}

#[test]
fn triggers_belong_to_the_declaring_agent_only() {
    let files = append(
        edit(
            family(),
            "agents/main/agents/other/triggers.yaml",
            "",
            "version: 1\ntriggers:\n",
        ),
        "agents/main/agents/other/triggers.yaml",
        "  - id: nightly\n    \"on\": {cron: \"0 3 * * *\"}\n    principal: \"service:morning\"\n    beat: true\n    target: {tool: agenda.list}\n",
    );
    let snap = snapshot(&files);
    assert_eq!(snap.agents[&agent("main")].triggers.len(), 1);
    assert_eq!(snap.agents[&agent("main/other")].triggers.len(), 1);
    assert!(snap.agents[&agent("main/research")].triggers.is_empty());
    match &snap.agents[&agent("main/other")].triggers[0].on {
        bot_core::profile::TriggerSource::Cron { beat, schedule, .. } => {
            assert_eq!((*beat, schedule.as_str()), (true, "0 3 * * *"))
        }
        other => panic!("{other:?}"),
    }
}
