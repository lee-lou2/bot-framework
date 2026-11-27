//! Admin read views (PG): resolved hold steps leave the hold list, hold kinds reflect source, and call errors use fixed classes.

use crate::support::{admin::*, *};
use bot_core::{
    admin::*,
    command::*,
    id::*,
    ledger::*,
    port::*,
    state::{EffectState, RunContext},
};

fn step(attempt: &Attempt, name: &str) -> EffectRequest {
    EffectRequest {
        attempt: attempt.clone(),
        call: call("c1"),
        step: name.into(),
        args: JsonPayload("{}".into()),
    }
}

async fn two_held_steps(db: &TestDb) -> (RunContext, [EffectId; 2]) {
    let run = leased(db, "multi").await;
    begin_call(db, &run.attempt, "c1").await;
    calls(db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let mut ids = Vec::new();
    for name in ["upload", "notify"] {
        let permit = effects(db).request(step(&run.attempt, name)).await.unwrap();
        let held = EffectStatus {
            effect: permit.status.effect.clone(),
            state: EffectState::Held,
            resolution: None,
            receipt: Some("timeout".into()),
        };
        effects(db)
            .record(run.attempt.clone(), call("c1"), held)
            .await
            .unwrap();
        ids.push(permit.status.effect);
    }
    calls(db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    (run, [ids.remove(0), ids.remove(0)])
}

async fn step_ref(db: &TestDb, run: &RunContext, effect: &EffectId) -> HoldRef {
    hold_ref(
        db,
        HoldKind::Tool,
        &run.attempt.run,
        &call("c1"),
        Some(effect),
    )
    .await
}

#[tokio::test]
async fn a_step_judged_not_executed_no_longer_shows_as_held() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, [first, second]) = two_held_steps(&db).await;
    let verdict = || HoldVerdict::NotExecuted {
        evidence: "not found at destination".into(),
    };
    let actor = PrincipalId("local:admin".into());
    let target = step_ref(&db, &run, &first).await;
    AdminAct::resolve_hold(&db.store, actor.clone(), target, verdict())
        .await
        .unwrap();

    let detail = AdminRead::detail(
        &db.store,
        &operator(),
        run.attempt.profile.clone(),
        run.attempt.run.clone(),
    )
    .await
    .unwrap();
    let state_of = |id: &EffectId| {
        detail
            .effects
            .iter()
            .find(|e| e.effect == *id)
            .map(|e| (e.state, e.reason.clone()))
            .unwrap()
    };
    assert_eq!(
        state_of(&first),
        (EffectState::Started, None),
        "resolved step is no longer held"
    );
    // NotExecuted resolution persists across store restart; unresolved steps have none.
    let restarted = db.peer("restarted").await;
    let after_restart = AdminRead::detail(
        &restarted,
        &operator(),
        run.attempt.profile.clone(),
        run.attempt.run.clone(),
    )
    .await
    .unwrap();
    let marker = |id: &EffectId| {
        after_restart
            .effects
            .iter()
            .find(|e| e.effect == *id)
            .and_then(|e| e.resolution)
    };
    assert_eq!(marker(&first), Some(EffectResolution::NotExecuted));
    assert_eq!(marker(&second), None);
    assert_eq!(
        state_of(&second).0,
        EffectState::Held,
        "only unresolved step remains held"
    );
    assert_eq!(
        state_of_run(&db, &run.attempt.run).await,
        "waiting",
        "run stays waiting while an unresolved step remains"
    );
    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1);
    let held: Vec<_> = holds[0]
        .steps
        .iter()
        .filter(|s| s.state == EffectState::Held)
        .collect();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].effect, second);

    let target = step_ref(&db, &run, &second).await;
    AdminAct::resolve_hold(&db.store, actor, target, verdict())
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    assert!(
        AdminRead::holds(&db.store, &operator(), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_held_call_shows_only_a_fixed_error_class() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (run, _) = two_held_steps(&db).await;
    let detail = AdminRead::detail(
        &db.store,
        &operator(),
        run.attempt.profile.clone(),
        run.attempt.run.clone(),
    )
    .await
    .unwrap();
    assert_eq!(detail.calls[0].state, CallState::Held);
    assert_eq!(detail.calls[0].error, Some(CallError::Timeout));
    let ok = leased(&db, "ok").await;
    begin_call(&db, &ok.attempt, "c9").await;
    let detail = AdminRead::detail(
        &db.store,
        &operator(),
        ok.attempt.profile.clone(),
        ok.attempt.run.clone(),
    )
    .await
    .unwrap();
    assert_eq!(detail.calls[0].error, None);
}

#[tokio::test]
async fn holds_carry_the_kind_of_their_source_and_names_cannot_impersonate_another_kind() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Tool named `reply` must still classify as HoldKind::Tool.
    let tool_run = leased(&db, "tool").await;
    calls(&db)
        .begin(tool_run.attempt.clone(), call("c1"), tool("reply"), [1; 32])
        .await
        .unwrap();
    calls(&db)
        .start(start_call(&tool_run.attempt, "c1", None))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: tool_run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    let dm = leased_in(&db, "dm", bot_core::state::ConversationKind::Direct).await;
    runs(&db)
        .finish(dm.attempt.clone(), done("hello"))
        .await
        .unwrap();
    db.sql("update bot_outbox set state = 'held', reason = 'timeout'")
        .await;
    let private = crate::support::private::started(&db, "p", 1).await;
    PrivateLedger::finish(&db.store, private.clone(), PrivateOutcome::Held)
        .await
        .unwrap();

    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    let kind_of = |f: &dyn Fn(&HoldView) -> bool| holds.iter().find(|h| f(h)).map(|h| h.kind);
    assert_eq!(holds.len(), 3);
    assert_eq!(
        kind_of(&|h| h.run == tool_run.attempt.run),
        Some(HoldKind::Tool)
    );
    assert_eq!(kind_of(&|h| h.run == dm.attempt.run), Some(HoldKind::Reply));
    assert_eq!(kind_of(&|h| h.run == private.run), Some(HoldKind::Private));
    assert_eq!(
        holds
            .iter()
            .find(|h| h.run == tool_run.attempt.run)
            .unwrap()
            .tool
            .0,
        "reply",
        "tool named reply still has HoldKind::Tool"
    );

    // Synthetic hold ID prefixes cannot be used as tool call IDs.
    let run = leased(&db, "ids").await;
    for id in ["reply:x:1", "private:x"] {
        assert!(matches!(
            calls(&db)
                .begin(run.attempt.clone(), call(id), tool("web.search"), [1; 32])
                .await,
            Err(Error::Invalid)
        ));
    }
}

