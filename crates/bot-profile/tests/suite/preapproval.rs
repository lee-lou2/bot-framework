//! Declaration validation tests for trigger `preapproved` grants and `human_approval` rules.

use crate::support::{Files, append, base, check, edit, ok};

const TRIGGERS: &str = "agents/main/triggers.yaml";
const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";
const PROFILE: &str = "profile.yaml";

fn with_notify(effects: bool) -> Files {
    append(
        base(),
        TOOLS,
        &format!(
            "  - id: notify\n    class: irreversible\n    effects: {effects}\n    scopes: [calendar.read]\n    input: {{type: object, required: [report], properties: {{report: {{type: string}}}}, additionalProperties: false}}\n"
        ),
    )
}

const PREAPPROVED: &str = "    preapproved:\n      max_per_day: 24\n      targets:\n        - {operation: post, target: \"channel:reports\"}\n        - {operation: post, target: \"channel:alerts\"}\n";

fn cron(extra: &str) -> Files {
    append(
        with_notify(true),
        TRIGGERS,
        &format!(
            "  - id: notify\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.notify, args: {{report: daily}}}}\n{extra}"
        ),
    )
}

fn expect(name: &str, files: &Files, code: &str) {
    match check(files) {
        Ok(_) => panic!("{name}: expected failure with {code}"),
        Err(report) => assert!(report.has(code), "{name}: expected {code}\n{report}"),
    }
}

#[test]
fn a_fixed_cron_trigger_with_a_preapproval_needs_no_human_approver_and_sorts_its_pairs() {
    expect(
        "approver required without preapproved",
        &cron(""),
        "approver-required",
    );
    let files = cron(PREAPPROVED);
    let set = ok(&files);
    let snapshot = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    let main = &snapshot.agents[&crate::support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "notify").unwrap();
    let pre = trigger.preapproved.as_ref().expect("preapproved parsed");
    assert_eq!(pre.max_per_day, 24);
    assert_eq!(
        pre.targets,
        vec![
            ("post".to_owned(), "channel:alerts".to_owned()),
            ("post".to_owned(), "channel:reports".to_owned()),
        ],
        "sorted pairs"
    );
    assert!(trigger.human_approval.is_none());
    let other = cron(&PREAPPROVED.replace("24", "25"));
    assert_ne!(snapshot.revision, crate::support::snapshot(&other).revision);
}

