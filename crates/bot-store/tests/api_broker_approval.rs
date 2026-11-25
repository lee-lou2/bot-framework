//! OpenAI 호환 입구로 접수된 실행의 일반 도구 승인 전체 흐름(실제 브로커 + 실제 PG 접수 원장·호출 기록·카드 작업). 승인자는 접수 때 기록한 값이 아니라 원래 바인딩·HTTP 주체로
//! 고정·현재 판본에서 다시 읽고, 지정된 사람이 게시 영수증이 확정된 카드를 누른 승인만 정확히 한 번 실행한다. 막으면 도구 실행 0이고 저장된 이유를 조회가 읽는다.

mod support;

use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, MemRoutes, RecordingTracer, snapshot, tool},
};
use bot_core::{
    api::{ApiState, ApprovalBlock},
    cards::{CardOutbox, CardOutcome},
    command::{CallResult, Delegate, JsonPayload, ToolCall},
    id::*,
    port::{Broker, DelegationStore, Error},
    profile::{ActiveRevisions, ApprovalRoute, Snapshot},
    state::{ConversationKind, Risk, RunContext},
};
use std::{collections::BTreeMap, sync::Arc};
use support::{api::*, *};

const HTTP: &str = "http:openai/build";
const PERSON: &str = "slack:home/U123";
const GRANTS: [&str; 2] = ["run", "agenda.delete"];

fn slack(person: &str, binding: &str) -> ApprovalRoute {
    ApprovalRoute::Slack {
        person: PrincipalId(person.into()),
        binding: BindingId(binding.into()),
    }
}

/// 모델 `assistant`(원래 바인딩 `b-assistant`)가 에이전트 `main`으로 간다. HTTP 주체 `build`가 `agenda.delete`(승인이 필요한 도구)를 쓸 수 있고, `route`가 승인 경로다.
/// 정책 판본은 판본이 바뀌어도 같게 둔다: 낡은 승인을 막는 것이 정책 판본 불일치가 아니라 승인 경로 판정임을 보인다.
fn world(revision: &str, route: Option<ApprovalRoute>) -> Arc<Snapshot> {
    let approvers: Vec<(&str, ApprovalRoute)> = route.into_iter().map(|r| (HTTP, r)).collect();
    snapshot("personal", revision)
        .policy_revision("p-openai")
        .role("owner", &GRANTS)
        .member(HTTP, &["owner"])
        .member(PERSON, &["owner"])
        .member("slack:home/U999", &["owner"])
        .policy_scopes(&GRANTS)
        .agent("main/research")
        .tool(
            "main",
            "agenda.delete",
            tool(Risk::Irreversible, &["agenda.delete"]),
        )
        .tool(
            "main/research",
            "agenda.delete",
            tool(Risk::Irreversible, &["agenda.delete"]),
        )
        .slack_channel_binding("team", "main")
        .openai_binding("b-assistant", "main", "openai", "assistant", &approvers)
        .build()
}

struct Rig {
    db: TestDb,
    catalog: Arc<FakeCatalog>,
    runner: Arc<FakeRunner>,
    /// 접수·게시를 한 프로세스의 호스트 등록부(워크스페이스 별칭 `home`의 채널 별칭 `planning`이 가리키는 팀은 `T1`)
    routes: Arc<MemRoutes>,
    /// 그 프로세스의 브로커(활성 집합 `set-1`에 고정)
    service: Arc<BrokerService>,
}

/// 호스트 등록부: 전달 바인딩 `team`이 가리키는 채널 별칭 `planning`의 실제 팀·채널.
fn host(team: &str) -> Arc<MemRoutes> {
    let routes = Arc::new(MemRoutes::new());
    routes.register(
        "personal",
        "team",
        ConversationKind::Channel,
        "planning",
        &format!("{team}/C1"),
    );
    routes
}

