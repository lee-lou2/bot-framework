//! HTTP(OpenAI 호환) 출처 승인 카드의 전달 전 대조(`CardDeliverer`, 가짜 Slack·메모리 저장소): 접수 때 고정한 승인 경로·전달 설치(팀)가 지금과 같을 때만 올리고,
//! 어긋나면 올리지 않고 저장된 판정(`approver_revoked`·`route_changed`)으로 닫는다. 증거를 읽지 못하면 올리지 않고, 증거가 있는 카드는 채널 출처 경로로 되돌아가지 않는다.
mod delivery_support;
mod support;

use bot_core::{
    api::ApprovalBlock,
    cards::{CardJob, CardKey, CardOrigin, CardOutcome},
    command::JsonPayload,
    id::{ApprovalId, BindingId, Generation, PrincipalId, ProfileId, Revision},
    profile::{Adapter, ApprovalRoute},
};
use bot_gateway::testing::{cards::MemoryCards, *};
use delivery_support::*;
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

/// 모델 `assistant`(OpenAI)가 있고 승인자가 Slack 채널 바인딩 `team`으로 정해진 프로필(판본 `rev`)
fn api_profile(rev: &str, route: Option<ApprovalRoute>, approver_member: bool) -> Fixture {
    api_profile_as(rev, route, approver_member, "assistant", "main")
}

/// 같은 바인딩 `assistant`가 공개 모델 `model`과 에이전트 `agent`를 가리키는 판본
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
            json!({"nonce": nonce, "title": "도구 승인", "lines": ["메일 보내기"]}).to_string(),
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
    // 접수 때는 T1이었는데 재배포로 같은 워크스페이스 별칭이 다른 팀(T2)을 가리킨다: 프로필·사람·바인딩은 그대로다
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
    assert_eq!(opens(&rig), 0, "새 팀에 DM을 열지도 않는다");
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
    // 고정 판본 r1과 지금의 r2를 모두 가진 조립: 고정 판본을 복원할 수 있다
    let pinned = || api_profile("personal-r1", Some(slack(APPROVER, "team")), true);
    let cases: Vec<(&str, Fixture, ApprovalBlock)> = vec![
        (
            "승인 지정 철회",
            api_profile("personal-r2", None, true),
            ApprovalBlock::ApproverRevoked,
        ),
        (
            "지정된 사람의 멤버 자격 철회",
            api_profile("personal-r2", Some(slack(APPROVER, "team")), false),
            ApprovalBlock::ApproverRevoked,
        ),
        (
            "다른 사람으로 재지정",
            api_profile("personal-r2", Some(slack("slack:home/U2", "team")), true)
                .member("slack:home/U2", &["owner"]),
            ApprovalBlock::RouteChanged,
        ),
        (
            "로컬 승인으로 재지정",
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
            "전달 바인딩 재지정",
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
    // 판본이 바뀌어도(r1 → r2) 승인 경로와 설치가 같으면 올린다
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
    assert!(cards.blocked().is_empty(), "저장소가 받지 못했다");
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
    // 바인딩 id·승인자·전달 바인딩·팀은 그대로인데 같은 바인딩이 다른 공개 모델이나 다른 에이전트를 가리키게 재배포됐다: 카드의 옛 도구 인자는 새 대상의 것이 아니다
    let pinned = || api_profile("personal-r1", Some(slack(APPROVER, "team")), true);
    let cases: Vec<(&str, Fixture)> = vec![
        (
            "모델 이름만 바뀜",
            api_profile_as(
                "personal-r2",
                Some(slack(APPROVER, "team")),
                true,
                "assistant-2",
                "main",
            ),
        ),
        (
            "에이전트만 바뀜",
            api_profile_as(
                "personal-r2",
                Some(slack(APPROVER, "team")),
                true,
                "assistant",
                "side",
            ),
        ),
        (
            "둘 다 바뀜",
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
        assert_eq!(opens(&rig), 0, "{name}: DM을 열지도 않는다");
        assert_eq!(rig.slack.calls("chat.postMessage").len(), 0, "{name}");
        assert!(cards.begun().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_publication_between_the_claim_and_the_post_leaves_nothing_posted() {
    // 점유해 판정까지 통과한 뒤, 올리기 전에 다른 프로세스가 같은 승인자·같은 팀인데 모델만 재지정한 새 집합을 게시한다.
    // `begin`은 확인한 판본이 지금의 활성 판본인지 다시 보므로 `Stale`이고, 카드는 올라가지 않는다. 다음에 다시 점유하면 판정이 막는다.
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
        "게시 사이에 끼어든 재배포인데 카드가 올라갔어요"
    );
    assert!(cards.begun().is_empty() && cards.settled().is_empty());
    // 이어서 같은 카드를 다시 점유하면(리스가 끝난 뒤) 새 판본의 판정이 막는다
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
    // 접수 때 고정한 판본(r1)은 승인자를 U2로 정했는데 카드는 U1 앞으로 와 있다(지금 판본 r2는 U1이다): 카드가 가리키는 사람이 고정 판본의 경로가 아니다
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