#[tokio::test]
async fn holding_a_call_turns_its_started_steps_without_a_receipt_into_judgeable_holds() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "unknown").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let sent = effects(&db)
        .request(step(&run.attempt, "upload"))
        .await
        .unwrap();
    effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            EffectStatus {
                effect: sent.status.effect.clone(),
                state: EffectState::Confirmed,
                resolution: None,
                receipt: Some("file-1".into()),
            },
        )
        .await
        .unwrap();
    let unknown = effects(&db)
        .request(step(&run.attempt, "notify"))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");

    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1, "held waiting run appears in holds list");
    let state_of = |id: &EffectId| {
        holds[0]
            .steps
            .iter()
            .find(|s| s.effect == *id)
            .map(|s| (s.state, s.reason.clone()))
    };
    assert_eq!(
        state_of(&unknown.status.effect),
        Some((EffectState::Held, Some("timeout".into()))),
        "step with unknown outcome becomes a judgeable hold"
    );
    assert_eq!(
        state_of(&sent.status.effect).map(|s| s.0),
        Some(EffectState::Confirmed),
        "confirmed step keeps its receipt"
    );
    // Marking notify as NotExecuted must not resume the call while upload holds a confirmed receipt.
    let target = step_ref(&db, &run, &unknown.status.effect).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:admin".into()),
        target,
        HoldVerdict::NotExecuted {
            evidence: "not found at destination".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "waiting");
    let final_hold = db
        .store
        .holds(&operator(), None)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(final_hold.hold_id.is_some());
    assert_eq!(final_hold.steps.len(), 2);
}

