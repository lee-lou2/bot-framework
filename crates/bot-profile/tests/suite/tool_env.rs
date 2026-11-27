//! Tests for `tools.yaml` `env` and secret environment name validation, inheritance, revision binding, and legacy restore.

use crate::support::{Files, TOOLS_PATH, agent, base, check, edit, ok, raw_bundle, snapshot};
use bot_core::{
    id::{ProfileId, ToolId},
    profile::{Snapshot, Tool},
};
use bot_profile::{Limits, from_bundle, revision_of};
use std::collections::BTreeSet;

const CHILD_ENV: &str = "agents/main/agents/research/env.yaml";

fn with_env(list: &str) -> Files {
    edit(
        base(),
        TOOLS_PATH,
        "egress: [\"calendar.example:443\"]\n",
        &format!("egress: [\"calendar.example:443\"]\n    env: {list}\n"),
    )
}

fn list_tool<'a>(snap: &'a Snapshot, agent_path: &str) -> &'a Tool {
    &snap.agents[&agent(agent_path)].skills["agenda"].tools[&ToolId("agenda.list".into())]
}

fn report_of(files: &Files) -> String {
    format!("{}", check(files).expect_err("expected failure"))
}

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn chosen_names_reach_the_tool_and_its_revision_and_empty_changes_nothing() {
    let plain = snapshot(&base());
    let chosen = snapshot(&with_env("[AGENDA_VIEW]"));
    assert!(list_tool(&plain, "main").env.is_empty());
    assert_eq!(list_tool(&chosen, "main").env, names(&["AGENDA_VIEW"]));
    assert_eq!(
        list_tool(&chosen, "main/research").env,
        names(&["AGENDA_VIEW"]),
        "child receives the same tool using inherited env"
    );
    assert_ne!(
        list_tool(&chosen, "main").revision,
        list_tool(&plain, "main").revision,
        "selected env names bind into tool revision"
    );
    assert_eq!(
        list_tool(&snapshot(&with_env("[]")), "main").revision,
        list_tool(&plain, "main").revision,
        "empty env list keeps the legacy tool revision"
    );
    let delete = |s: &Snapshot| {
        s.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.delete".into())]
            .revision
            .clone()
    };
    assert_eq!(delete(&chosen), delete(&plain));
}

#[test]
fn every_calling_agent_must_have_the_name_after_inheritance() {
    let none = report_of(&with_env("[NOPE_KEY]"));
    assert!(none.contains("env-missing"), "{none}");
    assert!(
        none.contains("main") && none.contains("main/research"),
        "{none}"
    );

    let child_only = edit(with_env("[ONLY_CHILD]"), CHILD_ENV, "", "ONLY_CHILD: x\n");
    let found = report_of(&child_only);
    assert!(found.contains("env-missing"), "{found}");
    assert!(
        found.contains("env.yaml") && found.contains("main") && !found.contains("main/research"),
        "only parent should be reported as missing: {found}"
    );

    ok(&with_env("[AGENDA_VIEW]"));
    ok(&edit(
        with_env("[AGENDA_VIEW]"),
        CHILD_ENV,
        "",
        "AGENDA_VIEW: week\n",
    ));
}

#[test]
fn rejects_reserved_loader_shell_proxy_and_host_names() {
    let with_name = |name: &str| {
        edit(
            with_env(&format!("[{name}]")),
            "agents/main/env.yaml",
            "AGENDA_VIEW: today\n",
            &format!("AGENDA_VIEW: today\n{name}: x\n"),
        )
    };
    for name in [
        "HOME",
        "TMPDIR",
        "NODE_OPTIONS",
        "GIT_DIR",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "SRT_DEBUG",
        "BASH_ENV",
    ] {
        let found = report_of(&with_name(name));
        assert!(found.contains("env-reserved"), "{name}\n{found}");
        assert!(!found.contains("env-missing"), "{name}\n{found}");
    }
    for name in ["PATH", "LD_PRELOAD", "BOT_TOKEN"] {
        let found = report_of(&with_name(name));
        assert!(found.contains("reserved-env"), "{name}\n{found}");
    }
}

#[test]
fn rejects_a_name_that_is_also_a_secret_environment_name() {
    let files = edit(
        with_env("[CALENDAR_TOKEN]"),
        "agents/main/env.yaml",
        "AGENDA_VIEW: today\n",
        "AGENDA_VIEW: today\nCALENDAR_TOKEN: plain\n",
    );
    let found = report_of(&files);
    assert!(found.contains("env-secret-conflict"), "{found}");
    assert!(!found.contains("env-missing"), "{found}");
}

#[test]
fn schema_rejects_duplicates_bad_names_and_wrong_shapes() {
    for list in [
        "[AGENDA_VIEW, AGENDA_VIEW]",
        "[agenda_view]",
        "[\"A-B\"]",
        "AGENDA_VIEW",
        "[1]",
    ] {
        let found = report_of(&with_env(list));
        assert!(found.contains("schema"), "{list}\n{found}");
    }
}

#[test]
fn restored_profiles_are_checked_the_same_way() {
    let (id, limits) = (ProfileId("personal".into()), Limits::default());
    let restore = |files: &Files| {
        let bundle = raw_bundle(files);
        from_bundle(&bundle, &id, &revision_of(&bundle), &limits)
    };
    assert!(restore(&with_env("[AGENDA_VIEW]")).is_ok());
    let found = restore(&with_env("[NOPE_KEY]")).unwrap_err();
    assert!(found.iter().any(|i| i.code == "env-missing"), "{found:?}");
}

#[test]
fn rejects_reserved_and_overlong_secret_env_names() {
    for name in [
        "PATH",
        "LD_PRELOAD",
        "NODE_OPTIONS",
        "HTTPS_PROXY",
        "BOT_EFFECT_URL",
    ] {
        let files = edit(
            base(),
            TOOLS_PATH,
            "CALENDAR_TOKEN: calendar-reader",
            &format!("{name}: calendar-reader"),
        );
        let found = report_of(&files);
        assert!(found.contains("env-reserved"), "{name}\n{found}");
    }
    let long_name = "A".repeat(129);
    let files = edit(
        base(),
        TOOLS_PATH,
        "CALENDAR_TOKEN: calendar-reader",
        &format!("{long_name}: calendar-reader"),
    );
    let found = report_of(&files);
    assert!(found.contains("schema"), "{found}");
}

#[test]
fn legacy_v1_bundle_with_reserved_or_overlong_secret_env_name_still_restores() {
    let (id, limits) = (ProfileId("personal".into()), Limits::default());
    let long_name = "A".repeat(129);
    for env_name in ["PATH", "NODE_OPTIONS", "BOT_EFFECT_URL", long_name.as_str()] {
        let files = edit(
            base(),
            TOOLS_PATH,
            "CALENDAR_TOKEN: calendar-reader",
            &format!("{env_name}: calendar-reader"),
        );
        assert!(
            check(&files).is_err(),
            "{env_name} must be rejected in Strict mode"
        );
        // Stored BOTPROFILE1 bundles must still restore; the broker blocks forbidden env names at execution time.
        let bundle = raw_bundle(&files);
        assert!(bundle.starts_with(b"BOTPROFILE1\n"));
        let restored = from_bundle(&bundle, &id, &revision_of(&bundle), &limits)
            .unwrap_or_else(|e| panic!("legacy BOTPROFILE1 restore failed ({env_name}): {e:?}"));
        assert!(
            list_tool(&restored, "main").secrets.contains_key(env_name),
            "{env_name}"
        );
    }
}
