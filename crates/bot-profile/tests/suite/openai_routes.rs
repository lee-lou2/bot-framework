//! Tests for OpenAI model binding approval routes (`Snapshot::approval_route`): lookup, fail-closed mutations, and bundle restore.

use crate::support::openai::{LOCAL, SLACK, models, ok, route, with_people};
use crate::support::raw_bundle;
use bot_core::{
    id::{BindingId, PrincipalId, ProfileId},
    profile::{Adapter, ApprovalRoute, Snapshot},
};
use bot_profile::{Limits, from_bundle};

#[test]
fn approval_routes_resolve_per_binding_and_requester() {
    let files = with_people(models(SLACK));
    let snap = ok(&files);
    let slack = ApprovalRoute::Slack {
        person: PrincipalId("slack:home/U1".into()),
        binding: BindingId("team".into()),
    };
    assert_eq!(route(&snap, "assistant", "http:openai/build"), Some(slack));
    assert_eq!(route(&snap, "research", "http:openai/build"), None);
    assert_eq!(route(&snap, "assistant", "http:openai/ops"), None);
    assert_eq!(route(&snap, "inbox", "http:openai/build"), None);
    assert_eq!(route(&snap, "desk", "local:owner"), None);
    assert_eq!(route(&snap, "nowhere", "http:openai/build"), None);
    // Non-HTTP requesters cannot resolve OpenAI approval routes.
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
    assert_eq!(asks(&gone), None, "approver removed from members");
    let mut left = snap.clone();
    left.members.remove(&build);
    assert_eq!(asks(&left), None, "requester removed from members");
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
        "delivery binding moved to another workspace"
    );
    let mut dm = snap.clone();
    dm.bindings
        .get_mut(&BindingId("team".into()))
        .unwrap()
        .source = Adapter::SlackDm {
        workspace: "home".into(),
    };
    assert_eq!(asks(&dm), None, "delivery binding changed to DM");
    let mut removed = snap.clone();
    removed.bindings.remove(&BindingId("team".into()));
    assert_eq!(asks(&removed), None, "delivery binding removed");
    let mut rebased = snap.clone();
    rebased
        .bindings
        .get_mut(&BindingId("assistant".into()))
        .unwrap()
        .source = Adapter::Http {
        endpoint: "reminders".into(),
    };
    assert_eq!(asks(&rebased), None, "model binding changed to webhook");
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
        "service principal cannot approve"
    );
    let local_with_slack_person = ApprovalRoute::Local {
        person: people("slack:home/U1"),
    };
    assert_eq!(
        asks(&rerouted(local_with_slack_person)),
        None,
        "Slack identity on local route"
    );
    let slack_with_local_person = ApprovalRoute::Slack {
        person: people("local:owner"),
        binding: BindingId("team".into()),
    };
    assert_eq!(
        asks(&rerouted(slack_with_local_person)),
        None,
        "local identity on Slack route"
    );
    let mut odd = rerouted(ApprovalRoute::Slack {
        person: people("slack:home/u1"),
        binding: BindingId("team".into()),
    });
    odd.members
        .insert(people("slack:home/u1"), Default::default());
    assert_eq!(asks(&odd), None, "malformed approver identity");
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
    assert_eq!(asks(&other), None, "requester endpoint mismatch");
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
