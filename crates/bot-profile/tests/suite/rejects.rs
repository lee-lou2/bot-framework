//! Rejection matrix: single-mutation tests verifying expected validation error codes.

use crate::support::{DELETE, Files, base, check, edit, host};

type Edit<'a> = (&'a str, &'a str, &'a str);

/// Declares the reserved built-in ID `memory.share`.
const MEMORY_SHARE: &str = "version: 1\nserver: demo-notes\ntools:\n  - id: share\n    class: write\n    scopes: [calendar.read]\n    input: {type: object, properties: {}, additionalProperties: false}\n";

const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";
const AGENT: &str = "agents/main/AGENT.md";

fn apply(edits: &[Edit]) -> Files {
    edits.iter().fold(base(), |files, (path, from, to)| {
        edit(files, path, from, to)
    })
}

fn expect(name: &str, files: &Files, code: &str) {
    match check(files) {
        Ok(_) => panic!("{name}: expected failure with {code}"),
        Err(report) => assert!(report.has(code), "{name}: expected {code}\n{report}"),
    }
}

#[test]
fn rejects_files_yaml_and_schema_mistakes() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "unsupported version",
            &[("profile.yaml", "version: 1", "version: 2")],
            "schema",
        ),
        (
            "unknown field",
            &[(
                "profile.yaml",
                "secrets: [calendar-reader]",
                "secrets: [calendar-reader]\nextra: 1",
            )],
            "schema",
        ),
        (
            "missing required model",
            &[("profile.yaml", "model: assistant\n", "")],
            "schema",
        ),
        (
            "uppercase model",
            &[("profile.yaml", "model: assistant", "model: Assistant")],
            "schema",
        ),
        (
            "double hyphen id",
            &[("profile.yaml", "{id: desk,", "{id: de--sk,")],
            "schema",
        ),
        (
            "duplicate key",
            &[(
                "profile.yaml",
                "model: assistant",
                "model: assistant\nmodel: fast",
            )],
            "yaml-duplicate-key",
        ),
        (
            "anchor",
            &[("profile.yaml", "model: assistant", "model: &m assistant")],
            "yaml-anchor",
        ),
        (
            "alias",
            &[(
                "profile.yaml",
                "secrets: [calendar-reader]",
                "secrets: &s [calendar-reader]\nextra: *s",
            )],
            "yaml-anchor",
        ),
        (
            "tag",
            &[("profile.yaml", "model: assistant", "model: !!str assistant")],
            "yaml-tag",
        ),
        (
            "multi-document YAML",
            &[("profile.yaml", "version: 1", "version: 1\n---\nversion: 1")],
            "yaml-multi-doc",
        ),
        (
            "syntax error",
            &[("profile.yaml", "roles:", "roles: [")],
            "yaml-syntax",
        ),
        (
            "escaped control character",
            &[(
                "profile.yaml",
                "model: assistant",
                "model: \"assi\\u0000stant\"",
            )],
            "yaml-control",
        ),
        (
            "BOM",
            &[("rules.md", "- Keep", "\u{feff}- Keep")],
            "text-bom",
        ),
        (
            "bidi override character",
            &[(AGENT, "Check", "Ch\u{202e}eck")],
            "text-control",
        ),
        ("missing AGENT.md", &[(AGENT, DELETE, "")], "missing-file"),
        (
            "misspelled file",
            &[("agents/main/trigger.yaml", "", "version: 1\n")],
            "unknown-file",
        ),
        (
            "unknown directory",
            &[("extra/readme.md", "", "x")],
            "unknown-dir",
        ),
        (
            "multiple root agents",
            &[("agents/other/AGENT.md", "", "Second root")],
            "multiple-root-agents",
        ),
        (
            "missing SKILL.md",
            &[("agents/main/skills/agenda/SKILL.md", DELETE, "")],
            "missing-file",
        ),
        (
            "reserved built-in tool memory.share",
            &[
                ("agents/main/skills/memory/SKILL.md", "", "Memory skill"),
                ("agents/main/skills/memory/tools.yaml", "", MEMORY_SHARE),
            ],
            "reserved-tool",
        ),
        (
            "skill name conflict",
            &[("skills/agenda/SKILL.md", "", "Shared agenda skill")],
            "skill-conflict",
        ),
        (
            "forbidden allowed-tools in SKILL.md",
            &[(
                "agents/main/skills/agenda/SKILL.md",
                "Use list",
                "---\nname: agenda\nallowed-tools: Bash\n---\nUse list",
            )],
            "skill-allowed-tools",
        ),
        (
            "SKILL.md name mismatch",
            &[(
                "agents/main/skills/agenda/SKILL.md",
                "Use list",
                "---\nname: other\n---\nUse list",
            )],
            "skill-name-mismatch",
        ),
        (
            "unclosed SKILL.md front matter",
            &[(
                "agents/main/skills/agenda/SKILL.md",
                "Use list",
                "---\nname: agenda\nUse list",
            )],
            "front-matter-unclosed",
        ),
        (
            "empty AGENT.md",
            &[(
                AGENT,
                "Check today's schedule and confirm the exact event before making changes.\n",
                "",
            )],
            "empty-file",
        ),
        (
            "unknown front matter key",
            &[(AGENT, "Check", "---\ntools: x\n---\nCheck")],
            "schema",
        ),
        (
            "unclosed AGENT.md front matter",
            &[(AGENT, "Check", "---\nmodel: fast\nCheck")],
            "front-matter-unclosed",
        ),
        (
            "non-string env value",
            &[("agents/main/env.yaml", "today", "[a]")],
            "schema",
        ),
        (
            "reserved env name",
            &[("agents/main/env.yaml", "AGENDA_VIEW: today", "BOT_X: one")],
            "reserved-env",
        ),
        (
            "disallowed knowledge extension",
            &[("knowledge/data.csv", "", "a,b")],
            "bad-document",
        ),
        (
            "memory seed missing owner",
            &[(
                "agents/main/memory/routine.md",
                "---\nowner: \"local:owner\"\n---\n",
                "",
            )],
            "memory-owner-missing",
        ),
        (
            "memory seed unknown owner",
            &[(
                "agents/main/memory/routine.md",
                "local:owner",
                "local:stranger",
            )],
            "memory-owner-unknown",
        ),
        (
            "invalid memory seed visibility",
            &[(
                "agents/main/memory/routine.md",
                "---\n",
                "---\nvisibility: everyone\n",
            )],
            "schema",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_profile_yaml_references() {
    let owner = "{identity: \"local:owner\", roles: [owner]}";
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "id mismatches folder",
            &[("profile.yaml", "id: personal", "id: other")],
            "profile-id-mismatch",
        ),
        (
            "unknown timezone",
            &[("profile.yaml", "Asia/Seoul", "Mars/Base")],
            "bad-timezone",
        ),
        (
            "timezone case mismatch",
            &[("profile.yaml", "Asia/Seoul", "asia/seoul")],
            "bad-timezone",
        ),
        (
            "unknown role",
            &[(
                "profile.yaml",
                owner,
                "{identity: \"local:owner\", roles: [boss]}",
            )],
            "unknown-role",
        ),
        (
            "duplicate member",
            &[("profile.yaml", owner, &format!("{owner}\n  - {owner}"))],
            "duplicate-member",
        ),
        (
            "malformed identity",
            &[(
                "profile.yaml",
                "identity: \"local:owner\"",
                "identity: \"owner\"",
            )],
            "bad-identity",
        ),
        (
            "binding points to unknown agent",
            &[(
                "profile.yaml",
                "account: owner, agent: main",
                "account: owner, agent: ghost",
            )],
            "unknown-agent",
        ),
        (
            "policy points to unknown agent",
            &[("profile.yaml", "main/research:", "main/ghost:")],
            "unknown-agent",
        ),
        (
            "approve scope typo",
            &[("profile.yaml", "approve: []", "approve: [calendar.delet]")],
            "unknown-scope",
        ),
        (
            "unknown policy scope",
            &[(
                "profile.yaml",
                "  scopes: [run, calendar.read, calendar.delete]",
                "  scopes: [run, calendar.read, calendar.delete, nope.scope]",
            )],
            "unknown-scope",
        ),
        (
            "Slack channel binding missing channel",
            &[(
                "profile.yaml",
                "workspace: home, channel: planning,",
                "workspace: home,",
            )],
            "schema",
        ),
        (
            "duplicate binding id",
            &[("profile.yaml", "{id: inbox,", "{id: desk,")],
            "duplicate-binding-id",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_tool_declarations() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "unregistered server",
            &[(TOOLS, "server: demo-agenda", "server: gone")],
            "missing-server",
        ),
        (
            "invalid class",
            &[(TOOLS, "class: irreversible", "class: dangerous")],
            "schema",
        ),
        (
            "unknown tool field",
            &[(
                TOOLS,
                "egress: [\"calendar.example:443\"]",
                "egress: [\"calendar.example:443\"]\n    bogus: true",
            )],
            "schema",
        ),
        (
            "undeclared secret",
            &[("profile.yaml", "secrets: [calendar-reader]", "secrets: []")],
            "unknown-secret",
        ),
        (
            "port out of range",
            &[(TOOLS, "calendar.example:443", "calendar.example:99999")],
            "bad-egress",
        ),
        (
            "destination path missing from input",
            &[(
                TOOLS,
                "scopes: [calendar.delete]\n",
                "scopes: [calendar.delete]\n    destination: {recipient: args.to, content: args.event}\n",
            )],
            "bad-destination",
        ),
        (
            "unsupported input keyword",
            &[(
                TOOLS,
                "{type: string, minLength: 1}",
                "{type: string, uniqueItems: true}",
            )],
            "schema",
        ),
        (
            "input missing type: object",
            &[(
                TOOLS,
                "input: {type: object, properties: {}, additionalProperties: false}",
                "input: {properties: {}}",
            )],
            "bad-input-schema",
        ),
        (
            "duplicate tool id",
            &[(TOOLS, "  - id: find", "  - id: list")],
            "duplicate-tool",
        ),
        (
            "empty scopes",
            &[(
                TOOLS,
                "scopes: [calendar.read]\n    secrets",
                "scopes: []\n    secrets",
            )],
            "schema",
        ),
        (
            "destination on read tool",
            &[(
                TOOLS,
                "class: read\n    scopes: [calendar.read]\n    secrets",
                "class: read\n    destination: reply\n    scopes: [calendar.read]\n    secrets",
            )],
            "destination-on-read",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
}

#[test]
fn rejects_aliases_the_host_did_not_register() {
    let cases: &[(&str, &[Edit], &str)] = &[
        (
            "disallowed profile model",
            &[("profile.yaml", "model: assistant", "model: huge")],
            "model-not-allowed",
        ),
        (
            "disallowed agent model",
            &[(AGENT, "Check", "---\nmodel: huge\n---\nCheck")],
            "model-not-allowed",
        ),
        (
            "unknown CLI account",
            &[("profile.yaml", "account: owner,", "account: ghost,")],
            "unknown-account",
        ),
        (
            "unknown Slack workspace",
            &[(
                "profile.yaml",
                "workspace: home, channel",
                "workspace: away, channel",
            )],
            "unknown-workspace",
        ),
        (
            "unknown Slack channel",
            &[("profile.yaml", "channel: planning", "channel: nowhere")],
            "unknown-channel",
        ),
        (
            "unknown HTTP endpoint",
            &[("profile.yaml", "endpoint: reminders", "endpoint: ghost")],
            "unknown-endpoint",
        ),
    ];
    for (name, edits, code) in cases {
        expect(name, &apply(edits), code);
    }
    let mut closed = host();
    closed.slack.get_mut("home").unwrap().dm = false;
    let dir = crate::support::Temp::new("dm");
    crate::support::write_profile(dir.path(), "personal", &base());
    let report =
        bot_profile::load(dir.path(), &closed, &bot_profile::Limits::default()).unwrap_err();
    assert!(report.has("dm-not-allowed"), "{report}");
}
