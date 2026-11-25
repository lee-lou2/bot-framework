//! 팀 프로필 템플릿(`examples/profiles/team`) 수락 시험: 폴더 계층이 위임 트리이고, 권한 상한·내장 도구·설정 상속·예약·기억이 의도한 대로 읽힌다.
//! 개인 비서 예시와 같은 폴더에 있으므로 내장 데모 호스트(`HostCatalog::demo()`)로 예시 폴더 전체를 읽는다.
use bot_core::{
    id::{AgentId, ProfileId, ToolId},
    profile::{Adapter, Builtin, TriggerSource},
    state::{Risk, Visibility},
};
use bot_profile::{HostCatalog, Limits, load};
use std::path::PathBuf;

fn examples_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

fn agent(s: &str) -> AgentId {
    AgentId(s.to_owned())
}

fn team() -> ProfileId {
    ProfileId("team".into())
}

fn names<'a>(items: impl IntoIterator<Item = &'a String>) -> Vec<&'a str> {
    items.into_iter().map(String::as_str).collect()
}

#[test]
fn validates_team_example() {
    let set = load(&examples_root(), &HostCatalog::demo(), &Limits::default())
        .unwrap_or_else(|r| panic!("예시가 검증을 통과하지 못했어요:\n{r}"));
    assert!(set.warnings().is_empty(), "{:?}", set.warnings());
    let snap = set.profile(&team()).unwrap();
    // 폴더 계층 = 위임 트리: 팀 리드(main) 아래 직계 넷
    let ids: Vec<&str> = snap.agents.keys().map(|a| a.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "main",
            "main/designer",
            "main/developer",
            "main/ops",
            "main/pm"
        ]
    );
    assert_eq!(snap.root, agent("main"));
    let children: Vec<&str> = snap
        .agents
        .iter()
        .filter(|(_, a)| a.parent == Some(agent("main")))
        .map(|(id, _)| id.0.as_str())
        .collect();
    assert_eq!(
        children,
        ["main/designer", "main/developer", "main/ops", "main/pm"]
    );
    let main = &snap.agents[&agent("main")];
    let developer = &snap.agents[&agent("main/developer")];
    let designer = &snap.agents[&agent("main/designer")];
    let pm = &snap.agents[&agent("main/pm")];
    let ops = &snap.agents[&agent("main/ops")];
    // 한 줄 설명: 부모가 위임 대상에게서 기대할 일
    assert!(developer.description.contains("읽기 전용"));
    assert!(ops.description.contains("승인"));
    // 내장 도구는 개발자만, 읽기 전용 `read`만 연다(상속하지 않는다)
    assert_eq!(
        developer.builtin.iter().collect::<Vec<_>>(),
        [&Builtin::Read]
    );
    for other in [main, designer, pm, ops] {
        assert!(other.builtin.is_empty());
    }
    // 권한 상한: 개발자·디자이너는 도구를 쓸 수 없고, 기획은 조회와 예약, 운영은 조회와 삭제(승인 대상)까지다
    assert_eq!(names(&developer.scopes), ["run"]);
    assert_eq!(names(&designer.scopes), ["run"]);
    assert_eq!(
        names(&pm.scopes),
        [
            "calendar.read",
            "run",
            "schedule.cancel",
            "schedule.create",
            "schedule.list"
        ]
    );
    assert_eq!(
        names(&ops.scopes),
        ["calendar.delete", "calendar.read", "run"]
    );
    // 도구 선언 예시: 조회는 read, 삭제는 승인이 항상 필요한 irreversible. 자식도 부모의 스킬을 본다
    let agenda = &main.skills["agenda"];
    let list = &agenda.tools[&ToolId("agenda.list".into())];
    let delete = &agenda.tools[&ToolId("agenda.delete".into())];
    assert_eq!((list.class, delete.class), (Risk::Read, Risk::Irreversible));
    assert_eq!(list.server, "demo-agenda");
    assert!(list.env.contains("TEAM_TIMEZONE"));
    assert!(ops.skills.contains_key("agenda") && ops.skills.contains_key("incident-runbook"));
    assert_eq!(
        names(developer.skills.keys()),
        ["agenda", "code-review", "delegation", "report-format"]
    );
    assert!(
        !designer.skills.contains_key("code-review"),
        "형제의 스킬은 보이지 않는다"
    );
    // 규칙 → 부모 → 자기 순서로 지침이 합쳐진다
    let at = |needle: &str| {
        ops.instructions
            .find(needle)
            .unwrap_or_else(|| panic!("{needle}"))
    };
    assert!(
        at("답은 한국어로") < at("너는 이 팀의 리드다")
            && at("너는 이 팀의 리드다") < at("너는 이 팀의 운영 담당이다"),
        "rules → 부모 → 자기 순서"
    );
    // 설정은 자식이 이어받고 같은 이름은 자식 값이 이긴다
    assert_eq!(main.env["TEAM_TIMEZONE"], "Asia/Seoul");
    assert_eq!(pm.env["TEAM_TIMEZONE"], "Asia/Seoul");
    assert_eq!(ops.env["TEAM_TIMEZONE"], "UTC");
    assert_eq!(ops.env["TEAM_NAME"], "example-team");
    // 평일 아침 팀 요약: 승인이 필요 없는 cron(서비스 주체는 읽기 역할)
    assert_eq!(main.triggers.len(), 1);
    let standup = &main.triggers[0];
    assert_eq!(standup.id, "standup");
    match &standup.on {
        TriggerSource::Cron {
            schedule,
            principal,
            approver,
            ..
        } => {
            assert_eq!(
                (schedule.as_str(), principal.0.as_str()),
                ("0 9 * * 1-5", "service:standup")
            );
            assert!(approver.is_none());
        }
        other => panic!("{other:?}"),
    }
    // 기억 시드: 에이전트마다 따로(상속하지 않는다). main은 팀 공유, pm은 비공개
    assert_eq!(main.memory_seeds.len(), 1);
    assert_eq!(main.memory_seeds[0].visibility, Visibility::Profile);
    assert_eq!(pm.memory_seeds.len(), 1);
    assert_eq!(pm.memory_seeds[0].visibility, Visibility::Private);
    assert!(developer.memory_seeds.is_empty() && ops.memory_seeds.is_empty());
    // 지식: 프로필 공용 둘 + 리드의 약속 하나 + 에이전트별 하나(자식은 조상의 것도 본다)
    assert_eq!(main.knowledge.len(), 3);
    assert_eq!(developer.knowledge.len(), 4);
    assert_eq!(ops.knowledge.len(), 4);
    // 입구 하나(CLI 계정 team)로 팀 리드에게만 말을 건다
    assert_eq!(snap.bindings.len(), 1);
    assert!(
        matches!(&snap.bindings.values().next().unwrap().source, Adapter::Cli { account } if account == "team")
    );
    let shown = set.render();
    assert!(
        shown.contains("에이전트 5 도구 2 트리거 1 바인딩 1"),
        "{shown}"
    );
}
