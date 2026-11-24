//! 구조 지도: 스냅숏에서 만든 카드·연결·구조 버전과 `/v1/admin/map` 응답.

mod support;
use bot_admin::{
    fake::{demo_catalog, personal, work},
    map::{self, Kind, Rel},
};
use bot_core::{
    id::ProfileId,
    profile::{Builtin, Document, Snapshot},
    state::Risk,
};
use std::{collections::BTreeMap, sync::Arc};
use support::*;

fn build(snaps: Vec<Snapshot>) -> map::Map {
    let all: BTreeMap<ProfileId, Arc<Snapshot>> = snaps
        .into_iter()
        .map(|s| (s.id.clone(), Arc::new(s)))
        .collect();
    map::build(&all)
}

fn find<'a>(m: &'a map::Map, profile: &str, id: &str) -> &'a map::Card {
    m.nodes
        .iter()
        .find(|c| c.profile == profile && c.id() == id)
        .unwrap_or_else(|| panic!("카드 없음: {profile}/{id}"))
}

fn fact<'a>(c: &'a map::Card, key: &str) -> Option<&'a str> {
    c.facts
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn targets(c: &map::Card, rel: Rel) -> Vec<&str> {
    c.links
        .iter()
        .filter(|(r, _)| *r == rel)
        .map(|(_, to)| to.as_str())
        .collect()
}

#[test]
fn cards_have_unique_ids_and_no_dangling_links() {
    let m = build(vec![personal(), work()]);
    assert!(
        map::problems(&m.nodes).is_empty(),
        "{:?}",
        map::problems(&m.nodes)
    );
    let kinds: std::collections::BTreeSet<Kind> = m.nodes.iter().map(|c| c.kind).collect();
    assert_eq!(kinds.len(), 8, "{kinds:?}");
    assert!(
        m.nodes
            .iter()
            .all(|c| !c.summary.is_empty() && c.summary.chars().count() <= 140)
    );
}

#[test]
fn an_inherited_skill_is_one_card_owned_by_the_agent_that_defines_it() {
    let m = build(vec![personal()]);
    let skills: Vec<_> = m.nodes.iter().filter(|c| c.kind == Kind::Skill).collect();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].agent.as_deref(), Some("main"));
    let child = find(&m, "personal", "agent:main/research");
    assert_eq!(
        targets(child, Rel::Uses),
        ["main/skill:agenda"],
        "자식은 부모의 카드를 가리킨다"
    );
    assert_eq!(
        targets(find(&m, "personal", "agent:main"), Rel::Delegates),
        ["agent:main/research"]
    );
    assert_eq!(
        targets(find(&m, "personal", "profile:personal"), Rel::Delegates),
        ["agent:main"]
    );
}

#[test]
fn tools_carry_class_approval_scopes_and_secret_names_only() {
    let m = build(vec![personal(), work()]);
    let del = find(&m, "personal", "main/tool:agenda.delete");
    assert_eq!(
        (fact(del, "class"), fact(del, "approval")),
        (Some("irreversible"), Some("always"))
    );
    let list = find(&m, "personal", "main/tool:agenda.list");
    assert_eq!(
        (fact(list, "class"), fact(list, "approval")),
        (Some("read"), Some("none"))
    );
    assert_eq!(targets(list, Rel::Needs), ["scope:calendar.read"]);
    let send = find(&m, "work", "main/tool:mail.send");
    assert_eq!(
        (
            fact(send, "approval"),
            fact(send, "external"),
            fact(send, "effects")
        ),
        (Some("policy"), Some("true"), Some("true"))
    );
    assert_eq!(fact(send, "destination"), Some("args.to <- args.message"));
    assert_eq!(fact(send, "egress"), Some("mail.example:443"));
    let needs = targets(send, Rel::Needs);
    assert!(
        needs.contains(&"secret:mail-sender") && needs.contains(&"scope:mail.send"),
        "{needs:?}"
    );
    let scope = find(&m, "work", "scope:mail.send");
    assert_eq!(
        (
            fact(scope, "granted"),
            fact(scope, "approval"),
            fact(scope, "roles")
        ),
        (Some("true"), Some("true"), Some("owner"))
    );
    let json = serde_json::to_string(&m.nodes.iter().map(map::Card::json).collect::<Vec<_>>())
        .expect("JSON");
    assert!(!json.contains("MAIL_TOKEN"), "환경 변수 이름도 싣지 않는다");
}

#[test]
fn triggers_link_to_their_targets_and_bindings_to_their_agents() {
    let m = build(vec![personal(), work()]);
    let morning = find(&m, "personal", "main/trigger:morning");
    assert_eq!(targets(morning, Rel::Triggers), ["main/tool:agenda.list"]);
    assert_eq!(
        (
            fact(morning, "on"),
            fact(morning, "principal"),
            fact(morning, "target")
        ),
        (
            Some("cron"),
            Some("service:morning"),
            Some("tool agenda.list")
        )
    );
    assert_eq!(
        targets(find(&m, "personal", "agent:main"), Rel::Runs),
        ["main/trigger:morning"]
    );
    let http = find(&m, "work", "main/trigger:cancel");
    assert_eq!(
        (fact(http, "when"), fact(http, "response")),
        (Some("POST /mail @inbox"), Some("accepted"))
    );
    assert_eq!(targets(http, Rel::Triggers), ["agent:main"]);
    assert_eq!(
        targets(find(&m, "work", "binding:inbox"), Rel::Handles),
        ["agent:main"]
    );
    assert_eq!(
        fact(find(&m, "work", "binding:team"), "detail"),
        Some("home #planning")
    );
}