/// 활성 집합 `set`에 고정한 핸들로 브로커를 만든다(그 집합을 활성화한 프로세스).
fn broker(
    db: &TestDb,
    catalog: &Arc<FakeCatalog>,
    runner: &Arc<FakeRunner>,
    routes: &Arc<MemRoutes>,
    set: &str,
) -> Arc<BrokerService> {
    let pg = Arc::new(db.store.for_activation(Revision(set.into())));
    let service = BrokerService::new(
        Parts {
            catalog: catalog.clone(),
            ledger: pg.clone(),
            effects: pg.clone(),
            memory: pg.clone(),
            audit: pg.clone(),
            runner: runner.clone(),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_routes(routes.clone());
    Arc::new(service)
}

async fn rig(route: Option<ApprovalRoute>) -> Option<Rig> {
    let db = TestDb::new().await?;
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(world("r1", route));
    let runner = Arc::new(FakeRunner::new());
    let routes = host("T1");
    let service = broker(&db, &catalog, &runner, &routes, "set-1");
    Some(Rig {
        db,
        catalog,
        runner,
        routes,
        service,
    })
}

/// 호스트가 새 활성 집합 `set-2`를 게시한다(재배포: 프로필 판본 `r1`은 그대로).
async fn publish_set_2(store: &bot_store::Store) {
    let profiles = BTreeMap::from([
        (ProfileId("personal".into()), Revision("r1".into())),
        (ProfileId("work".into()), Revision("r1".into())),
    ]);
    ActiveRevisions::replace(
        store,
        Some(Revision("set-1".into())),
        Revision("set-2".into()),
        profiles,
        Vec::new(),
        Default::default(),
    )
    .await
    .unwrap();
}

impl Rig {
    /// 재배포: 같은 별칭의 실제 팀만 `team`으로 바뀌어 새 활성 집합 `set-2`로 올라온다(프로필 판본·사람·바인딩은 그대로). 새 프로세스의 브로커와 등록부를 돌려준다.
    async fn redeploy(&self, team: &str) -> Arc<BrokerService> {
        publish_set_2(&self.db.store).await;
        broker(&self.db, &self.catalog, &self.runner, &host(team), "set-2")
    }
}

/// 접수 원장으로 접수하고 그 run을 집는다. 접수 때 호스트가 확인한 전달 설치는 `T1`이다.
async fn admitted(rig: &Rig) -> RunContext {
    let mut req = request("k1", "build", "assistant", 1, 8);
    req.team = Some("T1".into());
    req.accept.scopes = scopes(&GRANTS);
    accepted(admit(&rig.db, req).await);
    claimed(&rig.db).await
}

async fn call(rig: &Rig, ctx: &RunContext, id: &str) -> Result<CallResult, Error> {
    call_on(&rig.service, ctx, id).await
}

async fn call_on(service: &BrokerService, ctx: &RunContext, id: &str) -> Result<CallResult, Error> {
    service
        .call(ToolCall {
            attempt: ctx.attempt.clone(),
            call: CallId(id.into()),
            tool: ToolId("agenda.delete".into()),
            args: JsonPayload(r#"{"event":"walk"}"#.into()),
        })
        .await
}

fn awaiting(result: Result<CallResult, Error>) -> ApprovalId {
    match result {
        Ok(CallResult::Awaiting(nonce)) => nonce,
        other => panic!("승인 대기여야 한다: {other:?}"),
    }
}

async fn click(rig: &Rig, nonce: &ApprovalId, actor: &str) -> Result<(), Error> {
    click_on(&rig.service, nonce, actor).await
}

async fn click_on(service: &BrokerService, nonce: &ApprovalId, actor: &str) -> Result<(), Error> {
    service
        .decide(nonce.clone(), PrincipalId(actor.into()), true)
        .await
}

/// 게이트웨이가 하는 일: 전달 작업을 점유해 올리고 게시 영수증을 확정한다.
async fn post_card(rig: &Rig, team: &str) {
    let outbox: &dyn CardOutbox = &rig.db.store;
    let job = outbox.claim(8).await.unwrap().remove(0);
    outbox
        .begin(job.nonce.clone(), Revision("r1".into()))
        .await
        .unwrap();
    outbox
        .settle(
            job.nonce,
            CardOutcome::Posted {
                team: team.into(),
                dm: "D1".into(),
                ts: "1.5".into(),
            },
        )
        .await
        .unwrap();
}

async fn state(rig: &Rig) -> ApiState {
    lookup(&rig.db, "k1", "build").await.unwrap().state
}

#[tokio::test]
async fn the_designated_slack_person_approves_a_service_principals_tool_exactly_once() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let ctx = admitted(&rig).await;
    let nonce = awaiting(call(&rig, &ctx, "c1").await);
    // 요청자는 HTTP 주체, 승인자는 지정된 사람. 카드는 전달 바인딩 `team`의 지정된 사람 DM으로 쌓이고 원래 요청의 증거를 이어받는다
    assert_eq!(
        rig.db
            .scalar::<String>(&format!(
                "select requester || '|' || approver from bot_approvals where nonce = '{}'",
                nonce.0
            ))
            .await,
        format!("{HTTP}|{PERSON}")
    );
    assert_eq!(
        rig.db
            .scalar::<String>(
                "select binding || '|' || owner || '|' || origin_binding || '|' || origin_team from bot_card_jobs"
            )
            .await,
        format!("team|{PERSON}|b-assistant|T1")
    );
    assert_eq!(
        state(&rig).await,
        ApiState::Active,
        "카드가 아직 열리지 않았다"
    );
    // 영수증이 확정되기 전에는 누를 수 없다(미소비)
    assert!(matches!(
        click(&rig, &nonce, PERSON).await,
        Err(Error::Conflict)
    ));
    post_card(&rig, "T1").await;
    assert_eq!(state(&rig).await, ApiState::NeedsHuman);
    // HTTP 키·다른 사람·소유자는 승인할 수 없다
    for actor in [HTTP, "slack:home/U999", OWNER] {
        assert!(
            matches!(click(&rig, &nonce, actor).await, Err(Error::Denied)),
            "{actor}"
        );
    }
    assert_eq!(rig.runner.jobs().len(), 0);
    click(&rig, &nonce, PERSON).await.unwrap();
    assert_eq!(rig.runner.jobs().len(), 0, "승인만으로는 실행하지 않는다");
    // 승인이 깨운 run을 다시 집는다(승인을 기다리는 동안 점유는 풀려 있다)
    let ctx = claimed(&rig.db).await;
    for _ in 0..2 {
        let got = call(&rig, &ctx, "c1").await;
        assert!(matches!(got, Ok(CallResult::Done(_))), "{got:?}");
    }
    assert_eq!(rig.runner.jobs().len(), 1, "정확히 한 번");
    assert_eq!(rig.runner.jobs()[0].principal.0, HTTP);
}

#[tokio::test]
async fn a_route_that_did_not_exist_at_acceptance_blocks_with_the_stored_reason_and_no_card() {
    let Some(rig) = rig(None).await else {
        return;
    };
    // 나중에 판본에 매핑을 넣어도 옛 요청의 승인을 얻을 수 없다
    rig.catalog
        .publish(world("r2", Some(slack(PERSON, "team"))));
    let ctx = admitted(&rig).await;
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        state(&rig).await,
        ApiState::ApprovalBlocked(ApprovalBlock::NoApprover)
    );
    assert_eq!(rig.runner.jobs().len(), 0);
    for table in ["bot_approvals", "bot_card_jobs"] {
        assert_eq!(
            rig.db
                .scalar::<i64>(&format!("select count(*) from {table}"))
                .await,
            0,
            "{table}"
        );
    }
}

#[tokio::test]
async fn a_designation_withdrawn_after_the_card_opened_stales_the_button_and_the_host_reads_the_reason()
 {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let ctx = admitted(&rig).await;
    let nonce = awaiting(call(&rig, &ctx, "c1").await);
    post_card(&rig, "T1").await;
    assert_eq!(state(&rig).await, ApiState::NeedsHuman);
    // 매핑이 철회됐다: 옛 버튼은 무효이고, 깨어난 run이 같은 호출로 돌아오면 도구 실행 없이 이유가 저장된 채 막힌다
    rig.catalog.publish(world("r2", None));
    assert!(matches!(
        click(&rig, &nonce, PERSON).await,
        Err(Error::Stale)
    ));
    // 낡은 클릭이 카드를 닫고 run을 깨운다. 다시 집은 run이 같은 호출로 돌아온다
    let ctx = claimed(&rig.db).await;
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        state(&rig).await,
        ApiState::ApprovalBlocked(ApprovalBlock::ApproverRevoked)
    );
    assert_eq!(rig.runner.jobs().len(), 0);
}

