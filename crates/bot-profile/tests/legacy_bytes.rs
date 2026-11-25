//! 웹훅 전용 프로필의 판본 바이트는 OpenAI 모델 바인딩이 생겨도 그대로다.
//! 아래 두 해시(묶음 판본·정책 판본)는 OpenAI 바인딩을 모르던 구현이 이 파일들로 계산한 값이다. 이 값이 달라지면 이미 배포한 프로필의 판본이
//! 바뀌어 보관한 묶음 복원·진행 중 승인이 깨지므로, 구현을 고쳐야지 값을 고치면 안 된다.
mod support;

use bot_core::id::ProfileId;
use bot_profile::{Limits, from_bundle, revision_of};
use support::{Files, check, raw_bundle};

const GOLDEN_REVISION: &str = "70299af52fee70c9f2d34fb16a4c1ea5e985e98be6c2286b751b71d6f3646727";
const GOLDEN_POLICY: &str = "28413cb02691238dd12273e2b785161a6a4e80db9089b0769ee422266e6f068f";

/// 옛 네 가지 바인딩, 사람·서비스·HTTP·Slack 멤버, 승인자가 있는 cron을 모두 담은 자체 완결 프로필.
fn legacy() -> Files {
    let profile = r#"version: 1
id: personal
timezone: Asia/Seoul
model: assistant
roles:
  owner: [run, calendar.read]
  caller: [run]
members:
  - {identity: "local:owner", roles: [owner]}
  - {identity: "service:digest", roles: [caller]}
  - {identity: "http:reminders/ci", roles: [caller]}
  - {identity: "slack:home/U1", roles: [owner]}
bindings:
  - {id: desk, adapter: cli, account: owner, agent: main}
  - {id: inbox, adapter: http, endpoint: reminders, agent: main}
  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}
  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}
policy:
  scopes: [run, calendar.read]
  approve: []
"#;
    let triggers = r#"version: 1
triggers:
  - id: nightly
    "on": {cron: "0 3 * * *"}
    principal: "service:digest"
    approver: "local:owner"
    target: {instruction: "요약해 줘"}
"#;
    [
        ("profile.yaml", profile),
        ("agents/main/AGENT.md", "도와준다.\n"),
        ("agents/main/triggers.yaml", triggers),
    ]
    .iter()
    .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
    .collect()
}

#[test]
fn webhook_only_profiles_keep_their_revision_and_policy_revision() {
    let files = legacy();
    let set = check(&files).unwrap_or_else(|r| panic!("{r}"));
    let id = ProfileId("personal".into());
    let snap = set.profile(&id).unwrap();
    assert_eq!(snap.revision.0, GOLDEN_REVISION, "묶음 판본");
    assert_eq!(snap.policy_revision.0, GOLDEN_POLICY, "정책 판본");
    // 보관해 둔 묶음에서 다시 만들어도 같은 판본이다(재배포 뒤 복원)
    let bundle = raw_bundle(&files);
    assert_eq!(revision_of(&bundle), snap.revision);
    let restored = from_bundle(&bundle, &id, &snap.revision, &Limits::default())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(restored.policy_revision, snap.policy_revision);
}
