//! 관리 조회(PG): 판정된 보류 단계는 더 이상 보류로 보이지 않고, 보류 종류는 실제 출처로 정해지며, 호출 실패는 고정 분류로만 보인다.

mod support;

use bot_core::{
    admin::*,
    command::*,
    id::*,
    ledger::*,
    port::*,
    state::{EffectState, RunContext},
};
use support::{admin::*, *};

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
    let private = support::private::started(&db, "p", 1).await;
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
