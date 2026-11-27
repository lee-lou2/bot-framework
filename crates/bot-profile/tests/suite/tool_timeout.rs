//! Tests for `tools.yaml` `timeout_secs`: schema bounds, tool revision binding, and host ceiling (`tool-timeout-over-cap`).

use crate::support::{Files, TOOLS_PATH, Temp, agent, base, check, edit, host, write_profile};
use bot_core::{
    id::ToolId,
    profile::{Snapshot, Tool},
};
use bot_profile::{Limits, ProfileSet, Report, load};

fn with_timeout(value: &str) -> Files {
    edit(
        base(),
        TOOLS_PATH,
        "egress: [\"calendar.example:443\"]\n",
        &format!("egress: [\"calendar.example:443\"]\n    timeout_secs: {value}\n"),
    )
}

fn list_tool(snap: &Snapshot) -> &Tool {
    &snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.list".into())]
}

fn load_with(files: &Files, ceiling: Option<u32>) -> Result<ProfileSet, Report> {
    let root = Temp::new("timeout");
    write_profile(root.path(), "personal", files);
    let mut host = host();
    host.tool_call_max_secs = ceiling;
    load(root.path(), &host, &Limits::default())
}

fn codes(files: &Files) -> Vec<&'static str> {
    match load_with(files, Some(900)) {
        Ok(_) => Vec::new(),
        Err(report) => report.codes(),
    }
}

fn snapshot(files: &Files) -> Snapshot {
    let set = load_with(files, Some(900)).unwrap_or_else(|report| panic!("{report}"));
    (**set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap())
    .clone()
}

fn over_cap(files: &Files, ceiling: Option<u32>) -> bool {
    match load_with(files, ceiling) {
        Ok(_) => false,
        Err(report) => {
            assert!(report.has("tool-timeout-over-cap"), "{report}");
            true
        }
    }
}

#[test]
fn only_whole_seconds_from_one_to_nine_hundred_are_accepted_by_the_schema() {
    for ok in ["1", "60", "300", "900"] {
        assert!(
            codes(&with_timeout(ok)).is_empty(),
            "{ok}: {:?}",
            codes(&with_timeout(ok))
        );
    }
    for bad in ["0", "901", "-1", "1.5", "\"30\"", "true", "null", "[60]"] {
        assert_eq!(codes(&with_timeout(bad)), ["schema"], "{bad}");
    }
}

#[test]
fn a_declared_limit_reaches_the_tool_and_binds_into_its_revision_only_when_declared() {
    let plain = snapshot(&base());
    assert_eq!(list_tool(&plain).timeout_secs, None);
    assert_eq!(list_tool(&plain).call_secs(), 60, "default is 60 seconds");
    let long = snapshot(&with_timeout("600"));
    assert_eq!(list_tool(&long).timeout_secs, Some(600));
    assert_eq!(list_tool(&long).call_secs(), 600);
    // Declaring `timeout_secs` binds into tool revision so prior approvals cannot apply to a new timeout.
    assert_ne!(list_tool(&long).revision, list_tool(&plain).revision);
    assert_ne!(
        list_tool(&long).revision,
        list_tool(&snapshot(&with_timeout("601"))).revision
    );
    assert_eq!(
        list_tool(&long).revision,
        list_tool(&snapshot(&with_timeout("600"))).revision
    );
    let other = |snap: &Snapshot| {
        snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.delete".into())]
            .revision
            .clone()
    };
    assert_eq!(other(&long), other(&plain));
}

#[test]
fn a_declared_limit_above_the_host_ceiling_refuses_the_activation_and_names_the_tool() {
    assert!(!over_cap(&with_timeout("60"), None));
    assert!(over_cap(&with_timeout("61"), None));
    let report = format!("{}", check(&with_timeout("900")).unwrap_err());
    assert!(report.contains("agenda.list"), "{report}");
    assert!(report.contains("900"), "{report}");
    assert!(report.contains("limits.tool_call_max_secs"), "{report}");
    assert!(!over_cap(&with_timeout("900"), Some(900)));
    assert!(!over_cap(&with_timeout("300"), Some(300)));
    assert!(over_cap(&with_timeout("301"), Some(300)));
}

#[test]
fn an_undeclared_tool_counts_as_sixty_seconds_so_a_lower_ceiling_refuses_it_and_a_higher_one_does_not_change_it()
 {
    for ceiling in [None, Some(60), Some(300), Some(900)] {
        let set = load_with(&base(), ceiling).unwrap_or_else(|r| panic!("{ceiling:?}: {r}"));
        let snap = set
            .profile(&bot_core::id::ProfileId("personal".into()))
            .unwrap();
        assert_eq!(list_tool(snap).call_secs(), 60, "{ceiling:?}");
    }
    assert!(over_cap(&base(), Some(30)));
    let report = format!("{}", load_with(&base(), Some(30)).unwrap_err());
    assert!(
        report.to_lowercase().contains("default") && report.contains("60"),
        "{report}"
    );
    assert!(report.contains("`timeout_secs`"), "{report}");
    let mut all = base();
    for id in ["list", "delete", "find"] {
        all = edit(
            all,
            TOOLS_PATH,
            &format!("  - id: {id}\n"),
            &format!("  - id: {id}\n    timeout_secs: 30\n"),
        );
    }
    assert!(!over_cap(&all, Some(30)));
}
