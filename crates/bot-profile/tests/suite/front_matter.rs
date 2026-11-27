//! AGENT.md front matter: description(상속 없음, 기본 경로 마지막 이름)와 builtin(읽기 전용 내장 도구, 상속 없음).

use crate::support::{AGENT_MD, agent, base, edit, family, snapshot};

#[test]
fn description_comes_from_front_matter_and_is_not_inherited() {
    let mut files = family();
    files = edit(
        files,
        AGENT_MD,
        "오늘",
        "---\ndescription: 일정을 조회하고 정리하는 비서\n---\n오늘",
    );
    let snap = snapshot(&files);
    let desc = |id: &str| snap.agents[&agent(id)].description.clone();
    assert_eq!(desc("main"), "일정을 조회하고 정리하는 비서");
    assert_eq!(
        desc("main/research"),
        "research",
        "상속하지 않고 경로 마지막 이름"
    );
    assert_eq!(desc("main/research/deep"), "deep");
    assert_eq!(desc("main/other"), "other");

    let both = edit(
        base(),
        AGENT_MD,
        "오늘",
        "---\nmodel: fast\ndescription:   앞뒤 공백은 지운다  \n---\n오늘",
    );
    let snap = snapshot(&both);
    let main = &snap.agents[&agent("main")];
    assert_eq!(
        (main.model.as_str(), main.description.as_str()),
        ("fast", "앞뒤 공백은 지운다")
    );
    assert!(
        !main.instructions.contains("description"),
        "front matter는 지침에 섞이지 않는다"
    );
}

