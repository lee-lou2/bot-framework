//! Acceptance tests for the `examples/profiles/team` template: delegation tree, scope caps, built-ins, env inheritance, triggers, and tool revision stability.

use bot_core::{
    id::{AgentId, ProfileId, ToolId},
    profile::{Adapter, Builtin, TriggerSource},
    state::{Risk, Visibility},
};
use bot_profile::{HostCatalog, Limits, load};
use std::{fs, path::PathBuf};

fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

fn agent(s: &str) -> AgentId {
    AgentId(s.to_owned())
}

fn team() -> ProfileId {
    ProfileId("team".into())
}

fn names<'a>(items: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
    items.into_iter().map(String::as_str).collect()
}

fn read_team_doc(rel: &str) -> (Option<String>, String) {
    let text = fs::read_to_string(examples_root().join("team").join(rel)).unwrap();
    let mut desc = None;
    let body = if let Some(rest) = text.strip_prefix("---\n")
        && let Some((fm, tail)) = rest.split_once("\n---\n")
    {
        for line in fm.lines() {
            if let Some(val) = line.strip_prefix("description:") {
                desc = Some(val.trim().to_owned());
            }
        }
        tail
    } else {
        text.as_str()
    };
    let first_line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_owned();
    (desc, first_line)
}

fn summary_counts(line: &str, rev_prefix: &str) -> Vec<String> {
    let tail = line
        .split_once(rev_prefix)
        .unwrap_or_else(|| panic!("missing revision {rev_prefix} in: {line}"))
        .1;
    tail.split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn validates_team_example() {
    let set = load(&examples_root(), &HostCatalog::demo(), &Limits::default())
        .unwrap_or_else(|r| panic!("example profile failed validation:\n{r}"));
    assert!(set.warnings().is_empty(), "{:?}", set.warnings());
    let snap = set.profile(&team()).unwrap();
    let ids: Vec<&str> = snap.agents.keys().map(|a| a.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "main",
            "main/designer",
            "main/developer",
            "main/ops",
            "main/pm"
        ]
    );
    assert_eq!(snap.root, agent("main"));
    let children: Vec<&str> = snap
        .agents
        .iter()
        .filter(|(_, a)| a.parent == Some(agent("main")))
        .map(|(id, _)| id.0.as_str())
        .collect();
    assert_eq!(
        children,
        ["main/designer", "main/developer", "main/ops", "main/pm"]
    );
    let main = &snap.agents[&agent("main")];
    let developer = &snap.agents[&agent("main/developer")];
    let designer = &snap.agents[&agent("main/designer")];
    let pm = &snap.agents[&agent("main/pm")];
    let ops = &snap.agents[&agent("main/ops")];
    let (dev_desc, _) = read_team_doc("agents/main/agents/developer/AGENT.md");
    let (ops_desc, ops_body) = read_team_doc("agents/main/agents/ops/AGENT.md");
    assert_eq!(
        Some(developer.description.as_str()),
        dev_desc.as_deref(),
        "developer description from front matter"
    );
    assert_eq!(
        Some(ops.description.as_str()),
        ops_desc.as_deref(),
        "ops description from front matter"
    );
    assert_eq!(
        developer.builtin.iter().collect::<Vec<_>>(),
        [&Builtin::Read]
    );
    for other in [main, designer, pm, ops] {
        assert!(other.builtin.is_empty());
    }
    assert_eq!(names(&developer.scopes), ["run"]);
    assert_eq!(names(&designer.scopes), ["run"]);
    assert_eq!(
        names(&pm.scopes),
        [
            "calendar.read",
            "run",
            "schedule.cancel",
            "schedule.create",
            "schedule.list"
        ]
    );
    assert_eq!(
        names(&ops.scopes),
        ["calendar.delete", "calendar.read", "run"]
    );
    let agenda = &main.skills["agenda"];
    assert_eq!(agenda.description, None, "skill without front matter");
    let (report_desc, report_head) = read_team_doc("skills/report-format/SKILL.md");
    let report = &main.skills["report-format"];
    assert_eq!(report.description.as_deref(), report_desc.as_deref());
    assert!(
        report.instructions.text.starts_with(&report_head)
            && !report.instructions.text.contains("---"),
        "front matter must be stripped from skill body: {}",
        report.instructions.text
    );
    let (delegation_desc, delegation_head) =
        read_team_doc("agents/main/skills/delegation/SKILL.md");
    let delegation = &main.skills["delegation"];
    assert_eq!(
        delegation.description.as_deref(),
        delegation_desc.as_deref()
    );
    assert!(
        delegation.instructions.text.starts_with(&delegation_head)
            && !delegation.instructions.text.contains("name: delegation")
            && !delegation.instructions.text.contains("description:"),
        "{}",
        delegation.instructions.text
    );
    let list = &agenda.tools[&ToolId("agenda.list".into())];
    let delete = &agenda.tools[&ToolId("agenda.delete".into())];
    assert_eq!((list.class, delete.class), (Risk::Read, Risk::Irreversible));
    assert_eq!(list.server, "demo-agenda");
    assert!(list.env.contains("TEAM_TIMEZONE"));
    assert!(ops.skills.contains_key("agenda") && ops.skills.contains_key("incident-runbook"));
    assert_eq!(
        names(developer.skills.keys()),
        ["agenda", "code-review", "delegation", "report-format"]
    );
    assert!(
        !designer.skills.contains_key("code-review"),
        "sibling skills must not be visible"
    );
    let (_, rules_head) = read_team_doc("rules.md");
    let (_, main_head) = read_team_doc("agents/main/AGENT.md");
    let at = |needle: &str| {
        ops.instructions
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}"))
    };
    assert!(
        at(&rules_head) < at(&main_head) && at(&main_head) < at(&ops_body),
        "assembly order must be rules -> parent -> self"
    );
    assert_eq!(main.env["TEAM_TIMEZONE"], "Asia/Seoul");
    assert_eq!(pm.env["TEAM_TIMEZONE"], "Asia/Seoul");
    assert_eq!(ops.env["TEAM_TIMEZONE"], "UTC");
    assert_eq!(ops.env["TEAM_NAME"], "example-team");
    assert_eq!(main.triggers.len(), 1);
    let standup = &main.triggers[0];
    assert_eq!(standup.id, "standup");
    match &standup.on {
        TriggerSource::Cron {
            schedule,
            principal,
            approver,
            ..
        } => {
            assert_eq!(
                (schedule.as_str(), principal.0.as_str()),
                ("0 9 * * 1-5", "service:standup")
            );
            assert!(approver.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(main.memory_seeds.len(), 1);
    assert_eq!(main.memory_seeds[0].visibility, Visibility::Profile);
    assert_eq!(pm.memory_seeds.len(), 1);
    assert_eq!(pm.memory_seeds[0].visibility, Visibility::Private);
    assert!(developer.memory_seeds.is_empty() && ops.memory_seeds.is_empty());
    assert_eq!(main.knowledge.len(), 3);
    assert_eq!(developer.knowledge.len(), 4);
    assert_eq!(ops.knowledge.len(), 4);
    assert_eq!(snap.bindings.len(), 1);
    assert!(
        matches!(&snap.bindings.values().next().unwrap().source, Adapter::Cli { account } if account == "team")
    );
    let shown = set.render();
    let team_line = shown
        .lines()
        .find(|line| line.starts_with("team "))
        .unwrap_or_else(|| panic!("missing team summary line: {shown}"));
    assert_eq!(
        summary_counts(team_line, &snap.revision.0[..12]),
        ["5", "2", "1", "1"],
        "{shown}"
    );
}

#[test]
fn shipped_tool_revisions_are_stable() {
    let set = load(&examples_root(), &HostCatalog::demo(), &Limits::default()).unwrap();
    for (name, list_revision) in [
        (
            "personal",
            "b8161c32df73d2c13b356042688593009a5075ed4d2cf15c9b1ca16d7ffe05d4",
        ),
        (
            "team",
            "9386a6b475a2f5dfe42318e8850a142733b2e117f4fc31c096fde6bb120697e0",
        ),
    ] {
        let snap = set.profile(&ProfileId(name.into())).unwrap();
        let tools = &snap.agents[&agent("main")].skills["agenda"].tools;
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[&ToolId("agenda.list".into())].revision.0,
            list_revision
        );
        assert_eq!(
            tools[&ToolId("agenda.delete".into())].revision.0,
            "aad70ef628eafcd9e4131bf7d35deb3d151c1661d1b035ad7e88be11e484df7f"
        );
    }
}
