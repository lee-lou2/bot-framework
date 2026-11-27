//! Pre-send verification for HTTP (OpenAI-compatible) approval cards against pinned origin and team.
use crate::delivery_support::*;
use crate::support;
use bot_core::{
    api::ApprovalBlock,
    cards::{CardJob, CardKey, CardOrigin, CardOutcome},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId, Revision},
    profile::{Adapter, ApprovalRoute},
};
use bot_gateway::testing::{cards::MemoryCards, *};
use serde_json::json;
use std::collections::BTreeMap;

const REQUESTER: &str = "http:openai/build";
const APPROVER: &str = "slack:home/U1";

fn host(team: &str) -> String {
    format!(
        r#"{{"slack": {{"workspaces": {{"home": {{"team_id": "{team}", "dm": true, "bot_token": "bot-token",
            "app_token": "app-token", "channels": {{"planning": "C1", "ops": "C2"}}}}}}}},
            "http": {{"endpoints": {{"openai": {{"kind": "openai", "keys": {{"build": {{"secret": "build-key", "subject": "build"}}}}}}}}}},
            "admin_url": "https://admin.example/approvals"}}"#
    )
}

fn approvers(route: Option<ApprovalRoute>) -> BTreeMap<PrincipalId, ApprovalRoute> {
    route
        .map(|route| (PrincipalId(REQUESTER.into()), route))
        .into_iter()
        .collect()
}

fn slack(person: &str, binding: &str) -> ApprovalRoute {
    ApprovalRoute::Slack {
        person: PrincipalId(person.into()),
        binding: BindingId(binding.into()),
    }
}

fn api_profile(rev: &str, route: Option<ApprovalRoute>, approver_member: bool) -> Fixture {
    api_profile_as(rev, route, approver_member, "assistant", "main")
}

fn api_profile_as(
    rev: &str,
    route: Option<ApprovalRoute>,
    approver_member: bool,
    model: &str,
    agent: &str,
) -> Fixture {
    let mut fixture = profile("personal")
        .agent("side", &["run"])
        .revision(rev)
        .binding(
            "team",
            Adapter::SlackChannel {
                workspace: "home".into(),
                channel: "planning".into(),
                events: bot_core::slack::default_channel_events(),
            },
            "main",
        )
        .binding(
            "assistant",
            Adapter::OpenAi {
                endpoint: "openai".into(),
                model: model.into(),
                approvers: approvers(route),
            },
            agent,
        )
        .member(REQUESTER, &["runner"])
        .role("runner", &["run"])
        .policy_scopes(&["run"]);
    if approver_member {
        fixture = fixture.member(APPROVER, &["owner"]).role("owner", &["run"]);
    }
    fixture
}

fn job(nonce: &str, binding: &str, owner: &str) -> CardJob {
    CardJob {
        key: CardKey {
            nonce: ApprovalId(nonce.into()),
            attempt: 0,
        },
        profile: ProfileId("personal".into()),
        binding: BindingId(binding.into()),
        owner: PrincipalId(owner.into()),
        card: JsonPayload(
            json!({"nonce": nonce, "title": "Tool approval", "lines": ["Send email"]}).to_string(),
        ),
        generation: Generation(1),
    }
}

fn origin(rev: &str, team: Option<&str>) -> CardOrigin {
    CardOrigin {
        binding: BindingId("assistant".into()),
        requester: PrincipalId(REQUESTER.into()),
        revision: Revision(rev.into()),
        team: team.map(str::to_owned),
    }
}

fn opens(rig: &Rig) -> usize {
    rig.slack.calls("conversations.open").len()
}

async fn rig_for(team: &str, current: Fixture) -> Rig {
    rig_on(&host(team), current, support::default_secrets(), None).await
}

