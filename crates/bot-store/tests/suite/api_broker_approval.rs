//! End-to-end tool approval for OpenAI-compatible HTTP runs (real broker + real PG).

use crate::support::{api::*, *};
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

const HTTP: &str = "http:openai/build";
const PERSON: &str = "slack:home/U123";
const GRANTS: [&str; 2] = ["run", "agenda.delete"];

fn slack(person: &str, binding: &str) -> ApprovalRoute {
    ApprovalRoute::Slack {
        person: PrincipalId(person.into()),
        binding: BindingId(binding.into()),
    }
}

/// Policy revision stays constant across snapshots to isolate approval-route checks from policy-revision mismatches.
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
    routes: Arc<MemRoutes>,
    service: Arc<BrokerService>,
}

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
    /// Redeploys to `set-2` with the same profile revision and binding, remapping only the Slack team.
    async fn redeploy(&self, team: &str) -> Arc<BrokerService> {
        publish_set_2(&self.db.store).await;
        broker(&self.db, &self.catalog, &self.runner, &host(team), "set-2")
    }
}

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
        other => panic!("expected Awaiting approval: {other:?}"),
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

async fn post_card(rig: &Rig, team: &str) {
    let outbox: &dyn CardOutbox = &rig.db.store;
    let job = outbox.claim(8).await.unwrap().remove(0);
    outbox
        .begin(job.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    outbox
        .settle(
            job.key,
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
        "card is not posted yet"
    );
    // Unposted cards cannot be decided before a Posted receipt commits.
    assert!(matches!(
        click(&rig, &nonce, PERSON).await,
        Err(Error::Conflict)
    ));
    post_card(&rig, "T1").await;
    assert_eq!(state(&rig).await, ApiState::NeedsHuman);
    // HTTP principal, other users, and default owner cannot approve.
    for actor in [HTTP, "slack:home/U999", OWNER] {
        assert!(
            matches!(click(&rig, &nonce, actor).await, Err(Error::Denied)),
            "{actor}"
        );
    }
    assert_eq!(rig.runner.jobs().len(), 0);
    click(&rig, &nonce, PERSON).await.unwrap();
    assert_eq!(
        rig.runner.jobs().len(),
        0,
        "approval alone must not execute the tool"
    );
    let ctx = claimed(&rig.db).await;
    for _ in 0..2 {
        let got = call(&rig, &ctx, "c1").await;
        assert!(matches!(got, Ok(CallResult::Done(_))), "{got:?}");
    }
    assert_eq!(rig.runner.jobs().len(), 1, "must execute exactly once");
    assert_eq!(rig.runner.jobs()[0].principal.0, HTTP);
}

#[tokio::test]
async fn a_route_that_did_not_exist_at_acceptance_blocks_with_the_stored_reason_and_no_card() {
    let Some(rig) = rig(None).await else {
        return;
    };
    // Adding a route in a later revision cannot authorize a request admitted without one.
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
    // Withdrawing the route invalidates the card and blocks the resumed call without running the tool.
    rig.catalog.publish(world("r2", None));
    assert!(matches!(
        click(&rig, &nonce, PERSON).await,
        Err(Error::Stale)
    ));
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
    // Reassigning the approver before consumption blocks execution.
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
            instruction: "investigate".into(),
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
    // Revoking the route invalidates the card and surfaces the child's block on the root request.
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
    // Legacy webhook runs lack OpenAI admission origin and never enter the API approval route.
    let mut legacy = accept_as(
        "personal",
        "legacy",
        HTTP,
        ConversationKind::Http,
        "c-legacy",
        "question",
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
        "not an OpenAI approval block"
    );
}

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
    // Redeploy changes only the resolved team to T2 (revision, approver, and binding unchanged).
    let next = rig.redeploy("T2").await;
    let got = call_on(&rig.service, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    let got = call_on(&next, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Denied)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(
        approval_state(&rig, &nonce).await,
        "approved",
        "approval must not be consumed"
    );
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
    let next = rig.redeploy("T1").await;
    for _ in 0..2 {
        let got = call_on(&next, &ctx, "c1").await;
        assert!(matches!(got, Ok(CallResult::Done(_))), "{got:?}");
    }
    assert_eq!(rig.runner.jobs().len(), 1, "must execute exactly once");
    assert_eq!(approval_state(&rig, &nonce).await, "consumed");
}

#[tokio::test]
async fn an_unreachable_host_registry_neither_consumes_nor_blocks_and_the_approval_runs_once_when_it_returns()
 {
    let Some(rig) = rig(Some(slack(PERSON, "team"))).await else {
        return;
    };
    let (nonce, ctx) = approved_in_t1(&rig).await;
    // Registry outage returns Unavailable without consuming or blocking.
    rig.routes.outage(true);
    let got = call(&rig, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Unavailable)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(approval_state(&rig, &nonce).await, "approved");
    assert!(!matches!(state(&rig).await, ApiState::ApprovalBlocked(_)));
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
    // Publish set-2 between the pre-consume team check and DB commit; activation fence must reject.
    let store = rig.db.store.clone();
    rig.routes.on_nth_ask(rig.routes.asked() + 1, move || {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(publish_set_2(&store));
        });
    });
    let got = call_on(&rig.service, &ctx, "c1").await;
    assert!(matches!(got, Err(Error::Stale)), "{got:?}");
    assert_eq!(rig.runner.jobs().len(), 0);
    assert_eq!(
        approval_state(&rig, &nonce).await,
        "approved",
        "approval must not be consumed"
    );
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
    // Card posted to T1 is rejected as Stale on the set-2 process bound to T2.
    assert!(matches!(
        click_on(&next, &nonce, PERSON).await,
        Err(Error::Stale)
    ));
    assert_eq!(approval_state(&rig, &nonce).await, "superseded");
    assert_eq!(rig.runner.jobs().len(), 0);
}