#[tokio::test]
async fn batched_holds_and_aggregated_status_match_across_profiles_and_hold_kinds() {
    use bot_core::{
        intake::{DeliveryScope, PushTransportId, RequestKey},
        state::{Conversation, ConversationKind},
    };
    let Some(db) = TestDb::new().await else {
        return;
    };
    // personal: 1 multi-step tool hold (2 bot_holds rows -> 1 hold card), 1 single tool hold,
    // 1 reply hold, 1 expired unswept private DM, 1 pending approval, and 1 ready run.
    let (_multi, multi_steps) = two_held_steps(&db).await;

    let single = leased(&db, "single-hold").await;
    begin_call(&db, &single.attempt, "c-single").await;
    calls(&db)
        .start(start_call(&single.attempt, "c-single", None))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: single.attempt.clone(),
            call: call("c-single"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();

    let reply_run = leased_in(&db, "reply-hold", ConversationKind::Direct).await;
    runs(&db)
        .finish(reply_run.attempt.clone(), done("reply body"))
        .await
        .unwrap();
    let scope = DeliveryScope::all(PushTransportId::SLACK);
    let claimed_out = outbox(&db).claim(&scope, 10).await.unwrap();
    assert_eq!(claimed_out.len(), 1);
    outbox(&db)
        .begin(claimed_out[0].key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1 where state = 'sending'")
        .await;
    assert!(outbox(&db).claim(&scope, 10).await.unwrap().is_empty());

    // Expired private DM (unswept): status() must run sweep_private() before counting.
    let PrivatePermit::Start {
        attempt: priv_at, ..
    } = PrivateLedger::begin(
        &db.store,
        PrivateRequest {
            pass_fingerprint: None,
            key: RequestKey {
                profile: profile_id(),
                binding: BindingId("direct".into()),
                principal: PrincipalId("local:owner".into()),
                request: RequestId("priv-exp".into()),
            },
            fingerprint: [7; 32],
            revision: Revision("r1".into()),
            agent: AgentId("main".into()),
            tool: ToolId("inbox.receive".into()),
            conversation: Conversation {
                kind: ConversationKind::Direct,
                id: "dm-priv".into(),
                thread: None,
            },
            scopes: scopes(&["inbox.write"]),
        },
    )
    .await
    .unwrap()
    else {
        panic!("Start expected")
    };
    db.expire_lease(&priv_at.run).await;

    let awaiting = leased(&db, "pending-approval").await;
    begin_call(&db, &awaiting.attempt, "c-ap").await;
    calls(&db)
        .open_approval(open_approval(binding(&awaiting.attempt, "c-ap", "n-st-1")))
        .await
        .unwrap();

    runs(&db)
        .accept(accept("personal", "ready-p", "waiting"))
        .await
        .unwrap();
    runs(&db)
        .accept(accept("work", "work-hold", "task"))
        .await
        .unwrap();
    // Claim the work run while restoring personal's ready run so it stays queued.
    let mut work_run = None;
    for _ in 0..4 {
        let Some(ctx) = runs(&db).claim().await.unwrap() else {
            break;
        };
        if ctx.attempt.profile.0 == "work" {
            work_run = Some(ctx);
            break;
        }
        db.sql(&format!(
            "update bot_runs set state = 'ready', lease_until_ms = null, lease_owner = null where id = '{}'",
            ctx.attempt.run.0
        ))
        .await;
    }
    let work_run = work_run.expect("work run claimed");
    begin_call(&db, &work_run.attempt, "c-work").await;
    calls(&db)
        .start(start_call(&work_run.attempt, "c-work", None))
        .await
        .unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: work_run.attempt.clone(),
            call: call("c-work"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    runs(&db)
        .accept(accept("work", "ready-w", "pending task"))
        .await
        .unwrap();

    // Call status() before holds() to verify sweep_private() runs inside status().
    let statuses = AdminRead::status(&db.store, &operator()).await.unwrap();
    let all_holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();

    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0].profile.0, "personal");
    assert_eq!(statuses[1].profile.0, "work");
    for st in &statuses {
        let expected_holds = all_holds.iter().filter(|h| h.profile == st.profile).count() as u64;
        assert_eq!(
            st.holds, expected_holds,
            "status.holds for profile {} must match holds list count",
            st.profile.0
        );
    }
    assert_eq!(
        statuses[0].holds, 4,
        "personal: 1 multi-step + 1 single + 1 reply + 1 private DM"
    );
    assert_eq!(statuses[0].approvals_pending, 1);
    assert!(statuses[0].oldest_ready_secs.is_some());
    assert_eq!(statuses[1].holds, 1, "work: 1 single hold");
    assert_eq!(statuses[1].approvals_pending, 0);
    assert!(statuses[1].oldest_ready_secs.is_some());

    let multi_view = all_holds
        .iter()
        .find(|h| h.profile.0 == "personal" && h.call.0 == "c1")
        .expect("multi-step hold card");
    assert_eq!(multi_view.kind, HoldKind::Tool);
    assert!(multi_view.hold_id.is_none());
    assert_eq!(multi_view.steps.len(), 2);
    assert_eq!(multi_view.steps[0].effect, multi_steps[0]);
    assert_eq!(multi_view.steps[1].effect, multi_steps[1]);
    assert!(multi_view.steps.iter().all(|s| s.hold_id.is_some()));

    assert!(
        all_holds
            .iter()
            .any(|h| h.kind == HoldKind::Reply && h.hold_id.is_some()),
        "reply hold includes hold_id"
    );
    assert!(
        all_holds
            .iter()
            .any(|h| h.kind == HoldKind::Private && h.hold_id.is_some()),
        "private DM hold includes hold_id"
    );
}

#[tokio::test]
async fn config_max_connections_applies_and_0028_indexes_exist() {
    use bot_store::{Config, Store};
    assert_eq!(Config::default().max_connections, 10);
    assert_eq!(Store::pool_options().get_max_connections(), 10);
    assert_eq!(Store::pool_options_with(0).get_max_connections(), 1);
    assert_eq!(Store::pool_options_with(4).get_max_connections(), 4);

    let Some(db) = TestDb::new().await else {
        return;
    };
    let url = std::env::var("BOT_TEST_DATABASE_URL").unwrap();
    let custom = Store::connect(
        &url,
        Config {
            max_connections: 3,
            ..Config::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(custom.pool().options().get_max_connections(), 3);
    let clamped = Store::connect(
        &url,
        Config {
            max_connections: 0,
            ..Config::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(clamped.pool().options().get_max_connections(), 1);

    for index in [
        "bot_runs_private_active",
        "bot_runs_updated",
        "bot_calls_held_open",
        "bot_card_jobs_closed",
        "bot_outbox_held",
        "bot_runs_ready",
        "bot_runs_leased",
    ] {
        let present: bool = sqlx::query_scalar(
            "select exists (select 1 from pg_indexes where schemaname = $1 and indexname = $2)",
        )
        .bind(&db.schema)
        .bind(index)
        .fetch_one(db.store.pool())
        .await
        .unwrap();
        assert!(present, "index {index} must exist");
    }
}
