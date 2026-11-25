//! v2.2 `preapproved`(운영자가 사전 승인한 정적 호출)와 `human_approval`(지정한 사람의 승인)의 선언 검증.
mod support;

use support::{Files, append, base, check, edit, ok};

const TRIGGERS: &str = "agents/main/triggers.yaml";
const TOOLS: &str = "agents/main/skills/agenda/tools.yaml";
const PROFILE: &str = "profile.yaml";

/// 단계 원장을 쓰는 되돌릴 수 없는 도구(승인 대상). `effects`만 바꿔 본다.
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
        Ok(_) => panic!("{name}: 거절해야 하는데 통과했어요(기대 {code})"),
        Err(report) => assert!(report.has(code), "{name}: 기대 {code}\n{report}"),
    }
}

#[test]
fn a_fixed_cron_trigger_with_a_preapproval_needs_no_human_approver_and_sorts_its_pairs() {
    // 사전 승인이 없으면 승인이 필요한 도구를 cron이 부르려면 approver가 있어야 한다(기존 규칙)
    expect("approver 필요", &cron(""), "approver-required");
    let files = cron(PREAPPROVED);
    let set = ok(&files);
    let snapshot = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    let main = &snapshot.agents[&support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "notify").unwrap();
    let pre = trigger.preapproved.as_ref().expect("선언을 옮긴다");
    assert_eq!(pre.max_per_day, 24);
    assert_eq!(
        pre.targets,
        vec![
            ("post".to_owned(), "channel:alerts".to_owned()),
            ("post".to_owned(), "channel:reports".to_owned()),
        ],
        "정렬된 쌍"
    );
    assert!(trigger.human_approval.is_none());
    // 선언이 판본에 들어간다: 상한이나 쌍이 바뀌면 다른 판본이다
    let other = cron(&PREAPPROVED.replace("24", "25"));
    assert_ne!(snapshot.revision, support::snapshot(&other).revision);
}