#[tokio::test]
async fn a_card_whose_route_and_installation_are_unchanged_is_posted_once() {
    let rig = rig_for(
        "T1",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    let deliverer = rig.card_deliverer(cards.clone());
    assert_eq!(deliverer.tick().await.unwrap(), 1);
    assert_eq!(
        cards.begun(),
        [("n-1".to_owned(), "personal-r1".to_owned())]
    );
    assert_eq!(rig.slack.calls("chat.postMessage").len(), 1);
    assert!(matches!(
        cards.settled().as_slice(),
        [(_, CardOutcome::Posted { .. })]
    ));
    assert!(cards.blocked().is_empty());
    assert_eq!(deliverer.tick().await.unwrap(), 0);
    assert_eq!(rig.slack.calls("chat.postMessage").len(), 1);
}

#[tokio::test]
async fn the_same_alias_pointing_at_another_team_closes_the_card_without_posting() {
    // Same workspace alias now resolves to T2 instead of pinned T1.
    let rig = rig_for(
        "T2",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(opens(&rig), 0, "must not open DM on new team");
    assert!(cards.begun().is_empty() && cards.settled().is_empty());
    assert_eq!(rig.slack.calls("chat.postMessage").len(), 0);
}

#[tokio::test]
async fn an_http_origin_without_a_recorded_team_never_falls_back_to_the_channel_path() {
    let rig = rig_for(
        "T1",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", None));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(opens(&rig), 0);
}

#[tokio::test]
async fn the_reason_follows_what_changed_between_the_pinned_revision_and_now() {
    let pinned = || api_profile("personal-r1", Some(slack(APPROVER, "team")), true);
    let cases: Vec<(&str, Fixture, ApprovalBlock)> = vec![
        (
            "approver route removed",
            api_profile("personal-r2", None, true),
            ApprovalBlock::ApproverRevoked,
        ),
        (
            "approver membership revoked",
            api_profile("personal-r2", Some(slack(APPROVER, "team")), false),
            ApprovalBlock::ApproverRevoked,
        ),
        (
            "reassigned to another user",
            api_profile("personal-r2", Some(slack("slack:home/U2", "team")), true)
                .member("slack:home/U2", &["owner"]),
            ApprovalBlock::RouteChanged,
        ),
        (
            "reassigned to local approval",
            api_profile(
                "personal-r2",
                Some(ApprovalRoute::Local {
                    person: PrincipalId("local:owner".into()),
                }),
                true,
            )
            .member("local:owner", &["owner"]),
            ApprovalBlock::RouteChanged,
        ),
        (
            "delivery binding reassigned",
            api_profile("personal-r2", Some(slack(APPROVER, "ops-channel")), true).binding(
                "ops-channel",
                Adapter::SlackChannel {
                    workspace: "home".into(),
                    channel: "ops".into(),
                    events: bot_core::slack::default_channel_events(),
                },
                "main",
            ),
            ApprovalBlock::RouteChanged,
        ),
    ];
    for (name, now, expected) in cases {
        let rig = rig_for("T1", now).await;
        rig.catalog.remember([pinned().build()]);
        let cards = MemoryCards::new();
        cards.push(job("n-1", "team", APPROVER));
        cards.set_origin("n-1", origin("personal-r1", Some("T1")));
        rig.card_deliverer(cards.clone()).tick().await.unwrap();
        assert_eq!(cards.blocked(), [("n-1".to_owned(), expected)], "{name}");
        assert_eq!(opens(&rig), 0, "{name}");
        assert!(cards.begun().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_pinned_revision_that_cannot_be_restored_cannot_be_compared_so_the_card_is_closed() {
    let rig = rig_for(
        "T1",
        api_profile("personal-r2", Some(slack(APPROVER, "team")), true),
    )
    .await;
    rig.catalog.strict(true);
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(opens(&rig), 0);
}

#[tokio::test]
async fn the_same_route_in_a_newer_revision_still_posts() {
    let rig = rig_for(
        "T1",
        api_profile("personal-r2", Some(slack(APPROVER, "team")), true),
    )
    .await;
    rig.catalog
        .remember([api_profile("personal-r1", Some(slack(APPROVER, "team")), true).build()]);
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.begun(),
        [("n-1".to_owned(), "personal-r2".to_owned())]
    );
    assert_eq!(rig.slack.calls("chat.postMessage").len(), 1);
}

#[tokio::test]
async fn unreadable_evidence_posts_nothing_and_leaves_the_job_for_the_next_lease() {
    let rig = rig_for(
        "T1",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    cards.fail_origin(true);
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(opens(&rig), 0);
    assert!(cards.begun().is_empty() && cards.settled().is_empty() && cards.blocked().is_empty());
}

#[tokio::test]
async fn a_close_the_store_cannot_record_yet_is_retried_with_the_same_verdict_and_never_posts() {
    let rig = rig_for(
        "T2",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    cards.fail_settle([bot_core::port::Error::Unavailable]);
    let deliverer = rig.card_deliverer(cards.clone());
    deliverer.tick().await.unwrap();
    assert!(cards.blocked().is_empty(), "store rejected settle");
    deliverer.tick().await.unwrap();
    assert_eq!(
        cards.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(opens(&rig), 0);
}

#[tokio::test]
async fn a_reassigned_model_or_agent_closes_the_card_even_when_route_approver_and_team_are_unchanged()
 {
    // Reassigning model or agent on the same binding invalidates pending tool args.
    let pinned = || api_profile("personal-r1", Some(slack(APPROVER, "team")), true);
    let cases: Vec<(&str, Fixture)> = vec![
        (
            "model renamed",
            api_profile_as(
                "personal-r2",
                Some(slack(APPROVER, "team")),
                true,
                "assistant-2",
                "main",
            ),
        ),
        (
            "agent changed",
            api_profile_as(
                "personal-r2",
                Some(slack(APPROVER, "team")),
                true,
                "assistant",
                "side",
            ),
        ),
        (
            "both changed",
            api_profile_as(
                "personal-r2",
                Some(slack(APPROVER, "team")),
                true,
                "assistant-2",
                "side",
            ),
        ),
    ];
    for (name, now) in cases {
        let rig = rig_for("T1", now).await;
        rig.catalog.remember([pinned().build()]);
        let cards = MemoryCards::new();
        cards.push(job("n-1", "team", APPROVER));
        cards.set_origin("n-1", origin("personal-r1", Some("T1")));
        rig.card_deliverer(cards.clone()).tick().await.unwrap();
        assert_eq!(
            cards.blocked(),
            [("n-1".to_owned(), ApprovalBlock::RouteChanged)],
            "{name}"
        );
        assert_eq!(opens(&rig), 0, "{name}: must not open DM");
        assert_eq!(rig.slack.calls("chat.postMessage").len(), 0, "{name}");
        assert!(cards.begun().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_publication_between_the_claim_and_the_post_leaves_nothing_posted() {
    // Concurrent publication before `begin` makes the checked revision stale and prevents posting.
    let rig = rig_for(
        "T1",
        api_profile("personal-r1", Some(slack(APPROVER, "team")), true),
    )
    .await;
    let cards = MemoryCards::new();
    cards.fence(rig.catalog.clone());
    let catalog = rig.catalog.clone();
    cards.before_begin(move || {
        catalog.replace([api_profile_as(
            "personal-r2",
            Some(slack(APPROVER, "team")),
            true,
            "assistant-2",
            "main",
        )
        .build()]);
    });
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.catalog
        .remember([api_profile("personal-r1", Some(slack(APPROVER, "team")), true).build()]);
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        rig.slack.calls("chat.postMessage").len(),
        0,
        "card must not be posted after concurrent redeployment"
    );
    assert!(cards.begun().is_empty() && cards.settled().is_empty());
    // Next claim checks against the new revision and blocks the card.
    let again = MemoryCards::new();
    again.push(job("n-1", "team", APPROVER));
    again.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.card_deliverer(again.clone()).tick().await.unwrap();
    assert_eq!(
        again.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(rig.slack.calls("chat.postMessage").len(), 0);
}

#[tokio::test]
async fn a_card_issued_for_someone_the_pinned_revision_never_named_is_closed_even_if_today_it_matches()
 {
    // Pinned r1 named U2 rather than U1, even though current r2 names U1.
    let now = api_profile("personal-r2", Some(slack(APPROVER, "team")), true);
    let rig = rig_for("T1", now).await;
    rig.catalog.remember([
        api_profile("personal-r1", Some(slack("slack:home/U2", "team")), true)
            .member("slack:home/U2", &["owner"])
            .build(),
    ]);
    let cards = MemoryCards::new();
    cards.push(job("n-1", "team", APPROVER));
    cards.set_origin("n-1", origin("personal-r1", Some("T1")));
    rig.card_deliverer(cards.clone()).tick().await.unwrap();
    assert_eq!(
        cards.blocked(),
        [("n-1".to_owned(), ApprovalBlock::RouteChanged)]
    );
    assert_eq!(opens(&rig), 0);
}
