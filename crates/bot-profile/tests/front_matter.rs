//! AGENT.md front matter: description(상속 없음, 기본 경로 마지막 이름)와 builtin(읽기 전용 내장 도구, 상속 없음).
mod support;

use support::{AGENT_MD, agent, base, edit, family, snapshot};

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
        let codes = support::codes(&files);
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
    support::ok(&ok);
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
        let codes = support::codes(&files);
        assert!(
            !codes.is_empty() && codes.iter().all(|c| *c == "schema"),
            "{name}: {codes:?}"
        );
    }
}
