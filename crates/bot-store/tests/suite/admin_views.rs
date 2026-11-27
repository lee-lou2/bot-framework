//! 관리 조회(PG): 판정된 보류 단계는 더 이상 보류로 보이지 않고, 보류 종류는 실제 출처로 정해지며, 호출 실패는 고정 분류로만 보인다.

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

/// 두 단계가 모두 보류인 다단계 호출(c1)이 있어 run이 보류 대기에 든 상태.
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
            receipt: Some("timeout".into()), // 보류 사유 코드
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

/// 다단계 호출(c1)의 한 단계를 가리키는 판정 대상(열린 발생 ID는 보류 목록에서 읽는다).
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
        evidence: "수신처에 없음".into(),
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
        "판정된 단계는 보류가 아니다"
    );
    // 미실행 판정 표지는 DB에 남는다: 저장소를 새로 열어(재시작) 읽어도 같다. 판정하지 않은 단계에는 없다
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
    assert_eq!(state_of(&second).0, EffectState::Held, "미결 단계만 보류");
    assert_eq!(
        state_of_run(&db, &run.attempt.run).await,
        "waiting",
        "미결 단계가 남았으면 run은 계속 기다린다"
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
    // 보류가 아닌 호출은 분류가 없다
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
    // 도구 보류: 도구 이름이 `reply`여도 도구 보류다
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
    // 회신 보류: 푸시 대화 run의 답 행이 보류가 됐다
    let dm = leased_in(&db, "dm", bot_core::state::ConversationKind::Direct).await;
    runs(&db)
        .finish(dm.attempt.clone(), done("안녕하세요"))
        .await
        .unwrap();
    db.sql("update bot_outbox set state = 'held', reason = 'timeout'")
        .await;
    // 민감 DM 보류
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
        "이름이 reply여도 종류는 도구"
    );

    // 합성 ID 접두사는 도구 호출 ID로 쓸 수 없다(판정 경로가 섞이지 않는다)
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
    // 한 단계는 영수증을 받아 확정, 다른 단계는 시작만 하고 결과를 모른다
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
    assert_eq!(holds.len(), 1, "run이 보류로 기다리면 목록에도 있다");
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
        "결과가 불명인 단계는 판정 가능한 보류가 된다"
    );
    assert_eq!(
        state_of(&sent.status.effect).map(|s| s.0),
        Some(EffectState::Confirmed),
        "영수증을 받은 단계는 그대로다"
    );
    // 알림의 미실행 판정은 업로드 영수증이 있는 호출 전체를 재개하지 않는다.
    let target = step_ref(&db, &run, &unknown.status.effect).await;
    AdminAct::resolve_hold(
        &db.store,
        PrincipalId("local:admin".into()),
        target,
        HoldVerdict::NotExecuted {
            evidence: "수신처에 없음".into(),
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
    // personal: 1) 다단계 보류 호출(2단계 → bot_holds 2행이지만 보류 카드 1건),
    //           2) 단일 보류 호출(1건), 3) 회신 보류(1건), 4) 만료된 민감 DM(sweep 전!),
    //           5) 승인 대기 1건, 6) ready 1건
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

    // 회신 보류: direct run을 끝낸 뒤 outbox sending 리스를 만료시켜 held로 옮긴다
    let reply_run = leased_in(&db, "reply-hold", ConversationKind::Direct).await;
    runs(&db)
        .finish(reply_run.attempt.clone(), done("회신 본문"))
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

    // 만료된 민감 DM(아직 sweep 전): status()가 집계 전에 sweep_private()를 돌려 보류로 잡아야 한다
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

    // 승인 대기 1건
    let awaiting = leased(&db, "pending-approval").await;
    begin_call(&db, &awaiting.attempt, "c-ap").await;
    calls(&db)
        .open_approval(open_approval(binding(&awaiting.attempt, "c-ap", "n-st-1")))
        .await
        .unwrap();

    // ready 1건(personal) + work 프로필의 단일 보류 호출 1건 + ready 1건
    runs(&db)
        .accept(accept("personal", "ready-p", "기다림"))
        .await
        .unwrap();
    runs(&db)
        .accept(accept("work", "work-hold", "작업"))
        .await
        .unwrap();
    // personal ready를 건너뛰지 않도록 work run만 직접 claim되도록 work-ready는 뒤에 접수
    let mut work_run = None;
    for _ in 0..4 {
        let Some(ctx) = runs(&db).claim().await.unwrap() else {
            break;
        };
        if ctx.attempt.profile.0 == "work" {
            work_run = Some(ctx);
            break;
        }
        // personal ready는 다시 ready로 돌려놓는다
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
        .accept(accept("work", "ready-w", "작업 대기"))
        .await
        .unwrap();

    // AdminRead::status를 holds보다 먼저 호출해 sweep_private()가 status 안에서 수행되는지 검증한다
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
            "프로필 {}의 status.holds는 holds 목록 개수와 같아야 한다",
            st.profile.0
        );
    }
    assert_eq!(
        statuses[0].holds, 4,
        "personal: 다단계1 + 단일1 + 회신1 + 민감DM1"
    );
    assert_eq!(statuses[0].approvals_pending, 1);
    assert!(statuses[0].oldest_ready_secs.is_some());
    assert_eq!(statuses[1].holds, 1, "work: 단일 보류 1건");
    assert_eq!(statuses[1].approvals_pending, 0);
    assert!(statuses[1].oldest_ready_secs.is_some());

    // 배치 조회된 holds의 각 종류·단계·hold_id가 빠짐없이 채워져 있다
    let multi_view = all_holds
        .iter()
        .find(|h| h.profile.0 == "personal" && h.call.0 == "c1")
        .expect("다단계 보류 카드");
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
        "회신 보류 hold_id 포함"
    );
    assert!(
        all_holds
            .iter()
            .any(|h| h.kind == HoldKind::Private && h.hold_id.is_some()),
        "민감 DM 보류 hold_id 포함"
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

    // 0028 색인이 모두 생성되었고 기존 bot_runs_ready·bot_runs_leased 색인도 보존된다
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
        assert!(present, "색인 {index}가 있어야 한다");
    }
}
