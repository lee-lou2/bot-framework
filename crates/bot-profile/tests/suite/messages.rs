//! Actionable validation message tests (M1-M7): verify fix hints, allowed values, examples, and no raw regex/parser internals.

use crate::support::{TOOLS_PATH, Temp, base, check, edit, host, write_profile};
use bot_profile::{Issue, Limits, load};

fn errors(files: &crate::support::Files) -> Vec<Issue> {
    match check(files) {
        Ok(_) => panic!("expected validation to fail"),
        Err(report) => report.errors().cloned().collect(),
    }
}

fn only(files: &crate::support::Files) -> Issue {
    let mut found = errors(files);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.remove(0)
}

#[test]
fn m1_a_wrong_adapter_lists_every_allowed_value_and_the_closest_one() {
    let issue = only(&edit(base(), "profile.yaml", "adapter: cli", "adapter: cl"));
    assert_eq!(
        (issue.code, issue.path.as_str(), issue.at.as_str()),
        ("schema", "personal/profile.yaml", "/bindings/0/adapter")
    );
    assert!(
        issue
            .message
            .contains(r#""cli", "slack", "telegram", "http""#),
        "{}",
        issue.message
    );
    assert!(
        issue.message.contains(r#""cl""#) && issue.message.matches(r#""cli""#).count() >= 2,
        "{}",
        issue.message
    );
    assert!(!issue.message.contains(r#"must be "cli""#));
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "workspace: home, channel: planning,",
        "workspace: home,",
    ));
    assert_eq!(issue.at, "/bindings/2");
    assert!(
        issue.message.to_lowercase().contains("required") && issue.message.contains("`channel`"),
        "{}",
        issue.message
    );
}

#[test]
fn m2_a_model_alias_the_host_does_not_allow_names_the_allowed_ones_and_where_to_add() {
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "model: assistant",
        "model: assistent",
    ));
    assert_eq!(
        (issue.code, issue.path.as_str(), issue.at.as_str()),
        ("model-not-allowed", "personal/profile.yaml", "/model")
    );
    let text = &issue.message;
    assert!(
        text.contains("`assistent`") && text.contains("`personal`"),
        "{text}"
    );
    assert!(text.contains("assistant, fast"), "{text}");
    assert!(text.contains("`assistant`"), "{text}");
    assert!(
        text.contains("BOT_HOST_CONFIG") && text.contains("`models.personal`"),
        "{text}"
    );
    // An agent overriding `model` in its own AGENT.md is reported on that file rather than inherited descendants.
    let issue = only(&edit(
        base(),
        "agents/main/agents/research/AGENT.md",
        "Handle",
        "---\nmodel: fastt\n---\nHandle",
    ));
    assert_eq!(
        (issue.code, issue.path.as_str()),
        (
            "model-not-allowed",
            "personal/agents/main/agents/research/AGENT.md"
        )
    );
    assert!(issue.message.contains("`fast`"), "{}", issue.message);
    let root = Temp::new("models");
    write_profile(root.path(), "personal", &base());
    let mut empty = host();
    empty.models.clear();
    let report = load(root.path(), &empty, &Limits::default()).unwrap_err();
    let issue = report.errors().next().unwrap();
    assert!(
        issue
            .message
            .contains(r#""models": {"personal": ["assistant"]}"#),
        "{}",
        issue.message
    );
}

#[test]
fn m3_name_format_errors_say_what_is_allowed_with_an_example_instead_of_a_regex() {
    let issue = only(&edit(base(), TOOLS_PATH, "id: list", "id: List"));
    assert_eq!(issue.at, "/tools/0/id");
    assert!(
        issue.message.to_lowercase().contains("lowercase")
            && issue.message.contains("`daily-report`"),
        "{}",
        issue.message
    );
    let issue = only(&edit(
        base(),
        TOOLS_PATH,
        "secrets: {CALENDAR_TOKEN: calendar-reader}",
        "secrets: {calendar_token: calendar-reader}",
    ));
    assert!(
        issue.message.to_lowercase().contains("uppercase")
            && issue.message.contains("`CALENDAR_TOKEN`"),
        "{}",
        issue.message
    );
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "agent: main}\n  - {id: inbox",
        "agent: Main}\n  - {id: inbox",
    ));
    assert_eq!(issue.at, "/bindings/0/agent");
    assert!(
        issue.message.contains("`main/research`"),
        "{}",
        issue.message
    );
    let text = &issue.message;
    assert!(
        !text.contains("(?:") && !text.contains("^[") && !text.contains("(pattern "),
        "{text}"
    );
}

