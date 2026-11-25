//! OpenAI 호환 입구로 접수된 실행의 일반 도구 승인: 승인자는 접수 때 기록한 값이 아니라 루트의 원래 API 바인딩·HTTP 주체로 고정·현재 판본에서 다시 읽는다.
//! 고정·현재가 같은 사람·경로를 가리킬 때만 카드를 열고, 지정된 사람의 확정한 영수증 카드를 누른 승인만 정확히 한 번 실행한다. 막으면 도구 실행 0이고 이유를
//! 호출 기록에 저장한다. 기존 요청자=소유자 전용 기능과 레거시 webhook은 이 경로가 아니다.

use super::*;
use crate::service::route::Route;
use bot_core::{
    api::{ApiOrigin, ApprovalBlock},
    cards::CardReceipt,
    ledger::ApprovalState,
    profile::{ApprovalRoute, ProfileCatalog},
    state::{Conversation, ConversationKind, RunOrigin},
};

const HTTP: &str = "http:openai/build";
const PERSON: &str = "slack:home/U123";
const DELETE: &str = "{\"event\":\"walk\"}";

fn slack(person: &str, binding: &str) -> ApprovalRoute {
    ApprovalRoute::Slack {
        person: PrincipalId(person.into()),
        binding: BindingId(binding.into()),
    }
}

fn local(person: &str) -> ApprovalRoute {
    ApprovalRoute::Local {
        person: PrincipalId(person.into()),
    }
}

/// HTTP 주체 `build`는 모든 도구 스코프를 가진 멤버이고, 모델 `assistant` 바인딩의 승인 경로가 `route`다.
fn world_at(revision: &str, route: Option<ApprovalRoute>) -> Arc<Snapshot> {
    world_with(revision, route, &[])
}

/// `world_at`에 정책·역할이 `extra` 스코프도 더 연 판본(스코프 때문에 막히지 않는다는 것을 보이려는 시험용).
fn world_with(revision: &str, route: Option<ApprovalRoute>, extra: &[&str]) -> Arc<Snapshot> {
    let approvers: Vec<(&str, ApprovalRoute)> = route.into_iter().map(|r| (HTTP, r)).collect();
    let scopes: Vec<&str> = ALL.iter().chain(extra).copied().collect();
    base(revision)
        // 판본이 바뀌어도 정책 판본은 같게 둔다: 승인 결합값의 정책 판본 불일치가 아니라 **승인 경로 판정**만이 낡은 승인을 막는다는 것을 보인다
        .policy_revision("p-openai")
        .role("owner", &scopes)
        .policy_scopes(&scopes)
        .member(HTTP, &["owner"])
        .member(PERSON, &["owner"])
        .member("slack:home/U999", &["owner"])
        .slack_channel_binding("team", "main")
        .slack_channel_binding("other", "main")
        .openai_binding("assistant", "main", "openai", "assistant", &approvers)
        .build()
}

/// 루트가 인증된 OpenAI 접수로 들어온 실행. 위임 자식도 같은 바인딩·출처를 물려받는다.
fn openai_run(run: &str) -> RunContext {
    let mut ctx = run_context("personal", run, 1, HTTP, "main", &ALL);
    ctx.origin = RunOrigin::Http;
    ctx.conversation = Conversation {
        kind: ConversationKind::Http,
        id: "post-1".into(),
        thread: None,
    };
    ctx.binding = BindingId("assistant".into());
    ctx.api = Some(ApiOrigin {
        endpoint: "openai".into(),
        model: "assistant".into(),
    });
    ctx.approver = None;
    ctx
}

/// 호스트 등록부: 워크스페이스 별칭 `home`의 채널 별칭 `planning`(전달 바인딩 `team`·`other`)이 지금 가리키는 팀. `previous`를 주면 그 팀의 항목을 먼저 뺀다
/// (재배포로 같은 별칭의 실제 team_id만 바뀐 것을 흉내 낸다: 프로필 판본·사람·바인딩은 그대로다).
fn host_team(w: &World, previous: Option<&str>, team: &str) {
    if let Some(old) = previous {
        w.routes.unregister(&format!("{old}/C1"));
    }
    for binding in ["team", "other"] {
        w.routes.register(
            "personal",
            binding,
            ConversationKind::Channel,
            "planning",
            &format!("{team}/C1"),
        );
    }
}

/// 접수 때(고정 판본 `r1`)의 경로가 `pinned`이고 지금 활성 판본은 `r1` 그대로다. 호스트의 Slack 설치는 `T1`이다.
fn world(pinned: Option<ApprovalRoute>) -> World {
    let w = World::new();
    w.publish(world_at("r1", pinned));
    w.ledger.add_run(openai_run("r1"));
    host_team(&w, None, "T1");
    w
}