#[test]
fn rejects_malformed_or_unenforceable_preapprovals_with_the_fixed_reasons() {
    let pre = |body: &str| format!("    preapproved:\n{body}");
    let targets = "      targets: [{operation: post, target: \"channel:reports\"}]\n";
    let mut cases: Vec<(&str, Files, &str)> = vec![
        (
            "한도 0",
            cron(&pre(&format!("      max_per_day: 0\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "한도 상한 초과",
            cron(&pre(&format!("      max_per_day: 65536\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "한도가 문자열",
            cron(&pre(&format!("      max_per_day: \"3\"\n{targets}"))),
            "invalid_preapproval",
        ),
        (
            "빈 목록",
            cron(&pre("      max_per_day: 3\n      targets: []\n")),
            "invalid_preapproval",
        ),
        (
            "와일드카드",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: \"channel:*\"}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "URL 대상",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: \"https://hooks.example.com/x\"}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "중복 쌍",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: a}, {operation: post, target: a}]\n",
            )),
            "invalid_preapproval",
        ),
        (
            "모르는 키",
            cron(&pre(&format!(
                "      max_per_day: 3\n{targets}      note: x\n"
            ))),
            "invalid_preapproval",
        ),
        (
            "쌍에 모르는 키",
            cron(&pre(
                "      max_per_day: 3\n      targets: [{operation: post, target: a, params: x}]\n",
            )),
            "invalid_preapproval",
        ),
        ("객체가 아님", cron("    preapproved: yes\n"), "schema"),
    ];
    // 도구가 effects가 아니면 쓰기 경유를 보증할 수 없다
    cases.push((
        "effects 아님",
        append(
            with_notify(false),
            TRIGGERS,
            &format!("  - id: notify\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.notify, args: {{report: daily}}}}\n{PREAPPROVED}"),
        ),
        "preapproval_unenforceable",
    ));
    // 읽기 도구(effects 없음)
    cases.push((
        "읽기 도구",
        append(
            base(),
            TRIGGERS,
            &format!("  - id: look\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{tool: agenda.list, args: {{}}}}\n{PREAPPROVED}"),
        ),
        "preapproval_unenforceable",
    ));
    // 모델 지시(instruction)
    cases.push((
        "instruction 대상",
        append(
            base(),
            TRIGGERS,
            &format!("  - id: ask\n    \"on\": {{cron: \"0 9 * * *\"}}\n    principal: \"service:morning\"\n    target: {{instruction: 알려줘}}\n{PREAPPROVED}"),
        ),
        "invalid_preapproval",
    ));
    // 이벤트 트리거
    cases.push((
        "이벤트 원천",
        append(
            with_notify(true),
            TRIGGERS,
            &format!("  - id: byhand\n    \"on\": {{event: message, binding: team}}\n    input: {{report: body.text}}\n    target: {{tool: agenda.notify}}\n{PREAPPROVED}"),
        ),
        "invalid_preapproval",
    ));
    // beat
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
            "  - id: confirm\n    \"on\": {{event: message, binding: team}}\n    match: {{body.text: 승인}}\n    input: {{report: body.text}}\n    target: {{tool: agenda.notify}}\n    human_approval:\n      approvers: {approvers}\n{extra_trigger}"
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
    let main = &snapshot.agents[&support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "confirm").unwrap();
    let human = trigger.human_approval.as_ref().expect("선언을 옮긴다");
    assert_eq!(human.approvers.len(), 1);
    assert_eq!(human.approvers[0].0, "slack:home/U1");
    assert!(trigger.preapproved.is_none());
    // 지정이 바뀌면 판본이 바뀐다
    let other = with_approvers(
        &format!("{PERSON}  - {{identity: \"slack:home/U2\", roles: [owner]}}\n"),
        "[\"slack:home/U2\"]",
        "",
    );
    assert_ne!(snapshot.revision, support::snapshot(&other).revision);
}

#[test]
fn rejects_approvers_that_cannot_be_evidence_and_misplaced_declarations() {
    let cases: Vec<(&str, Files)> = vec![
        (
            "멤버가 아님",
            with_approvers(PERSON, "[\"slack:home/U9\"]", ""),
        ),
        (
            "다른 워크스페이스",
            with_approvers(
                &format!("{PERSON}  - {{identity: \"slack:other/U1\", roles: [owner]}}\n"),
                "[\"slack:other/U1\"]",
                "",
            ),
        ),
        (
            "서비스 주체",
            with_approvers(PERSON, "[\"service:morning\"]", ""),
        ),
        ("로컬 신원", with_approvers(PERSON, "[\"local:owner\"]", "")),
        (
            "중복",
            with_approvers(PERSON, "[\"slack:home/U1\", \"slack:home/U1\"]", ""),
        ),
        ("빈 목록", with_approvers(PERSON, "[]", "")),
        (
            "함께 쓸 수 없음",
            with_approvers(PERSON, "[\"slack:home/U1\"]", PREAPPROVED),
        ),
        (
            "cron에는 쓸 수 없음",
            append(
                with_notify(true),
                TRIGGERS,
                "  - id: timed\n    \"on\": {cron: \"0 9 * * *\"}\n    principal: \"service:morning\"\n    approver: \"local:owner\"\n    target: {tool: agenda.notify, args: {report: daily}}\n    human_approval:\n      approvers: [\"slack:home/U1\"]\n",
            ),
        ),
        (
            "모델 지시에는 쓸 수 없음",
            append(
                with_approvers(PERSON, "[\"slack:home/U1\"]", ""),
                TRIGGERS,
                "  - id: ask\n    \"on\": {event: message, binding: team}\n    order: 5\n    target: {instruction: 알려줘}\n    human_approval:\n      approvers: [\"slack:home/U1\"]\n",
            ),
        ),
    ];
    for (name, files) in &cases {
        match check(files) {
            Ok(_) => panic!("{name}: 거절해야 하는데 통과했어요"),
            Err(report) => assert!(
                report.has("invalid_human_approval") || report.has("invalid_preapproval"),
                "{name}: 기대한 고정 코드가 없어요\n{report}"
            ),
        }
    }
}

/// 반응·스레드 답글 트리거에 `human_approval`을 붙인 프로필. `events`는 바인딩이 받는 이벤트, `trigger`는 on·match 줄이다.
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
    // 계약 예시: 등록된 채널에서 정확한 반응을 받은 경우(행위자 = 반응한 사람)
    let reaction = with_event_trigger(
        "[mention, reaction]",
        "    \"on\": {event: reaction, binding: team}\n    match: {body.reaction: white_check_mark}\n",
    );
    let set = ok(&reaction);
    let snapshot = set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap();
    let main = &snapshot.agents[&support::agent("main")];
    let trigger = main.triggers.iter().find(|t| t.id == "confirm").unwrap();
    assert!(trigger.human_approval.is_some());
    assert!(matches!(
        &trigger.on,
        bot_core::profile::TriggerSource::Event { event, .. } if event == "reaction"
    ));
    // 봇이 참여한 스레드의 정확한 확인 문구
    ok(&with_event_trigger(
        "[mention, thread_reply]",
        "    \"on\": {event: thread-reply, binding: team}\n    match: {body.text: 승인}\n",
    ));
}

#[test]
fn human_approval_needs_the_exact_phrase_or_emoji_pinned_by_match() {
    let cases: Vec<(&str, Files)> = vec![
        (
            "반응에 match 없음",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n",
            ),
        ),
        (
            "반응인데 이모지가 아닌 필드만 고정",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.channel: C1}\n",
            ),
        ),
        (
            "반응에 본문 글을 고정",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.text: 승인}\n",
            ),
        ),
        (
            "빈 이모지",
            with_event_trigger(
                "[mention, reaction]",
                "    \"on\": {event: reaction, binding: team}\n    match: {body.reaction: \"\"}\n",
            ),
        ),
        (
            "메시지에 match 없음",
            with_event_trigger("[mention]", "    \"on\": {event: message, binding: team}\n"),
        ),
        (
            "답글에 match 없음",
            with_event_trigger(
                "[mention, thread_reply]",
                "    \"on\": {event: thread-reply, binding: team}\n",
            ),
        ),
        (
            "메시지가 문자열이 아닌 값에 고정",
            with_event_trigger(
                "[mention]",
                "    \"on\": {event: message, binding: team}\n    match: {body.text: true}\n",
            ),
        ),
    ];
    for (name, files) in &cases {
        expect(name, files, "invalid_human_approval");
        // 거절 이유가 확인 문구·이모지 고정이다(다른 이유로 우연히 거절된 것이 아니다)
        let report = check(files).expect_err("거절").to_string();
        assert!(report.contains("match"), "{name}: {report}");
    }
}