#[test]
fn m4_a_misspelled_field_is_one_line_naming_the_field_it_should_be() {
    let issue = only(&edit(base(), "profile.yaml", "bindings:", "bindngs:"));
    assert_eq!(issue.at, "/bindngs");
    assert!(
        issue.message.contains("`bindngs`") && issue.message.contains("`bindings`"),
        "{}",
        issue.message
    );
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "secrts: [calendar-reader]",
    ));
    assert!(
        issue.message.contains("`secrts`") && issue.message.contains("`secrets`"),
        "{}",
        issue.message
    );
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "secrets: [calendar-reader]\nextra: 1",
    ));
    assert!(
        issue.message.contains("`extra`") && issue.message.contains("bindings"),
        "{}",
        issue.message
    );
}

#[test]
fn m5_an_entrance_used_twice_says_how_to_separate_the_two_bindings() {
    let twice = edit(
        base(),
        "profile.yaml",
        "  - {id: inbox, adapter: http",
        "  - {id: desk2, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http",
    );
    let issue = only(&twice);
    assert_eq!(issue.code, "duplicate-entrance");
    assert!(
        issue.message.contains("`desk`")
            && issue.message.contains("`desk2`")
            && issue.message.contains("`account`"),
        "{}",
        issue.message
    );
    let root = Temp::new("m5");
    write_profile(root.path(), "personal", &base());
    let second = vec![
        (
            "profile.yaml".to_owned(),
            "version: 1\nid: second\nmodel: assistant\nroles: {owner: [run]}\nmembers: [{identity: \"local:owner\", roles: [owner]}]\nbindings:\n  - {id: desk, adapter: cli, account: owner, agent: main}\n".to_owned(),
        ),
        ("agents/main/AGENT.md".to_owned(), "Second.\n".to_owned()),
    ];
    write_profile(root.path(), "second", &second);
    let report = load(root.path(), &host(), &Limits::default()).unwrap_err();
    let issue = report.errors().next().unwrap();
    assert_eq!(issue.code, "duplicate-entrance");
    let text = &issue.message;
    assert!(text.contains("personal") && text.contains("desk"), "{text}");
    assert!(text.contains("`account`"), "{text}");
    assert!(text.contains("`gateway.cli.accounts`"), "{text}");
    assert!(text.contains("bot chat --profile"), "{text}");
    for (key, field) in [
        ("slack:home:channel:planning", "`channel`"),
        ("slack:home:dm", "`workspace`"),
        ("http:reminders", "`endpoint`"),
    ] {
        let fix = bot_profile_hint_for(key);
        assert!(fix.contains(field), "{key}: {fix}");
    }
}

fn bot_profile_hint_for(key: &str) -> String {
    let (adapter, edit_to) = match key {
        "slack:home:channel:planning" => (
            "  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}",
            "  - {id: team2, adapter: slack, workspace: home, channel: planning, agent: main}\n  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}",
        ),
        "slack:home:dm" => (
            "  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}",
            "  - {id: direct2, adapter: slack, workspace: home, dm: true, agent: main}\n  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}",
        ),
        _ => (
            "  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
            "  - {id: inbox2, adapter: http, endpoint: reminders, agent: main}\n  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
        ),
    };
    only(&edit(base(), "profile.yaml", adapter, edit_to)).message
}

#[test]
fn m7_yaml_syntax_errors_say_how_to_fix_it_in_korean_with_line_and_column() {
    let cases = [
        ("model: assistant: x", "indent", ":"),
        ("model:\tassistant", "space", "tab"),
        ("secrets: [calendar-reader", "[", "quote"),
    ];
    for (broken, first, second) in cases {
        let from = if broken.starts_with("secrets") {
            "secrets: [calendar-reader]"
        } else {
            "model: assistant"
        };
        let issue = only(&edit(base(), "profile.yaml", from, broken));
        assert_eq!(issue.code, "yaml-syntax", "{broken}");
        let at_lower = issue.at.to_lowercase();
        assert!(
            at_lower.contains("line") && at_lower.contains("col"),
            "{broken}: {}",
            issue.at
        );
        let msg_lower = issue.message.to_lowercase();
        assert!(
            msg_lower.contains(first) || msg_lower.contains(second),
            "{broken}: {}",
            issue.message
        );
        // Raw parser error strings must not leak to users.
        for raw in [
            "not allowed in this context",
            "must be followed by",
            "did not find",
            "illegal placement",
        ] {
            assert!(!issue.message.contains(raw), "{broken}: {}", issue.message);
        }
    }
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "roles:\n  owner:",
        "roles:\n   owner:\n  bad",
    ));
    assert!(
        issue.message.to_lowercase().contains("indent"),
        "{}",
        issue.message
    );
}