fn receipt(owner: &str, binding: &str) -> CardReceipt {
    CardReceipt {
        team: "T1".into(),
        dm: "D1".into(),
        ts: "1700000000.000100".into(),
        owner: PrincipalId(owner.into()),
        profile: ProfileId("personal".into()),
        binding: BindingId(binding.into()),
    }
}

fn denied(result: Result<CallResult, Error>) {
    assert!(matches!(result, Err(Error::Denied)), "{result:?}");
}

fn refused_with(w: &World, word: &str) -> bool {
    w.audit
        .verdicts()
        .contains(&("call.deny".to_owned(), word.to_owned()))
}

#[test]
fn the_designated_slack_person_approves_an_http_principals_tool_exactly_once() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    let stored = w.ledger.stored_approval(&nonce);
    // 요청자는 HTTP 주체이고 승인자는 지정된 사람이다. 카드는 전달 바인딩의 설치로 간다
    assert_eq!(stored.approval.requester.0, HTTP);
    assert_eq!(stored.approval.approver.0, PERSON);
    assert_eq!(
        w.ledger.approval_delivery(&nonce),
        Some(BindingId("team".into()))
    );
    assert!(w.runner.jobs().is_empty(), "승인 전 실행 0건");
    // 게시 영수증이 확정되기 전의 카드는 아직 열리지 않았다: 지정된 사람이어도 누를 수 없다(미소비)
    assert!(matches!(
        w.decide(&nonce, PERSON, true),
        Err(Error::Conflict)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    // 지정된 사람이 아니면 영수증이 있어도 누를 수 없다: HTTP 키·다른 사람·소유자 모두
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    for actor in [HTTP, "slack:home/U999", "local:owner", "local:alice"] {
        assert!(
            matches!(w.decide(&nonce, actor, true), Err(Error::Denied)),
            "{actor}"
        );
    }
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    // 영수증이 다른 사람·다른 전달 바인딩의 것이면(복제 카드) 지정된 사람의 클릭도 거절한다
    for wrong in [receipt("slack:home/U999", "team"), receipt(PERSON, "other")] {
        w.ledger.post_receipt(&nonce, wrong);
        assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
        assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    }
    // 확정한 영수증의 카드를 지정된 사람이 누르면 그때만 승인된다
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.runner.jobs().len(), 0, "승인만으로는 실행하지 않는다");
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    // 실행한 호출의 재전달·다시 누르기는 한 번 더 실행하지 않는다
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    // HTTP 주체의 권한은 그대로다: 실행 요청자는 지정된 사람이 아니라 HTTP 주체다
    assert_eq!(w.runner.jobs()[0].principal.0, HTTP);
}