#[test]
fn rejects_malformed_or_unenforceable_preapprovals_with_the_fixed_reasons() {
    let pre = |body: &str| format!("    preapproved:\n{body}");
    let targets = "      targets: [{operation: post, target: \"channel:reports\"}]\n";
    let mut cases: Vec<(&str, Files, &str)> = vec![
        (
            "max_per_day 0",
            cron(&pre(&format!("      max_per_day: 0\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "max_per_day over ceiling",
            cron(&pre(&format!("      max_per_day: 65536\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "string max_per_day",
            cron(&pre(&format!("      max_per_day: \"3\"\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "empty targets",
            cron(&pre("      max_per_day: 3\n      targets: []\n")),
            "invalid_preapproval",
        ),
        (
            "wildcard target",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: \"channel:*\"}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "URL target",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: \"https://hooks.example.com/x\"}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "duplicate pair",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: a}, {operation: post, target: a}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "unknown key",
            cron(&pre(&format!(
                "      max_per_day: 3\n{targets}      note: x\n"
            ))),
            "invalid_preapproval",
        ),
        (
            "unknown pair key",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: a, params: x}]\n",
            )),
            "invalid_preapproval",
        ),
        ("non-object", cron("    preapproved: yes\n"), "schema"),
    ];
    cases.push((
        "effects false",
        append(
            with_notify(false),
            TRIGGERS,
            &format!("  - id: notify\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.notify, args: {{report: daily}}}}\n{PREAPPROVED}"),
        ),
        "preapproval_unenforceable",
    ));
    cases.push((
        "read tool",
        append(
            base(),
            TRIGGERS,
            &format!("  - id: look\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.list, args: {{}}}}\n{PREAPPROVED}"),
        ),
        "preapproval_unenforceable",
    ));
    cases.push((
        "instruction target",
        append(
            base(),
            TRIGGERS,
            &format!("  - id: ask\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{instruction: notify}}\n{PREAPPROVED}"),
        ),
        "invalid_preapproval",
    ));
    cases.push((
        "event source",
        append(
            with_notify(true),
            TRIGGERS,
            &format!("  - id: byhand\n    \"on\": {{event: message, binding: team}}\n    input: {{report: body.text}}\n    target: {{tool: agenda.notify}}\n{PREAPPROVED}"),
        ),
        "invalid_preapproval",
    ));
    cases.push((
        "beat",
        cron(&format!("    beat: true\n{PREAPPROVED}")),
        "invalid_preapproval",
    ));
    for (name, files, code) in &cases {
        expect(name, files, code);
    }
}

fn with_approvers(members: &str, approvers: &str, extra_trigger: &str) -> Files {
    let files = edit(
        with_notify(true),
        PROFILE,
        "  - {identity: \"service:morning\", roles: [reader]}\n",
        &format!("  - {{identity: \"service:morning\", roles: [reader]}}\n{members}"),
    );
    append(
        files,
        TRIGGERS,
        &format!(
            "  - id: confirm\n    \"on\": {{event: message, binding: team}}\n    match: {{body.text: approve}}\n    input: {{report: body.text}}\n    target: {{tool: agenda.notify}}\n    human_approval:\n      approvers: {approvers}\n{extra_trigger}"
        ),
    )
}

const PERSON: &str = "  - {identity: \"slack:home/U1\", roles: [owner]}\n";

#[test]
fn human_approval_names_current_slack_people_of_the_trigger_workspace() {
    let files = with_approvers(PERSON, "[\"slack:home/U1\"]", "");
    let set = ok(&files);
    let snapshot = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    let main = &snapshot.agents[&crate::support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "confirm").unwrap();
    let human = trigger
        .human_approval
        .as_ref()
        .expect("human_approval parsed");
    assert_eq!(human.approvers.len(), 1);
    assert_eq!(human.approvers[0].0, "slack:home/U1");
    assert!(trigger.preapproved.is_none());
    let other = with_approvers(
        &format!("{PERSON}  - {{identity: \"slack:home/U2\", roles: [owner]}}\n"),
        "[\"slack:home/U2\"]",
        "",
    );
    assert_ne!(snapshot.revision, crate::support::snapshot(&other).revision);
}

#[test]
fn rejects_approvers_that_cannot_be_evidence_and_misplaced_declarations() {
    let cases: Vec<(&str, Files)> = vec![
        (
            "non-member",
            with_approvers(PERSON, "[\"slack:home/U9\"]", ""),
        ),
        (
            "other workspace",
            with_approvers(
                &format!("{PERSON}  - {{identity: \"slack:other/U1\", roles: [owner]}}\n"),
                "[\"slack:other/U1\"]",
                "",
            ),
        ),
        (
            "service principal",
            with_approvers(PERSON, "[\"service:morning\"]", ""),
        ),
        (
            "local identity",
            with_approvers(PERSON, "[\"local:owner\"]", ""),
        ),
        (
            "duplicate",
            with_approvers(PERSON, "[\"slack:home/U1\", \"slack:home/U1\"]", ""),
        ),
        ("empty list", with_approvers(PERSON, "[]", "")),
        (
            "both preapproved and human_approval",
            with_approvers(PERSON, "[\"slack:home/U1\"]", PREAPPROVED),
        ),
        (
            "cron source",
            append(
                with_notify(true),
                TRIGGERS,
                "  - id: timed\n    \"on\": {cron: \"0 9 * * *\"}\n    principal: \"service:morning\"\n    approver: \"local:owner\"\n    target: {tool: agenda.notify, args: {report: daily}}\n    human_approval:\n      approvers: [\"slack:home/U1\"]\n",
            ),
        ),
        (
            "instruction target",
            append(
                with_approvers(PERSON, "[\"slack:home/U1\"]", ""),
                TRIGGERS,
                "  - id: ask\n    \"on\": {event: message, binding: team}\n    order: 5\n    target: {instruction: notify}\n    human_approval:\n      approvers: [\"slack:home/U1\"]\n",
            ),
        ),
    ];
    for (name, files) in &cases {
        match check(files) {
            Ok(_) => panic!("{name}: expected failure"),
            Err(report) => assert!(
                report.has("invalid_human_approval") || report.has("invalid_preapproval"),
                "{name}: missing fixed error code\n{report}"
            ),
        }
    }
}

fn with_event_trigger(events: &str, trigger_lines: &str) -> Files {
    let files = edit(
        with_notify(true),
        PROFILE,
        "  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}",
        &format!(
            "  - {{id: team, adapter: slack, workspace: home, channel: planning, agent: main, events: {events}}}"
        ),
    );
    let files = edit(
        files,
        PROFILE,
        "  - {identity: \"service:morning\", roles: [reader]}\n",
        &format!("  - {{identity: \"service:morning\", roles: [reader]}}\n{PERSON}"),
    );
    append(
        files,
        TRIGGERS,
        &format!(
            "  - id: confirm\n{trigger_lines}    input: {{report: body.text}}\n    target: {{tool: agenda.notify}}\n    human_approval:\n      approvers: [\"slack:home/U1\"]\n"
        ),
    )
}

#[test]
fn human_approval_accepts_reaction_and_thread_reply_triggers_with_an_exact_confirmation() {
    let reaction = with_event_trigger(
        "[mention, reaction]",
        "    \"on\": {event: reaction, binding: team}\n    match: {body.reaction: white_check_mark}\n",
    );
    let set = ok(&reaction);
    let snapshot = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    let main = &snapshot.agents[&crate::support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "confirm").unwrap();
    assert!(trigger.human_approval.is_some());
    assert!(matches!(
        &trigger.on,
        bot_core::profile::TriggerSource::Event { event, .. } if event == "reaction"
    ));
    ok(&with_event_trigger(
        "[mention, thread_reply]",
        "    \"on\": {event: thread-reply, binding: team}\n    match: {body.text: approve}\n",
    ));
}

#[test]
fn human_approval_needs_the_exact_phrase_or_emoji_pinned_by_match() {
    let cases: Vec<(&str, Files)> = vec![
        (
            "reaction without match",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n",
            ),
        ),
        (
            "reaction pinning non-emoji field",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.channel: C1}\n",
            ),
        ),
        (
            "reaction pinning body.text",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.text: approve}\n",
            ),
        ),
        (
            "empty reaction emoji",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.reaction: \"\"}\n",
            ),
        ),
        (
            "message without match",
            with_event_trigger("[mention]", "    \"on\": {event: message, binding: team}\n"),
        ),
        (
            "thread-reply without match",
            with_event_trigger(
                "[mention, thread_reply]",
                "    \"on\": {event: thread-reply, binding: team}\n",
            ),
        ),
        (
            "message pinning non-string text",
            with_event_trigger(
                "[mention]",
                "    \"on\": {event: message, binding: team}\n    match: {body.text: true}\n",
            ),
        ),
    ];
    for (name, files) in &cases {
        expect(name, files, "invalid_human_approval");
        let report = check(files).expect_err("expected failure").to_string();
        assert!(report.contains("match"), "{name}: {report}");
    }
}