#[test]
fn rev_is_stable_for_the_same_structure_and_changes_with_it() {
    let a = build(vec![personal(), work()]);
    assert_eq!(
        a.rev,
        build(vec![work(), personal()]).rev,
        "입력 순서와 무관"
    );
    assert_eq!(a.rev.len(), 12);
    let mut changed = personal();
    if let Some(t) = changed
        .agents
        .values_mut()
        .flat_map(|a| a.skills.values_mut())
        .flat_map(|k| k.tools.values_mut())
        .next()
    {
        t.class = Risk::Write;
    }
    assert_ne!(a.rev, build(vec![changed, work()]).rev);
}

#[test]
fn instruction_text_documents_and_secret_values_never_reach_the_map() {
    let mut s = personal();
    for a in s.agents.values_mut() {
        a.instructions = "PRIVATE-PROMPT-TEXT".into();
        a.knowledge.push(Document {
            relative_path: "knowledge/a.md".into(),
            text: "PRIVATE-DOC-TEXT".into(),
        });
        a.env.insert("KEY".into(), "PRIVATE-ENV-VALUE".into());
        for sk in a.skills.values_mut() {
            sk.instructions.text = "PRIVATE-SKILL-TEXT".into();
        }
    }
    let m = build(vec![s.clone()]);
    let all = serde_json::to_string(&m.nodes.iter().map(map::Card::json).collect::<Vec<_>>())
        .expect("JSON")
        + &map::access(&s).to_string();
    for banned in [
        "PRIVATE-PROMPT-TEXT",
        "PRIVATE-DOC-TEXT",
        "PRIVATE-ENV-VALUE",
        "PRIVATE-SKILL-TEXT",
    ] {
        assert!(!all.contains(banned), "{banned}");
    }
}

#[tokio::test]
async fn the_map_endpoint_is_operator_only_and_supports_profile_and_rev() {
    let s = start(bot_admin::fake::Fake::demo()).await;
    assert_eq!(get(&s, GUEST, "/v1/admin/map").await.status, 403);
    let all = get(&s, OPERATOR, "/v1/admin/map").await;
    assert_eq!(all.status, 200);
    let j = all.json();
    let rev = j["rev"].as_str().expect("rev").to_owned();
    assert!(j["graph"]["nodes"].as_array().is_some_and(|n| n.len() > 20));
    assert_eq!(
        j["access"]["personal"]["members"][0]["identity"],
        "local:owner"
    );
    assert_eq!(j["access"]["work"]["policy"]["approve"][0], "mail.send");
    assert_eq!(j["build"]["name"], "bot");
    // 같은 rev면 카드를 생략한다(접근 정보는 계속 온다).
    let same = get(&s, OPERATOR, &format!("/v1/admin/map?rev={rev}"))
        .await
        .json();
    assert!(same["graph"].is_null() && same["access"]["personal"].is_object());
    let stale = get(&s, OPERATOR, "/v1/admin/map?rev=000000000000")
        .await
        .json();
    assert!(stale["graph"]["nodes"].is_array());
    let one = get(&s, OPERATOR, "/v1/admin/map?profile=work").await.json();
    let profiles: std::collections::BTreeSet<&str> = one["graph"]["nodes"]
        .as_array()
        .expect("카드")
        .iter()
        .filter_map(|c| c["profile"].as_str())
        .collect();
    assert_eq!(profiles, ["work"].into_iter().collect());
    assert!(
        one["access"]["personal"].is_null(),
        "고르지 않은 프로필은 싣지 않는다"
    );
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/map?profile=nope").await.status,
        404
    );
    assert_eq!(
        get(&s, OPERATOR, "/v1/admin/map?profile=Bad").await.status,
        400
    );
    let _ = demo_catalog();
}

#[test]
fn an_agent_card_shows_its_builtin_tools_and_its_own_description() {
    let m = build(vec![personal(), work()]);
    let main = find(&m, "personal", "agent:main");
    assert_eq!(
        fact(main, "builtin"),
        Some("read, grep"),
        "열어 준 내장 도구만, 정해진 순서로"
    );
    assert!(
        main.summary.starts_with("오늘 일정을 조회"),
        "{}",
        main.summary
    );
    // 상속하지 않는다: 자식과 설명이 없는 에이전트는 비어 있고 요약은 모델을 보인다.
    let child = find(&m, "personal", "agent:main/research");
    assert_eq!(fact(child, "builtin"), Some("-"));
    assert!(child.summary.contains("모델"), "{}", child.summary);
    let mut s = personal();
    if let Some(a) = s.agents.get_mut(&bot_core::id::AgentId("main".into())) {
        a.description = format!("{}\n둘째 줄은 쓰지 않는다", "가".repeat(200));
        a.builtin = std::collections::BTreeSet::from([Builtin::Glob, Builtin::Read]);
    }
    let m = build(vec![s]);
    let main = find(&m, "personal", "agent:main");
    assert_eq!(
        main.summary.chars().count(),
        140,
        "요약은 첫 줄을 140자까지"
    );
    assert_eq!(fact(main, "builtin"), Some("read, glob"));
    assert!(map::problems(&m.nodes).is_empty());
}
