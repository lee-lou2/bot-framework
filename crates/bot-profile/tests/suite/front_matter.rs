//! Front matter tests for AGENT.md (`description`, `builtin`) and SKILL.md metadata and body limits.

use crate::support::{AGENT_MD, agent, base, edit, family, snapshot};

#[test]
fn description_comes_from_front_matter_and_is_not_inherited() {
    let mut files = family();
    files = edit(
        files,
        AGENT_MD,
        "Check",
        "---\ndescription: Assistant that checks and organizes schedules\n---\nCheck",
    );
    let snap = snapshot(&files);
    let desc = |id: &str| snap.agents[&agent(id)].description.clone();
    assert_eq!(
        desc("main"),
        "Assistant that checks and organizes schedules"
    );
    assert_eq!(
        desc("main/research"),
        "research",
        "description is not inherited and defaults to the last path segment"
    );
    assert_eq!(desc("main/research/deep"), "deep");
    assert_eq!(desc("main/other"), "other");

    let both = edit(
        base(),
        AGENT_MD,
        "Check",
        "---\nmodel: fast\ndescription:   Trim surrounding spaces  \n---\nCheck",
    );
    let snap = snapshot(&both);
    let main = &snap.agents[&agent("main")];
    assert_eq!(
        (main.model.as_str(), main.description.as_str()),
        ("fast", "Trim surrounding spaces")
    );
    assert!(
        !main.instructions.contains("description"),
        "front matter must be stripped from instructions"
    );
}

#[test]
fn invalid_descriptions_are_rejected() {
    let long = "€".repeat(201);
    for (name, front) in [
        ("empty string", "description: \"\"".to_owned()),
        ("whitespace only", "description: \"   \"".to_owned()),
        (
            "multiline",
            "description: \"line one\\nline two\"".to_owned(),
        ),
        ("too long", format!("description: {long}")),
        ("not a string", "description: [a]".to_owned()),
    ] {
        let files = edit(
            base(),
            AGENT_MD,
            "Check",
            &format!("---\n{front}\n---\nCheck"),
        );
        let codes = crate::support::codes(&files);
        assert!(
            !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
            "{name}: {codes:?}"
        );
    }
    let ok = edit(
        base(),
        AGENT_MD,
        "Check",
        &format!("---\ndescription: {}\n---\nCheck", "€".repeat(200)),
    );
    crate::support::ok(&ok);
}

#[test]
fn builtin_tools_are_declared_per_agent_and_not_inherited() {
    use bot_core::profile::Builtin;
    let mut files = family();
    files = edit(
        files,
        AGENT_MD,
        "Check",
        "---\nbuiltin: [grep, read]\n---\nCheck",
    );
    files = edit(
        files,
        "agents/main/agents/research/AGENT.md",
        "Handle",
        "---\nmodel: fast\nbuiltin: [glob]\n---\nHandle",
    );
    let snap = snapshot(&files);
    let set = |id: &str| {
        snap.agents[&agent(id)]
            .builtin
            .iter()
            .copied()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        set("main"),
        [Builtin::Read, Builtin::Grep],
        "sorted regardless of declaration order"
    );
    assert_eq!(
        set("main/research"),
        [Builtin::Glob],
        "only own builtin tools, not inherited from parent"
    );
    assert!(set("main/research/deep").is_empty());
    assert!(set("main/other").is_empty());
    let empty = snapshot(&edit(
        base(),
        AGENT_MD,
        "Check",
        "---\nbuiltin: []\n---\nCheck",
    ));
    assert!(empty.agents.values().all(|a| a.builtin.is_empty()));
    let all = snapshot(&edit(
        base(),
        AGENT_MD,
        "Check",
        "---\nmodel: fast\ndescription: Assistant\nbuiltin: [read, grep, glob]\n---\nCheck",
    ));
    assert_eq!(all.agents[&agent("main")].builtin.len(), 3);
}

#[test]
fn invalid_builtin_declarations_are_rejected() {
    for (name, front) in [
        ("duplicate", "builtin: [read, read]"),
        ("unregistered tool", "builtin: [bash]"),
        ("write tool", "builtin: [write]"),
        ("web", "builtin: [web]"),
        ("uppercase", "builtin: [Read]"),
        ("not an array", "builtin: read"),
        ("four items", "builtin: [read, grep, glob, read]"),
        ("object item", "builtin: [{name: read}]"),
    ] {
        let files = edit(
            base(),
            AGENT_MD,
            "Check",
            &format!("---\n{front}\n---\nCheck"),
        );
        let codes = crate::support::codes(&files);
        assert!(
            !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
            "{name}: {codes:?}"
        );
    }
}

const SKILL_MD: &str = "agents/main/skills/agenda/SKILL.md";