#[test]
fn a_local_route_needs_no_receipt_and_carries_no_delivery_binding() {
    let w = world(Some(local("local:owner")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.stored_approval(&nonce).approval.approver.0,
        "local:owner"
    );
    assert_eq!(w.ledger.approval_delivery(&nonce), None);
    assert!(matches!(w.decide(&nonce, HTTP, true), Err(Error::Denied)));
    w.decide(&nonce, "local:owner", true).unwrap();
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(
        w.routes.asked(),
        0,
        "관리 화면 승인은 Slack 설치를 보지 않는다"
    );
}

#[test]
fn a_tool_that_needs_no_approval_runs_without_any_route() {
    // 승인자 지정은 권한 상향이 아니고, 승인이 필요 없는 도구는 경로가 없어도 HTTP 주체의 권한으로 돈다
    let w = world(None);
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
}

#[test]
fn without_a_route_in_the_pinned_revision_nothing_runs_and_the_reason_is_stored() {
    // 접수 때(고정 판본)는 경로가 없었다: 지금 판본에 매핑을 넣어도 옛 요청의 승인을 얻을 수 없다
    let w = world(None);
    w.publish(world_at("r2", Some(slack(PERSON, "team"))));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::NoApprover)
    );
    assert!(
        w.ledger.pending_approval("r1", "c1").is_none(),
        "카드를 열지 않는다"
    );
    assert!(refused_with(&w, "no_approver"));
    // 같은 호출을 다시 불러도 같은 거절이고 실행은 없다
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_designation_withdrawn_while_a_card_waits_stales_the_old_button_and_blocks_the_call() {
    let w = world(Some(local("local:owner")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    // 매핑이 철회됐다(새 판본에는 경로가 없다)
    w.publish(world_at("r2", None));
    // 옛 버튼은 무효다: 승인되지 않고 카드는 닫힌다
    assert!(matches!(
        w.decide(&nonce, "local:owner", true),
        Err(Error::Stale)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    // 깨어난 run이 같은 호출로 돌아오면 도구 실행 없이 이유가 저장된 채 막힌다
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::ApproverRevoked)
    );
    assert!(refused_with(&w, "approver_revoked"));
}

#[test]
fn an_approved_card_whose_route_changed_before_consumption_never_runs() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    // 승인했지만 소비 전에 같은 사람의 전달 바인딩이 바뀌었다: 옛 승인으로는 실행하지 않는다
    w.publish(world_at("r2", Some(slack(PERSON, "other"))));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_ne!(w.ledger.approval_state(&nonce), ApprovalState::Consumed);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
}

#[test]
fn a_reassigned_person_blocks_the_call_and_the_old_person_cannot_approve() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.publish(world_at("r2", Some(slack("slack:home/U999", "team"))));
    // 옛 사람의 버튼은 무효이고, 새로 지정된 사람은 이 카드의 승인자가 아니다
    assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
    assert!(matches!(
        w.decide(&nonce, "slack:home/U999", true),
        Err(Error::Denied)
    ));
    denied(w.call("c1", "agenda.delete", DELETE));
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_legacy_webhook_with_the_same_name_gets_no_openai_approval_path() {
    // 접수 원장의 OpenAI 출처가 없는 실행(레거시 webhook)은 같은 이름의 HTTP 주체여도 이 경로가 아니다: 승인자가 없어 실행 0
    let w = world(Some(slack(PERSON, "team")));
    let mut legacy = openai_run("r1");
    legacy.api = None;
    w.ledger.add_run(legacy);
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert!(w.ledger.pending_approval("r1", "c1").is_none());
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
    assert!(refused_with(&w, "no_approver"));
}

#[test]
fn an_owner_only_function_stays_closed_to_an_openai_principal() {
    // 승인 경로가 있어도 기억 공유·사용자 예약 같은 요청자=소유자 전용 기능은 열리지 않는다(승인 카드도 없다)
    let w = World::new();
    // 스코프 때문에 막히는 것이 아님을 보이려고 HTTP 주체에게 `memory.share`까지 열어 둔다
    w.publish(world_with(
        "r1",
        Some(local("local:owner")),
        &["memory.share"],
    ));
    w.ledger.add_run(openai_run("r1"));
    w.memory.add(
        "m1",
        "local:owner",
        bot_core::state::Visibility::Private,
        "비밀",
    );
    denied(w.call("s1", "memory.share", "{\"id\":\"m1\"}"));
    assert!(w.memory.shared().is_empty());
    assert!(!w.ledger.has_call("r1", "s1"), "카드도 호출 기록도 없다");
    assert_eq!(w.ledger.blocked_why("r1", "s1"), None);
}

#[test]
fn a_delegated_child_uses_the_roots_original_binding_and_origin() {
    let w = world(Some(slack(PERSON, "team")));
    let mut child = openai_run("r2");
    child.root = RunId("r1".into());
    child.agent = AgentId("main".into());
    w.ledger.add_run(child);
    let nonce = awaiting(w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE));
    let stored = w.ledger.stored_approval(&nonce);
    assert_eq!(stored.approval.approver.0, PERSON);
    assert_eq!(
        w.ledger.approval_delivery(&nonce),
        Some(BindingId("team".into()))
    );
    // 같은 매핑이 철회되면 자식도 막힌다
    w.publish(world_at("r2", None));
    assert!(matches!(
        w.call_on(&w.broker, "r2", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Denied)
    ));
    assert_eq!(
        w.ledger.blocked_why("r2", "c1"),
        Some(ApprovalBlock::ApproverRevoked)
    );
    assert!(w.runner.jobs().is_empty());
}

/// 슬랙 승인 카드를 게시 영수증까지 확정하고 지정된 사람이 승인한 상태(`T1` 설치)로 만든다.
fn approved_in_t1(w: &World) -> ApprovalId {
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    w.decide(&nonce, PERSON, true).unwrap();
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    nonce
}