#[tokio::test]
async fn an_approval_given_before_the_route_was_reassigned_never_runs() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let ctx = admitted(&rig).await;
    let nonce = awaiting(call(&rig, &ctx, "c1").await);
    post_card(&rig, "T1").await;
    click(&rig, &nonce, PERSON).await.unwrap();
    // 승인했지만 소비 전에 다른 사람으로 재지정됐다(승인이 깨운 run을 다시 집어 같은 호출로 돌아온다)
    rig.catalog
        .publish(world("r2", Some(slack("slack:home/U999", "team"))));
    let ctx = claimed(&rig.db).await;
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(
        state(&rig).await,
        ApiState::ApprovalBlocked(ApprovalBlock::RouteChanged)
    );
}

#[tokio::test]
async fn a_delegated_child_asks_the_roots_designated_person_and_is_blocked_with_the_root() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let parent = admitted(&rig).await;
    rig.db
        .store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            checked_revision: Revision("r1".into()),
            step: "research".into(),
            child: AgentId("main/research".into()),
            instruction: "조사해".into(),
            scopes: scopes(&GRANTS),
        })
        .await
        .unwrap();
    let child = claimed(&rig.db).await;
    assert_eq!(child.binding.0, "b-assistant");
    let nonce = awaiting(call(&rig, &child, "c1").await);
    assert_eq!(
        rig.db
            .scalar::<String>(&format!(
                "select approver from bot_approvals where nonce = '{}'",
                nonce.0
            ))
            .await,
        PERSON
    );
    post_card(&rig, "T1").await;
    // 철회되면 옛 버튼은 무효이고(run을 깨운다) 자식의 막힘이 이 요청의 상태로 올라온다
    rig.catalog.publish(world("r2", None));
    assert!(matches!(
        click(&rig, &nonce, PERSON).await,
        Err(Error::Stale)
    ));
    let child = claimed(&rig.db).await;
    let got = call(&rig, &child, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(
        state(&rig).await,
        ApiState::ApprovalBlocked(ApprovalBlock::ApproverRevoked)
    );
    assert_eq!(rig.runner.jobs().len(), 0);
}