#[test]
fn invalid_descriptions_are_rejected() {
    let long = "가".repeat(201);
    for (name, front) in [
        ("빈 문자열", "description: \"\"".to_owned()),
        ("공백만", "description: \"   \"".to_owned()),
        ("여러 줄", "description: \"첫 줄\\n둘째 줄\"".to_owned()),
        ("너무 김", format!("description: {long}")),
        ("문자열이 아님", "description: [a]".to_owned()),
    ] {
        let files = edit(
            base(),
            AGENT_MD,
            "오늘",
            &format!("---\n{front}\n---\n오늘"),
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
        "오늘",
        &format!("---\ndescription: {}\n---\n오늘", "가".repeat(200)),
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
        "오늘",
        "---\nbuiltin: [grep, read]\n---\n오늘",
    );
    files = edit(
        files,
        "agents/main/agents/research/AGENT.md",
        "부모가",
        "---\nmodel: fast\nbuiltin: [glob]\n---\n부모가",
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
        "선언 순서와 무관하게 정렬된 집합"
    );
    assert_eq!(
        set("main/research"),
        [Builtin::Glob],
        "부모 것을 물려받지 않고 자기 선언만"
    );
    assert!(
        set("main/research/deep").is_empty(),
        "손자는 아무것도 받지 않는다"
    );
    assert!(set("main/other").is_empty(), "형제에게도 퍼지지 않는다");
    // 빈 배열과 생략은 같다(기존 번들에는 builtin이 없다)
    let empty = snapshot(&edit(
        base(),
        AGENT_MD,
        "오늘",
        "---\nbuiltin: []\n---\n오늘",
    ));
    assert!(empty.agents.values().all(|a| a.builtin.is_empty()));
    let all = snapshot(&edit(
        base(),
        AGENT_MD,
        "오늘",
        "---\nmodel: fast\ndescription: 비서\nbuiltin: [read, grep, glob]\n---\n오늘",
    ));
    assert_eq!(all.agents[&agent("main")].builtin.len(), 3);
}

#[test]
fn invalid_builtin_declarations_are_rejected() {
    for (name, front) in [
        ("중복", "builtin: [read, read]"),
        ("등록되지 않은 도구", "builtin: [bash]"),
        ("쓰기 도구", "builtin: [write]"),
        ("웹", "builtin: [web]"),
        ("대문자", "builtin: [Read]"),
        ("배열이 아님", "builtin: read"),
        ("네 개", "builtin: [read, grep, glob, read]"),
        ("객체 항목", "builtin: [{name: read}]"),
    ] {
        let files = edit(
            base(),
            AGENT_MD,
            "오늘",
            &format!("---\n{front}\n---\n오늘"),
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
    // front matter가 없는 기존 SKILL.md는 그대로 동작한다
    let plain = snapshot(&base());
    let agenda = &plain.agents[&agent("main")].skills["agenda"];
    assert_eq!(agenda.description, None);
    assert_eq!(agenda.instructions.text, "일정 목록은 list로 조회한다.");

    // 허용 필드 전체 + 본문 속 구분선(`---`) 보존 + 앞뒤 공백 정리
    let with_fm = edit(
        base(),
        SKILL_MD,
        "일정 목록은 list로 조회한다.\n",
        "---\nname: agenda\ndescription:   일정을 조회하고 관리한다  \nlicense: MIT\ncompatibility: 로컬 MCP 서버\nmetadata:\n  author: example-org\n  version: \"1.0\"\n---\n\n# 일정\n일정 목록은 list로 조회한다.\n\n---\n\n추가 메모\n",
    );
    let snap = snapshot(&with_fm);
    let agenda = &snap.agents[&agent("main")].skills["agenda"];
    assert_eq!(
        agenda.description.as_deref(),
        Some("일정을 조회하고 관리한다")
    );
    assert_eq!(
        agenda.instructions.text,
        "# 일정\n일정 목록은 list로 조회한다.\n\n---\n\n추가 메모"
    );
    assert!(
        !agenda.instructions.text.contains("name: agenda")
            && !agenda.instructions.text.contains("description:"),
        "front matter는 스킬 본문에 남지 않는다: {}",
        agenda.instructions.text
    );

    // description만 있는 선택적 front matter(name 생략 허용) + 상속 시 자식도 동일 메타·본문을 본다
    let desc_only = edit(
        base(),
        SKILL_MD,
        "일정 목록은 list로 조회한다.\n",
        "---\ndescription: 일정 조회\n---\n일정 목록은 list로 조회한다.\n",
    );
    let snap = snapshot(&desc_only);
    assert_eq!(
        snap.agents[&agent("main/research")].skills["agenda"]
            .description
            .as_deref(),
        Some("일정 조회")
    );
    assert_eq!(
        snap.agents[&agent("main/research")].skills["agenda"]
            .instructions
            .text,
        "일정 목록은 list로 조회한다."
    );
}

#[test]
fn invalid_skill_front_matter_is_rejected() {
    // allowed-tools 거절 + 권한 출처 안내
    let allowed = edit(
        base(),
        SKILL_MD,
        "일정",
        "---\nname: agenda\nallowed-tools: Bash(git:*) Read\n---\n일정",
    );
    let report = crate::support::check(&allowed).unwrap_err();
    assert_eq!(report.codes(), ["skill-allowed-tools"]);
    let msg = &report.errors().next().unwrap().message;
    assert!(
        msg.contains("tools.yaml") && msg.contains("정책"),
        "권한 출처 안내가 있어야 해요: {msg}"
    );

    // name과 폴더 이름 불일치 거절
    let mismatch = edit(base(), SKILL_MD, "일정", "---\nname: other\n---\n일정");
    assert_eq!(crate::support::codes(&mismatch), ["skill-name-mismatch"]);

    // 닫히지 않은 front matter 거절
    let unclosed = edit(base(), SKILL_MD, "일정", "---\nname: agenda\n일정");
    assert_eq!(crate::support::codes(&unclosed), ["front-matter-unclosed"]);

    // front matter만 있고 본문이 없는 SKILL.md 거절
    let no_body = edit(
        base(),
        SKILL_MD,
        "일정 목록은 list로 조회한다.\n",
        "---\nname: agenda\ndescription: 일정 조회\n---\n\n",
    );
    assert_eq!(crate::support::codes(&no_body), ["empty-file"]);

    // 스키마 위반 및 공백만 있는 값 거절
    let long_desc = "가".repeat(1025);
    let long_compat = "나".repeat(501);
    for (label, front) in [
        ("알 수 없는 키", "extra: 1".to_owned()),
        ("대문자 name", "name: Agenda".to_owned()),
        ("앞 하이픈 name", "name: -agenda".to_owned()),
        ("연속 하이픈 name", "name: a--b".to_owned()),
        ("빈 description", "description: \"\"".to_owned()),
        ("공백만 description", "description: \"   \"".to_owned()),
        ("1025자 description", format!("description: {long_desc}")),
        ("공백만 license", "license: \"  \"".to_owned()),
        ("공백만 compatibility", "compatibility: \"  \"".to_owned()),
        (
            "501자 compatibility",
            format!("compatibility: {long_compat}"),
        ),
        ("비문자열 metadata", "metadata:\n  version: 1".to_owned()),
    ] {
        let files = edit(
            base(),
            SKILL_MD,
            "일정",
            &format!("---\n{front}\n---\n일정"),
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
    // 본문이 정확히 64 KiB이고 front matter가 덧붙어도 본문 기준이라 통과한다
    let exact = format!(
        "---\nname: agenda\ndescription: {}\n---\n{}\n",
        "설명".repeat(200),
        "a".repeat(limit)
    );
    let exact_files = edit(base(), SKILL_MD, "일정 목록은 list로 조회한다.\n", &exact);
    let snap = snapshot(&exact_files);
    assert_eq!(
        snap.agents[&agent("main")].skills["agenda"]
            .instructions
            .text
            .len(),
        limit
    );

    // 단일 스킬 본문이 64 KiB를 1바이트 넘으면 거절한다
    let over_one = edit(
        base(),
        SKILL_MD,
        "일정 목록은 list로 조회한다.\n",
        &"a".repeat(limit + 1),
    );
    assert_eq!(crate::support::codes(&over_one), ["skill-too-large"]);

    // 상속을 합친 에이전트별 합계가 64 KiB를 넘으면 그 에이전트 경로로 거절하고, 한도 안인 형제는 문제없다
    let mut inherited = family();
    inherited = edit(
        inherited,
        SKILL_MD,
        "일정 목록은 list로 조회한다.\n",
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