#[test]
fn a_redeploy_that_only_moves_the_alias_to_another_team_never_consumes_an_approved_card() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    // 재배포: 같은 별칭 `home`의 실제 팀만 T2가 됐다. 프로필 판본·지정된 사람·전달 바인딩은 그대로라 판본 비교로는 같다
    host_team(&w, Some("T1"), "T2");
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty(), "도구 실행 0");
    assert_eq!(
        w.ledger.approval_state(&nonce),
        ApprovalState::Approved,
        "승인 소비 0"
    );
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
    assert!(refused_with(&w, "route_changed"));
    // 같은 호출은 다시 불러도 같은 거절이고 실행은 없다
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_card_posted_to_the_previous_team_cannot_be_approved_after_the_alias_moved() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    host_team(&w, Some("T1"), "T2");
    // 카드가 올라간 설치가 지금 호스트의 설치와 다르다: 옛 버튼은 무효이고 승인은 받지 않는다
    assert!(matches!(w.decide(&nonce, PERSON, true), Err(Error::Stale)));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Superseded);
    // 깨어난 run이 같은 호출로 돌아오면 옛 카드가 아니라 새 카드를 연다(실행 0)
    let again = awaiting(w.call("c1", "agenda.delete", DELETE));
    assert_ne!(again, nonce);
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn an_unreachable_host_registry_confirms_no_block_and_the_approval_waits() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = awaiting(w.call("c1", "agenda.delete", DELETE));
    w.ledger.post_receipt(&nonce, receipt(PERSON, "team"));
    // 확인하지 못하는 동안은 클릭도 소비도 확정하지 않는다(막지 않고 다시 시도하게 한다)
    w.routes.outage(true);
    assert!(matches!(
        w.decide(&nonce, PERSON, true),
        Err(Error::Unavailable)
    ));
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Pending);
    w.routes.outage(false);
    w.decide(&nonce, PERSON, true).unwrap();
    w.routes.outage(true);
    assert!(matches!(
        w.call("c1", "agenda.delete", DELETE),
        Err(Error::Unavailable)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        None,
        "일시 오류로 막지 않는다"
    );
    // 확인이 돌아오면 같은 승인으로 정확히 한 번 실행한다
    w.routes.outage(false);
    assert_eq!(done(w.call("c1", "agenda.delete", DELETE)), "{\"ok\":true}");
    assert_eq!(w.runner.jobs().len(), 1);
}

#[test]
fn a_broker_without_a_host_registry_never_consumes_a_slack_card() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    // 등록부를 붙이지 않은 브로커는 카드가 올라간 설치를 확인할 수 없다: 소비하지도 막지도 않는다
    let bare = w.bare();
    assert!(matches!(
        w.call_on(&bare, "r1", 1, "c1", "agenda.delete", DELETE),
        Err(Error::Unavailable)
    ));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(w.ledger.blocked_why("r1", "c1"), None);
}

#[test]
fn an_approved_slack_card_without_a_confirmed_receipt_is_never_consumed() {
    let w = world(Some(slack(PERSON, "team")));
    let nonce = approved_in_t1(&w);
    // 카드가 어느 설치에 올라갔는지 모르면 닫힌 채 막는다
    w.ledger.clear_receipt(&nonce);
    denied(w.call("c1", "agenda.delete", DELETE));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(w.ledger.approval_state(&nonce), ApprovalState::Approved);
    assert_eq!(
        w.ledger.blocked_why("r1", "c1"),
        Some(ApprovalBlock::RouteChanged)
    );
}

#[test]
fn the_current_site_is_read_through_the_registry_alias_of_the_delivery_binding() {
    let w = world(Some(slack(PERSON, "team")));
    let profile = ProfileId("personal".into());
    let snapshot = w.catalog.current(&profile).unwrap();
    let site = |binding: &str, posted: &str| {
        w.broker
            .site_state(&profile, &snapshot, &BindingId(binding.into()), posted)
    };
    assert_eq!(site("team", "T1"), Route::Same);
    assert_eq!(site("team", "T2"), Route::Gone, "다른 팀");
    assert_eq!(
        site("assistant", "T1"),
        Route::Gone,
        "Slack 채널 바인딩이 아니다"
    );
    assert_eq!(site("missing", "T1"), Route::Gone, "없는 바인딩");
    // 등록부가 채널 별칭을 모르면(재지정·삭제) 설치를 알 수 없다
    w.routes.unregister("T1/C1");
    assert_eq!(site("team", "T1"), Route::Gone, "별칭이 등록부에 없다");
    // 정규 경로(팀/채널)가 아닌 값에서는 팀을 읽을 수 없다
    w.routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "planning",
        "T1",
    );
    assert_eq!(site("team", "T1"), Route::Gone);
    // 일시 오류·등록부 없음은 확정하지 않는다
    w.routes.outage(true);
    assert_eq!(site("team", "T1"), Route::Unsure);
    assert_eq!(
        w.bare()
            .site_state(&profile, &snapshot, &BindingId("team".into()), "T1"),
        Route::Unsure
    );
}