#[tokio::test]
async fn a_legacy_webhook_run_with_the_same_principal_name_gets_no_card_and_no_openai_verdict() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    // 접수 원장의 OpenAI 출처가 없는 일반 접수(레거시 webhook): 같은 이름의 HTTP 주체여도 승인 경로가 아니다
    let mut legacy = accept_as(
        "personal",
        "legacy",
        HTTP,
        ConversationKind::Http,
        "c-legacy",
        "질문",
    );
    legacy.scopes = scopes(&GRANTS);
    runs(&rig.db).accept(legacy).await.unwrap();
    let ctx = claimed(&rig.db).await;
    assert!(ctx.api.is_none());
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    for table in ["bot_approvals", "bot_card_jobs"] {
        assert_eq!(
            rig.db
                .scalar::<i64>(&format!("select count(*) from {table}"))
                .await,
            0,
            "{table}"
        );
    }
    assert_eq!(
        rig.db
            .scalar::<i64>("select count(*) from bot_calls where reason is not null")
            .await,
        0,
        "OpenAI 판정이 아니다"
    );
}

/// 지정된 사람이 접수 때 고정한 설치(`T1`)에 올라온 카드를 승인했고, 승인이 깨운 run을 다시 집은 상태.
async fn approved_in_t1(rig: &Rig) -> (ApprovalId, RunContext) {
    let ctx = admitted(rig).await;
    let nonce = awaiting(call(rig, &ctx, "c1").await);
    post_card(rig, "T1").await;
    click(rig, &nonce, PERSON).await.unwrap();
    (nonce, claimed(&rig.db).await)
}

async fn approval_state(rig: &Rig, nonce: &ApprovalId) -> String {
    rig.db
        .scalar::<String>(&format!(
            "select state from bot_approvals where nonce = '{}'",
            nonce.0
        ))
        .await
}

