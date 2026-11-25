//! OpenAI 모델 바인딩의 인간 승인 경로(`Snapshot::approval_route`): 바인딩·요청자별 조회, 스냅숏이 바뀌었을 때의 닫힘, 보관한 묶음에서의 복원.
mod support;

use bot_core::{
    id::{BindingId, PrincipalId, ProfileId},
    profile::{Adapter, ApprovalRoute, Snapshot},
};
use bot_profile::{Limits, from_bundle};
use support::openai::{LOCAL, SLACK, models, ok, route, with_people};
use support::raw_bundle;

#[test]
fn approval_routes_resolve_per_binding_and_requester() {
    let files = with_people(models(SLACK));
    let snap = ok(&files);
    let slack = ApprovalRoute::Slack {
        person: PrincipalId("slack:home/U1".into()),
        binding: BindingId("team".into()),
    };
    assert_eq!(route(&snap, "assistant", "http:openai/build"), Some(slack));
    // 매핑은 바인딩마다 독립이다: 같은 요청자도 매핑이 없는 모델에서는 승인자가 없다
    assert_eq!(route(&snap, "research", "http:openai/build"), None);
    // 멤버가 아니거나 매핑이 없는 주체, 웹훅·CLI 바인딩, 없는 바인딩
    assert_eq!(route(&snap, "assistant", "http:openai/ops"), None);
    assert_eq!(route(&snap, "inbox", "http:openai/build"), None);
    assert_eq!(route(&snap, "desk", "local:owner"), None);
    assert_eq!(route(&snap, "nowhere", "http:openai/build"), None);
    // 사람이 요청자인 척해도 HTTP 주체가 아니면 열리지 않는다
    assert_eq!(route(&snap, "assistant", "local:owner"), None);
    let local = ok(&models(LOCAL));
    assert_eq!(
        route(&local, "assistant", "http:openai/build"),
        Some(ApprovalRoute::Local {
            person: PrincipalId("local:owner".into())
        })
    );
}

#[test]
fn approval_routes_fail_closed_when_a_snapshot_is_altered() {
    let snap = ok(&with_people(models(SLACK)));
    let (build, person) = (
        PrincipalId("http:openai/build".into()),
        PrincipalId("slack:home/U1".into()),
    );
    let asks = |s: &Snapshot| s.approval_route(&BindingId("assistant".into()), &build);
    assert!(asks(&snap).is_some());
    let mut gone = snap.clone();
    gone.members.remove(&person);
    assert_eq!(asks(&gone), None, "승인자가 멤버에서 빠짐");
    let mut left = snap.clone();
    left.members.remove(&build);
    assert_eq!(asks(&left), None, "요청자가 멤버에서 빠짐");
    let mut moved = snap.clone();
    moved
        .bindings
        .get_mut(&BindingId("team".into()))
        .unwrap()
        .source = Adapter::SlackChannel {
        workspace: "other".into(),
        channel: "lobby".into(),
        events: bot_core::slack::default_channel_events(),
    };
    assert_eq!(
        asks(&moved),
        None,
        "전달 바인딩이 다른 워크스페이스로 옮겨 감"
    );
    let mut dm = snap.clone();
    dm.bindings
        .get_mut(&BindingId("team".into()))
        .unwrap()
        .source = Adapter::SlackDm {
        workspace: "home".into(),
    };
    assert_eq!(asks(&dm), None, "전달 바인딩이 DM");
    let mut removed = snap.clone();
    removed.bindings.remove(&BindingId("team".into()));
    assert_eq!(asks(&removed), None, "전달 바인딩 삭제");
    let mut rebased = snap.clone();
    rebased
        .bindings
        .get_mut(&BindingId("assistant".into()))
        .unwrap()
        .source = Adapter::Http {
        endpoint: "reminders".into(),
    };
    assert_eq!(asks(&rebased), None, "모델 바인딩이 웹훅으로 바뀜");
    // 승인자가 사람이 아니거나, 경로 종류와 신원 종류가 맞지 않으면 열리지 않는다
    let rerouted = |route: ApprovalRoute| {
        let mut changed = snap.clone();
        let Adapter::OpenAi { approvers, .. } = &mut changed
            .bindings
            .get_mut(&BindingId("assistant".into()))
            .unwrap()
            .source
        else {
            unreachable!()
        };
        approvers.insert(build.clone(), route);
        changed
    };
    let people = |id: &str| PrincipalId(id.into());
    let service = ApprovalRoute::Local {
        person: people("service:morning"),
    };
    assert_eq!(
        asks(&rerouted(service)),
        None,
        "서비스 주체는 승인할 수 없다"
    );
    let local_with_slack_person = ApprovalRoute::Local {
        person: people("slack:home/U1"),
    };
    assert_eq!(
        asks(&rerouted(local_with_slack_person)),
        None,
        "로컬 경로에 Slack 신원"
    );
    let slack_with_local_person = ApprovalRoute::Slack {
        person: people("local:owner"),
        binding: BindingId("team".into()),
    };
    assert_eq!(
        asks(&rerouted(slack_with_local_person)),
        None,
        "Slack 경로에 로컬 신원"
    );
    let mut odd = rerouted(ApprovalRoute::Slack {
        person: people("slack:home/u1"),
        binding: BindingId("team".into()),
    });
    odd.members
        .insert(people("slack:home/u1"), Default::default());
    assert_eq!(asks(&odd), None, "신원 문법이 맞지 않는 승인자");
    // 매핑 키가 다른 별칭의 주체이면 그 입구에서는 열리지 않는다
    let mut other = snap.clone();
    other
        .bindings
        .get_mut(&BindingId("assistant".into()))
        .unwrap()
        .source = Adapter::OpenAi {
        endpoint: "second".into(),
        model: "assistant".into(),
        approvers: [(
            build.clone(),
            ApprovalRoute::Local {
                person: PrincipalId("local:owner".into()),
            },
        )]
        .into(),
    };
    assert_eq!(asks(&other), None, "요청자 별칭과 바인딩 별칭이 다름");
}

#[test]
fn model_profiles_restore_from_their_stored_bundle() {
    for files in [models(""), with_people(models(SLACK)), models(LOCAL)] {
        let snap = ok(&files);
        let bundle = raw_bundle(&files);
        let id = ProfileId("personal".into());
        let back = from_bundle(&bundle, &id, &snap.revision, &Limits::default())
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(back.revision, snap.revision);
        assert_eq!(back.policy_revision, snap.policy_revision);
        assert_eq!(back.bindings.len(), snap.bindings.len());
        let asks = |s: &Snapshot| route(s, "assistant", "http:openai/build");
        assert_eq!(asks(&back), asks(&snap));
    }
}
