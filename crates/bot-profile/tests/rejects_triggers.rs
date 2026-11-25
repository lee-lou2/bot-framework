//! 거절 사례 표(트리거): 기본 프로필에 트리거를 더하거나 고쳐 어떤 오류 코드가 나와야 하는지 확인한다.
mod support;

use support::{Files, append, base, check, edit};

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
        Ok(_) => panic!("{name}: 거절해야 하는데 통과했어요(기대 {code})"),
        Err(report) => assert!(report.has(code), "{name}: 기대 {code}\n{report}"),
    }
}

/// DM 바인딩의 private 트리거가 `agenda.batch`(쓰기 도구, 승인 불필요)를 부른다. `effects`만 다르다.
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
            "잘못된 cron",
            apply(&[(TRIGGERS, "\"* * * * *\"", "\"61 * * * *\"")]),
            "bad-cron",
        ),
        (
            "없는 도구",
            apply(&[(TRIGGERS, "agenda.list", "agenda.nope")]),
            "unknown-tool",
        ),
        (
            "없는 주체",
            apply(&[(TRIGGERS, "service:morning", "service:ghost")]),
            "trigger-principal-unknown",
        ),
        (
            "cron이 사람 주체",
            apply(&[(TRIGGERS, "service:morning", "local:owner")]),
            "cron-needs-service",
        ),
        (
            "주체에게 스코프 없음",
            apply(&[(TRIGGERS, "target: {tool: agenda.list, args: {}}", delete)]),
            "trigger-scope",
        ),
        ("approver 없음", apply(&strong), "approver-required"),
        (
            "서비스 승인자",
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
            "HTTP 주체는 승인자가 될 수 없음",
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
            "모르는 승인자",
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
            "고정 args가 스키마 위반",
            apply(&[(
                TRIGGERS,
                "target: {tool: agenda.list, args: {}}",
                "target: {tool: agenda.find, args: {query: 5}}",
            )]),
            "args-invalid",
        ),
        (
            "없는 바인딩",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: nope}\n    target: {instruction: x}\n",
            ),
            "unknown-binding",
        ),
        (
            "바인딩 종류 불일치",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: desk}\n    target: {instruction: x}\n",
            ),
            "binding-kind-mismatch",
        ),
        (
            "지원하지 않는 이벤트",
            event(
                "  - id: chat\n    \"on\": {event: edited, binding: team}\n    target: {instruction: x}\n",
            ),
            "unsupported-event",
        ),
        (
            "HTTP 경로",
            event(
                "  - id: hook\n    \"on\": {http: {method: POST, path: \"/a/../b\"}, binding: inbox}\n    target: {instruction: x}\n",
            ),
            "bad-http-path",
        ),
        (
            "봉투 밖 경로",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    match: {secret.token: x}\n    target: {instruction: x}\n",
            ),
            "bad-path",
        ),
        (
            "sync인데 지침 대상",
            event(
                "  - id: hook\n    \"on\": {http: {method: POST, path: /a}, binding: inbox}\n    response: {mode: sync}\n    target: {instruction: x}\n",
            ),
            "sync-needs-tool",
        ),
        (
            "이벤트 트리거에 beat",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    beat: true\n    target: {instruction: x}\n",
            ),
            "schema",
        ),
        (
            "이벤트 트리거에 principal",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    principal: \"service:morning\"\n    target: {instruction: x}\n",
            ),
            "schema",
        ),
        (
            "같은 후보군 같은 순위",
            event(
                "  - id: a\n    \"on\": {event: message, binding: team}\n    target: {instruction: x}\n  - id: b\n    \"on\": {event: message, binding: team}\n    target: {instruction: y}\n",
            ),
            "duplicate-order",
        ),
        (
            "private가 DM 아님",
            event(
                "  - id: secure\n    \"on\": {event: message, binding: team}\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
            ),
            "private-needs-dm",
        ),
        (
            "private보다 일반이 앞",
            event(
                "  - id: plain\n    \"on\": {event: message, binding: direct}\n    target: {instruction: x}\n  - id: secure\n    \"on\": {event: message, binding: direct}\n    order: 5\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
            ),
            "private-preempted",
        ),
        (
            "private 도구가 승인 대상",
            event(
                "  - id: secure\n    \"on\": {event: message, binding: direct}\n    private: true\n    input: {event: body.text}\n    target: {tool: agenda.delete}\n",
            ),
            "private-approval",
        ),
        (
            "private 도구가 다단계(effects)",
            private_batch(true),
            "private-effects",
        ),
        (
            "고정 args와 input 충돌",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    input: {query: body.text}\n    target: {tool: agenda.find, args: {query: x}}\n",
            ),
            "input-key-conflict",
        ),
        (
            "스키마에 없는 인자",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    input: {query: body.text, extra: body.x}\n    target: {tool: agenda.find}\n",
            ),
            "input-unknown-arg",
        ),
        (
            "필수 인자를 정하는 곳이 없음",
            event(
                "  - id: chat\n    \"on\": {event: message, binding: team}\n    target: {tool: agenda.find}\n",
            ),
            "input-missing-required",
        ),
    ];
    cases.push(("트리거 id 중복", append(base(), TRIGGERS, "  - id: morning\n    \"on\": {cron: \"* * * * *\"}\n    principal: \"service:morning\"\n    target: {tool: agenda.list, args: {}}\n"), "duplicate-trigger-id"));
    for (name, files, code) in &cases {
        expect(name, files, code);
    }
}

#[test]
fn accepts_valid_variants_of_the_same_shapes() {
    // 같은 모양의 private 트리거라도 도구가 effects 없는 단일 실행이면 통과한다
    support::ok(&private_batch(false));
    let valid_event = event(
        "  - id: chat\n    \"on\": {event: message, binding: team}\n    order: 3\n    match: {body.kind: ask}\n    input: {query: body.text}\n    target: {tool: agenda.find}\n  - id: hook\n    \"on\": {http: {method: POST, path: /agenda}, binding: inbox}\n    response: {mode: sync}\n    target: {tool: agenda.list}\n  - id: secure\n    \"on\": {event: message, binding: direct}\n    private: true\n    input: {query: body.text}\n    target: {tool: agenda.find}\n",
    );
    let set = support::ok(&valid_event);
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
    support::ok(&with_approver);
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
    let set = support::ok(&warned);
    assert!(set.warnings().iter().any(|w| w.code == "env-looks-secret"));
    let ungranted = apply(&[(
        TOOLS,
        "external: true\n    scopes: [calendar.read]",
        "external: true\n    scopes: [calendar.unknown]",
    )]);
    assert!(
        support::ok(&ungranted)
            .warnings()
            .iter()
            .any(|w| w.code == "tool-scope-ungranted")
    );
}
