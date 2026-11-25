//! 팀 프로필 템플릿(`examples/profiles/team`)을 README 5분 시작과 같은 준비로 확인한다: 내장 데모 호스트(`BOT_HOST_CONFIG` 없음)와
//! 저장소의 예시 폴더 전체(personal + team). 검증·지도는 DB·srt 없이 돌고, 대화는 팀 시험 DB·`srt`·`python3`가 있어야 하며 없으면 건너뛴다.
mod process_support;
use process_support::*;
use std::path::{Path, PathBuf};

/// 걸러 내지 않은 저장소의 예시 폴더. 내장 데모 호스트가 두 프로필의 모델과 CLI 계정(`owner`·`team`)을 모두 안다.
fn all_examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/profiles")
}

#[test]
fn the_team_template_validates_and_maps_with_the_built_in_demo_host() {
    let profiles = all_examples();
    let env = [("BOT_PROFILES", profiles.to_str().unwrap())];
    let validate = bot(&env, &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
    let shown = text(&validate);
    assert!(shown.contains("검증 통과: 프로필 2개"), "{shown}");
    assert!(
        shown.contains("team 판본 ") && shown.contains("에이전트 5 도구 2 트리거 1 바인딩 1"),
        "{shown}"
    );
    let map = bot(&env, &["map", "--profile", "team"]);
    assert!(map.status.success(), "{}", text(&map));
    let tree = text(&map);
    // 폴더 계층이 위임 트리다: 팀 리드가 직계 넷에게 맡긴다
    for agent in [
        "main",
        "main/developer",
        "main/designer",
        "main/pm",
        "main/ops",
    ] {
        assert!(tree.contains(&format!("**{agent}**")), "{agent}\n{tree}");
    }
    for child in ["designer", "developer", "ops", "pm"] {
        assert!(
            tree.contains(&format!("delegates → agent:main/{child}")),
            "{child}\n{tree}"
        );
    }
    // 되돌릴 수 없는 도구는 항상 사람 승인, 정기 요약은 cron 하나
    assert!(
        tree.contains("class=irreversible") && tree.contains("approval=always"),
        "{tree}"
    );
    assert!(tree.contains("cron 0 9 * * 1-5"), "{tree}");
}

#[test]
fn the_team_lead_answers_through_the_demo_model() {
    let Some(schema) = Schema::create() else {
        return;
    };
    if !have_srt_and_python() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("t")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let installs = dir.path().join("installs");
    let profiles = all_examples();
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_INSTALLS", installs.to_str().unwrap())],
    );
    let env = refs(&env);
    let _server = start_gateway_roles(&env, "gateway,worker", &socket);
    // 팀 프로필의 CLI 입구(계정 team)로 팀 리드에게 묻는다: 데모 모델이 agenda.list로 합성 일정을 읽어 답한다
    let chat = bot(
        &env,
        &[
            "chat",
            "--profile",
            "team",
            "--request-id",
            "team-list",
            "오늘 일정 알려줘",
        ],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert!(text(&chat).contains("산책"), "{}", text(&chat));
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_runs where profile = 'team' and state = 'succeeded'"
        ),
        "1"
    );
    // 개인 비서 입구는 따로 열려 있고 team의 실행과 섞이지 않는다
    assert_eq!(
        schema.query("select count(*) from {s}.bot_runs where profile = 'personal'"),
        "0"
    );
    // 되돌릴 수 없는 도구(agenda.delete)는 승인 카드가 열릴 때까지 실행되지 않는다
    let start = bot(
        &env,
        &[
            "chat",
            "--profile",
            "team",
            "--no-wait",
            "--request-id",
            "team-del",
            "산책 일정을 삭제해줘",
        ],
    );
    assert!(start.status.success(), "{}", text(&start));
    let pending =
        "select count(*) from {s}.bot_approvals where profile = 'team' and state = 'pending'";
    wait_for("승인 카드가 열리지 않았어요", || {
        schema.query(pending) == "1"
    });
}