#[test]
fn skill_front_matter_is_optional_validated_and_stripped_from_instructions() {
    let plain = snapshot(&base());
    let agenda = &plain.agents[&agent("main")].skills["agenda"];
    assert_eq!(agenda.description, None);
    assert_eq!(agenda.instructions.text, "Use list to view the schedule.");

    // All allowed fields, preserving horizontal rules (`---`) in body and trimming description.
    let with_fm = edit(
        base(),
        SKILL_MD,
        "Use list to view the schedule.\n",
        "---\nname: agenda\ndescription:   View and manage schedules  \nlicense: MIT\ncompatibility: Local MCP server\nmetadata:\n  author: example-org\n  version: \"1.0\"\n---\n\n# Schedule\nUse list to view the schedule.\n\n---\n\nExtra note\n",
    );
    let snap = snapshot(&with_fm);
    let agenda = &snap.agents[&agent("main")].skills["agenda"];
    assert_eq!(
        agenda.description.as_deref(),
        Some("View and manage schedules")
    );
    assert_eq!(
        agenda.instructions.text,
        "# Schedule\nUse list to view the schedule.\n\n---\n\nExtra note"
    );
    assert!(
        !agenda.instructions.text.contains("name: agenda")
            && !agenda.instructions.text.contains("description:"),
        "front matter must not remain in skill body: {}",
        agenda.instructions.text
    );

    // Optional front matter with description only (`name` omitted) is inherited intact by child agents.
    let desc_only = edit(
        base(),
        SKILL_MD,
        "Use list to view the schedule.\n",
        "---\ndescription: Schedule lookup\n---\nUse list to view the schedule.\n",
    );
    let snap = snapshot(&desc_only);
    assert_eq!(
        snap.agents[&agent("main/research")].skills["agenda"]
            .description
            .as_deref(),
        Some("Schedule lookup")
    );
    assert_eq!(
        snap.agents[&agent("main/research")].skills["agenda"]
            .instructions
            .text,
        "Use list to view the schedule."
    );
}

#[test]
fn invalid_skill_front_matter_is_rejected() {
    let allowed = edit(
        base(),
        SKILL_MD,
        "Use list",
        "---\nname: agenda\nallowed-tools: Bash(git:*) Read\n---\nUse list",
    );
    let report = crate::support::check(&allowed).unwrap_err();
    assert_eq!(report.codes(), ["skill-allowed-tools"]);
    let msg = &report.errors().next().unwrap().message;
    assert!(
        msg.contains("tools.yaml") && msg.to_lowercase().contains("policy"),
        "expected authority guidance: {msg}"
    );

    let mismatch = edit(
        base(),
        SKILL_MD,
        "Use list",
        "---\nname: other\n---\nUse list",
    );
    assert_eq!(crate::support::codes(&mismatch), ["skill-name-mismatch"]);

    let unclosed = edit(base(), SKILL_MD, "Use list", "---\nname: agenda\nUse list");
    assert_eq!(crate::support::codes(&unclosed), ["front-matter-unclosed"]);

    let no_body = edit(
        base(),
        SKILL_MD,
        "Use list to view the schedule.\n",
        "---\nname: agenda\ndescription: Schedule lookup\n---\n\n",
    );
    assert_eq!(crate::support::codes(&no_body), ["empty-file"]);

    let long_desc = "€".repeat(1025);
    let long_compat = "★".repeat(501);
    for (label, front) in [
        ("unknown key", "extra: 1".to_owned()),
        ("uppercase name", "name: Agenda".to_owned()),
        ("leading hyphen name", "name: -agenda".to_owned()),
        ("double hyphen name", "name: a--b".to_owned()),
        ("empty description", "description: \"\"".to_owned()),
        ("blank description", "description: \"   \"".to_owned()),
        ("1025-char description", format!("description: {long_desc}")),
        ("blank license", "license: \"  \"".to_owned()),
        ("blank compatibility", "compatibility: \"  \"".to_owned()),
        (
            "501-char compatibility",
            format!("compatibility: {long_compat}"),
        ),
        ("non-string metadata", "metadata:\n  version: 1".to_owned()),
    ] {
        let files = edit(
            base(),
            SKILL_MD,
            "Use list",
            &format!("---\n{front}\n---\nUse list"),
        );
        let codes = crate::support::codes(&files);
        assert!(
            !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
            "{label}: {codes:?}"
        );
    }
}

#[test]
fn skill_body_size_limit_is_enforced_per_agent_excluding_front_matter() {
    let limit = bot_profile::SKILL_MAX_BYTES;
    assert_eq!(limit, 64 * 1024);
    // Exactly 64 KiB body passes even with front matter attached.
    let exact = format!(
        "---\nname: agenda\ndescription: {}\n---\n{}\n",
        "note".repeat(200),
        "a".repeat(limit)
    );
    let exact_files = edit(base(), SKILL_MD, "Use list to view the schedule.\n", &exact);
    let snap = snapshot(&exact_files);
    assert_eq!(
        snap.agents[&agent("main")].skills["agenda"]
            .instructions
            .text
            .len(),
        limit
    );

    let over_one = edit(
        base(),
        SKILL_MD,
        "Use list to view the schedule.\n",
        &"a".repeat(limit + 1),
    );
    assert_eq!(crate::support::codes(&over_one), ["skill-too-large"]);

    // Combined inherited skill bodies exceeding 64 KiB fail on that agent while siblings under the cap pass.
    let mut inherited = family();
    inherited = edit(
        inherited,
        SKILL_MD,
        "Use list to view the schedule.\n",
        &"a".repeat(40 * 1024),
    );
    inherited = edit(
        inherited,
        "agents/main/agents/research/skills/extra/SKILL.md",
        "",
        &"b".repeat(25 * 1024),
    );
    inherited = edit(
        inherited,
        "agents/main/agents/other/skills/small/SKILL.md",
        "",
        &"c".repeat(20 * 1024),
    );
    let report = crate::support::check(&inherited).unwrap_err();
    assert_eq!(report.codes(), ["skill-too-large"]);
    let err = report.errors().next().unwrap();
    assert_eq!(err.path, "personal/agents/main/agents/research");
    assert!(
        err.message.contains("main/research") && err.message.contains(&limit.to_string()),
        "{}",
        err.message
    );
}