#[tokio::test]
async fn a_redeploy_that_only_changes_the_team_of_the_same_alias_never_consumes_an_approved_card() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let (nonce, ctx) = approved_in_t1(&rig).await;
    // 재배포: 같은 별칭의 실제 팀만 T2로 바뀌었다. 프로필 판본(r1)·지정된 사람·전달 바인딩은 그대로라 판본 비교로는 같다
    let next = rig.redeploy("T2").await;
    // 옛 프로세스(활성 집합 set-1에 고정)는 낡은 집합이라 소비하지 못한다
    let got = call_on(&rig.service, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    // 새 프로세스: 카드가 올라간 설치(T1)가 지금 호스트의 설치(T2)와 달라 소비 0·실행 0이고, 저장된 이유가 route_changed다
    let got = call_on(&next, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(approval_state(&rig, &nonce).await, "approved", "소비 0");
    assert_eq!(
        state(&rig).await,
        ApiState::ApprovalBlocked(ApprovalBlock::RouteChanged)
    );
}

#[tokio::test]
async fn a_redeploy_that_keeps_the_team_still_honors_the_existing_approval_exactly_once() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let (nonce, ctx) = approved_in_t1(&rig).await;
    // 새 활성 집합으로 올라오지만 같은 팀(T1)이다: 실제 팀이 같으면 기존 유효 승인은 정확히 한 번 실행된다
    let next = rig.redeploy("T1").await;
    for _ in 0..2 {
        let got = call_on(&next, &ctx, "c1").await;
        assert!(matches!(got, Ok(CallResult::Done(_))), "{got:?}");
    }
    assert_eq!(rig.runner.jobs().len(), 1, "정확히 한 번");
    assert_eq!(approval_state(&rig, &nonce).await, "consumed");
}

#[tokio::test]
async fn an_unreachable_host_registry_neither_consumes_nor_blocks_and_the_approval_runs_once_when_it_returns()
 {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let (nonce, ctx) = approved_in_t1(&rig).await;
    // 설치를 확인하지 못하는 동안은 막지도 소비하지도 않는다(일시 오류는 막힘의 증거가 아니다)
    rig.routes.outage(true);
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Unavailable)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(approval_state(&rig, &nonce).await, "approved");
    assert!(!matches!(state(&rig).await, ApiState::ApprovalBlocked(_)));
    // 확인이 돌아오면 같은 승인으로 정확히 한 번 실행한다
    rig.routes.outage(false);
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Ok(CallResult::Done(_))), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 1);
    assert_eq!(approval_state(&rig, &nonce).await, "consumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_active_set_fence_stops_a_consume_when_the_host_is_redeployed_right_after_the_team_check()
 {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let (nonce, ctx) = approved_in_t1(&rig).await;
    // 소비 직전 설치 확인(T1 그대로)과 소비 커밋 사이에 재배포가 새 활성 집합을 게시한다: 확인은 통과했어도 커밋이 집합을 다시 대조해 소비하지 않는다
    let store = rig.db.store.clone();
    rig.routes.on_nth_ask(rig.routes.asked() + 1, move || {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(publish_set_2(&store));
        });
    });
    let got = call_on(&rig.service, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(approval_state(&rig, &nonce).await, "approved", "소비 0");
}

#[tokio::test]
async fn a_card_posted_to_the_previous_team_cannot_be_clicked_by_the_redeployed_process() {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let ctx = admitted(&rig).await;
    let nonce = awaiting(call(&rig, &ctx, "c1").await);
    post_card(&rig, "T1").await;
    let next = rig.redeploy("T2").await;
    // 카드는 옛 설치(T1)에 올라갔다: 지금 호스트의 설치(T2)에서는 승인으로 받아들이지 않고 옛 버튼을 무효로 한다
    assert!(matches!(
        click_on(&next, &nonce, PERSON).await,
        Err(Error::Stale)
    ));
    assert_eq!(approval_state(&rig, &nonce).await, "superseded");
    assert_eq!(rig.runner.jobs().len(), 0);
}
