//! Rejection matrix for trigger definitions and cross-references.

use crate::support::{Files, append, base, check, edit};

type Edit<'a> = (&'a str, &'a str, &'a str);

const TRIGGERS: &str = "agents/main/triggers.yaml";
const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";

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

fn private_batch(effects: bool) -> Files {
    let tool = format!(
        "  - id: batch\n    class: write\n    effects: {effects}\n    scopes: [calendar.read]\n    input: {{type: object, required: [text], properties: {{text: {{type: string}}}}, additionalProperties: false}}\n"
    );
    let files = append(base(), TOOLS, &tool);
    append(
        files,
        TRIGGERS,
        "  - id: secure\n    \"on\": {event: message, binding: direct}\n    private: true\n    input: {text: body.text}\n    target: {tool: agenda.batch}\n",
    )
}

fn event(extra: &str) -> Files {
    append(base(), TRIGGERS, extra)
}

#[test]
fn rejects_trigger_mistakes() {
    let owner_reader = "{identity: \"service:morning\", roles: [reader]}";
    let delete = "target: {tool: agenda.delete, args: {event: walk}}";
    let strong = [
        (
            "profile.yaml",
            owner_reader,
            "{identity: \"service:morning\", roles: [owner]}",
        ),
        (TRIGGERS, "target: {tool: agenda.list, args: {}}", delete),
    ];
    let mut cases: Vec<(&str, Files, &str)> = vec![
        (
            "invalid cron",
            apply(&[(TRIGGERS, "\"* * * * *\"", "\"61 * * * *\"")]),
            "bad-cron",
        ),
        (
            "unknown tool",
            apply(&[(TRIGGERS, "agenda.list", "agenda.nope")]),
            "unknown-tool",
        ),
        (
            "unknown principal",
            apply(&[(TRIGGERS, "service:morning", "service:ghost")]),
            "trigger-principal-unknown",
        ),
        (
            "cron with human principal",
            apply(&[(TRIGGERS, "service:morning", "local:owner")]),
            "cron-needs-service",
        ),
        (
            "principal lacks scope",
            apply(&[(TRIGGERS, "target: {tool: agenda.list, args: {}}", delete)]),
            "trigger-scope",
        ),
        ("missing approver", apply(&strong), "approver-required"),
        (
            "service approver",
            apply(&[
                strong[0],
                strong[1],
                (
                    TRIGGERS,
                    "principal: \"service:morning\"",
                    "principal: \"service:morning\"\n    approver: \"service:morning\"",
                ),
            ]),
            "approver-service",
        ),
        (
            "HTTP principal cannot approve",
            apply(&[
                strong[0],
                strong[1],
                (
                    "profile.yaml",
                    "{identity: \"service:morning\", roles: [owner]}",
                    "{identity: \"service:morning\", roles: [owner]}\n  - {identity: \"http:reminders/alice\", roles: [owner]}",
                ),
                (
                    TRIGGERS,
                    "principal: \"service:morning\"",
                    "principal: \"service:morning\"\n    approver: \"http:reminders/alice\"",
                ),
            ]),
            "approver-not-human",
        ),
        (
            "unknown approver",
            apply(&[
                strong[0],
                strong[1],
                (
                    TRIGGERS,
                    "principal: \"service:morning\"",
                    "principal: \"service:morning\"\n    approver: \"local:ghost\"",
                ),
            ]),
            "approver-unknown",
        ),
        (
            "static args violate schema",
            apply(&[(
                TRIGGERS,
                "target: {tool: agenda.list, args: {}}",
                "target: {tool: agenda.find, args: {query: 5}}",
            )]),
            "args-invalid",
        ),
        (
            "unknown binding",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: nope}\n    target: {instruction: x}\n",
            ),
            "unknown-binding",
        ),
        (
            "binding kind mismatch",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: desk}\n    target: {instruction: x}\n",
            ),
            "binding-kind-mismatch",
        ),
        (
            "unsupported event",
            event(
                "  - id: chat\n    \"on\": {event: edited, binding: team}\n    target: {instruction: x}\n",
            ),
            "unsupported-event",
        ),
        (
            "invalid HTTP path",
            event(
                "  - id: hook\n    \"on\": {http: {method: POST, path: \"/a/../b\"}, binding: inbox}\n    target: {instruction: x}\n",
            ),
            "bad-http-path",
        ),
        (
            "path outside envelope",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    match: {secret.token: x}\n    target: {instruction: x}\n",
            ),
            "bad-path",
        ),
        (
            "sync with instruction target",
            event(
                "  - id: hook\n    \"on\": {http: {method: POST, path: /a}, binding: inbox}\n    response: {mode: sync}\n    target: {instruction: x}\n",
            ),
            "sync-needs-tool",
        ),
        (
            "beat on event trigger",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    beat: true\n    target: {instruction: x}\n",
            ),
            "schema",
        ),
        (
            "principal on event trigger",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    principal: \"service:morning\"\n    target: {instruction: x}\n",
            ),
            "schema",
        ),
        (
            "duplicate order in group",
            event(
                "  - id: a\n    \"on\": {event: message, binding: team}\n    target: {instruction: x}\n  - id: b\n    \"on\": {event: message, binding: team}\n    target: {instruction: y}\n",
            ),
            "duplicate-order",
        ),
        (
            "private on non-DM binding",
            event(
                "  - id: secure\n    \"on\": {event: message, binding: team}\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
            ),
            "private-needs-dm",
        ),
        (
            "public trigger precedes private trigger",
            event(
                "  - id: plain\n    \"on\": {event: message, binding: direct}\n    target: {instruction: x}\n  - id: secure\n    \"on\": {event: message, binding: direct}\n    order: 5\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
            ),
            "private-preempted",
        ),
        (
            "private tool requires approval",
            event(
                "  - id: secure\n    \"on\": {event: message, binding: direct}\n    private: true\n    input: {event: body.text}\n    target: {tool: agenda.delete}\n",
            ),
            "private-approval",
        ),
        (
            "private tool uses effects",
            private_batch(true),
            "private-effects",
        ),
        (
            "static args and input conflict",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    input: {query: body.text}\n    target: {tool: agenda.find, args: {query: x}}\n",
            ),
            "input-key-conflict",
        ),
        (
            "unknown input arg",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    input: {query: body.text, extra: body.x}\n    target: {tool: agenda.find}\n",
            ),
            "input-unknown-arg",
        ),
        (
            "missing required input arg",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    target: {tool: agenda.find}\n",
            ),
            "input-missing-required",
        ),
    ];
    cases.push((
        "duplicate trigger id",
        append(
            base(),
            TRIGGERS,
            "  - id: morning\n    \"on\": {cron: \"* * * * *\"}\n    principal: \"service:morning\"\n    target: {tool: agenda.list, args: {}}\n",
        ),
        "duplicate-trigger-id",
    ));
    for (name, files, code) in &cases {
        expect(name, files, code);
    }
}

