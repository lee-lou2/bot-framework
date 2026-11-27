//! Golden revision stability test ensuring webhook-only profiles keep identical bundle and policy hashes.
//! If either hash changes, fix the implementation rather than updating these values—changing them breaks bundle restore and in-flight approvals.

use crate::support::{Files, check, raw_bundle};
use bot_core::id::ProfileId;
use bot_profile::{Limits, from_bundle, revision_of};

const GOLDEN_REVISION: &str = "b96865887bcece8aa67c2040454322ef4f3b4745ce107285976c6acda38014d5";
const GOLDEN_POLICY: &str = "28413cb02691238dd12273e2b785161a6a4e80db9089b0769ee422266e6f068f";

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
    target: {instruction: "Summarize."}
"#;
    [
        ("profile.yaml", profile),
        ("agents/main/AGENT.md", "Helps the user.\n"),
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
    assert_eq!(snap.revision.0, GOLDEN_REVISION, "bundle revision");
    assert_eq!(snap.policy_revision.0, GOLDEN_POLICY, "policy revision");
    let bundle = raw_bundle(&files);
    assert_eq!(revision_of(&bundle), snap.revision);
    let restored = from_bundle(&bundle, &id, &snap.revision, &Limits::default())
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(restored.policy_revision, snap.policy_revision);
}