#[test]
fn accepts_valid_variants_of_the_same_shapes() {
    crate::support::ok(&private_batch(false));
    let valid_event = event(
        "  - id: chat\n    \"on\": {event: message, binding: team}\n    order: 3\n    match: {body.kind: ask}\n    input: {query: body.text}\n    target: {tool: agenda.find}\n  - id: hook\n    \"on\": {http: {method: POST, path: /agenda}, binding: inbox}\n    response: {mode: sync}\n    target: {tool: agenda.list}\n  - id: secure\n    \"on\": {event: message, binding: direct}\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
    );
    let set = crate::support::ok(&valid_event);
    let snap = &set
        .active()
        .values()
        .next()
        .unwrap()
        .agents
        .values()
        .next()
        .unwrap()
        .triggers;
    assert_eq!(snap.len(), 4);
    let with_approver = apply(&[
        (
            "profile.yaml",
            "{identity: \"service:morning\", roles: [reader]}",
            "{identity: \"service:morning\", roles: [owner]}\n  - {identity: \"slack:home/U1\", roles: [owner]}",
        ),
        (
            TRIGGERS,
            "target: {tool: agenda.list, args: {}}",
            "approver: \"slack:home/U1\"\n    target: {tool: agenda.delete, args: {event: walk}}",
        ),
    ]);
    crate::support::ok(&with_approver);
    let invalid = edit(
        with_approver,
        "profile.yaml",
        "slack:home/U1",
        "slack:home/u1",
    );
    expect("invalid Slack member", &invalid, "bad-identity");
    let warned = apply(&[(
        "agents/main/env.yaml",
        "AGENDA_VIEW: today",
        "AGENDA_TOKEN: x",
    )]);
    let set = crate::support::ok(&warned);
    assert!(set.warnings().iter().any(|w| w.code == "env-looks-secret"));
    let ungranted = apply(&[(
        TOOLS,
        "external: true\n    scopes: [calendar.read]",
        "external: true\n    scopes: [calendar.unknown]",
    )]);
    assert!(
        crate::support::ok(&ungranted)
            .warnings()
            .iter()
            .any(|w| w.code == "tool-scope-ungranted")
    );
}
