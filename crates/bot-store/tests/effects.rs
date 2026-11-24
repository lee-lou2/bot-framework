mod support;

use bot_core::{command::*, id::*, ledger::*, port::*, state::*};
use support::*;

fn step(attempt: &Attempt, call: &str, name: &str, args: &str) -> EffectRequest {
    EffectRequest {
        attempt: attempt.clone(),
        call: CallId(call.into()),
        step: name.into(),
        args: JsonPayload(args.into()),
    }
}

fn confirmed(effect: &EffectId, receipt: &str) -> EffectStatus {
    EffectStatus {
        effect: effect.clone(),
        state: EffectState::Confirmed,
        resolution: None,
        receipt: Some(receipt.into()),
    }
}

/// 시작한 호출(c1)이 있는 실행 중 run.
async fn running(db: &TestDb) -> RunContext {
    let run = leased(db, "r").await;
    begin_call(db, &run.attempt, "c1").await;
    calls(db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    run
}

#[tokio::test]
async fn only_the_first_request_may_send_and_confirmed_steps_return_their_receipt() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let first = effects(&db)
        .request(step(&run.attempt, "c1", "upload", r#"{"file":"summary"}"#))
        .await
        .unwrap();
    assert!(first.send);
    assert_eq!(first.status.state, EffectState::Started);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_effects").await,
        1
    );

    // 같은 단계의 재요청은 보내지 않는다(전송 여부가 불명이므로 held)
    let again = effects(&db)
        .request(step(&run.attempt, "c1", "upload", r#"{"file": "summary"}"#))
        .await
        .unwrap();
    assert!(!again.send);
    assert_eq!(
        (again.status.effect.clone(), again.status.state),
        (first.status.effect.clone(), EffectState::Held)
    );

    effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            confirmed(&first.status.effect, "file-17"),
        )
        .await
        .unwrap();
    effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            confirmed(&first.status.effect, "file-17"),
        )
        .await
        .unwrap(); // 같은 영수증 반복
    let other = effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            confirmed(&first.status.effect, "file-99"),
        )
        .await;
    assert!(matches!(other, Err(Error::Conflict)));

    let done = effects(&db)
        .request(step(&run.attempt, "c1", "upload", r#"{"file":"summary"}"#))
        .await
        .unwrap();
    assert!(!done.send);
    assert_eq!(
        (done.status.state, done.status.receipt.as_deref()),
        (EffectState::Confirmed, Some("file-17"))
    );
    let changed = effects(&db)
        .request(step(&run.attempt, "c1", "upload", r#"{"file":"other"}"#))
        .await;
    assert!(
        matches!(changed, Err(Error::Conflict)),
        "같은 키·다른 인자는 충돌"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_effects").await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn unresolved_lists_started_and_held_steps_only() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let upload = effects(&db)
        .request(step(&run.attempt, "c1", "upload", "{}"))
        .await
        .unwrap()
        .status
        .effect;
    let notify = effects(&db)
        .request(step(&run.attempt, "c1", "notify", "{}"))
        .await
        .unwrap()
        .status
        .effect;
    assert_eq!(
        effects(&db)
            .unresolved(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .len(),
        2
    );
    effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            confirmed(&upload, "file-1"),
        )
        .await
        .unwrap();
    let left = effects(&db)
        .unresolved(run.attempt.clone(), call("c1"))
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].effect, notify);

    // 불명 사유를 적어 보류하면 계속 미결이고, 원래 영수증은 그대로다
    let held = EffectStatus {
        effect: notify.clone(),
        state: EffectState::Held,
        resolution: None,
        receipt: Some("timeout".into()),
    };
    effects(&db)
        .record(run.attempt.clone(), call("c1"), held)
        .await
        .unwrap();
    assert_eq!(
        effects(&db)
            .unresolved(run.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.scalar::<String>("select reason from bot_effects where step = 'notify'")
            .await,
        "timeout"
    );
    assert_eq!(
        db.scalar::<String>("select receipt from bot_effects where step = 'upload'")
            .await,
        "file-1"
    );
    let status = effects(&db)
        .status(run.attempt.clone(), call("c1"), upload)
        .await
        .unwrap();
    assert_eq!(
        (status.state, status.receipt.as_deref()),
        (EffectState::Confirmed, Some("file-1"))
    );
}

#[tokio::test]
async fn steps_need_a_running_call_and_a_current_owner() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "r").await;
    begin_call(&db, &run.attempt, "c1").await; // 시작하지 않은(Open) 호출
    let open = effects(&db)
        .request(step(&run.attempt, "c1", "upload", "{}"))
        .await;
    assert!(matches!(open, Err(Error::Conflict)));
    let ghost = effects(&db)
        .request(step(&run.attempt, "c9", "upload", "{}"))
        .await;
    assert!(matches!(ghost, Err(Error::Missing)));

    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let id = effects(&db)
        .request(step(&run.attempt, "c1", "upload", "{}"))
        .await
        .unwrap()
        .status
        .effect;
    // 다른 호출·옛 세대의 갱신은 거절한다
    begin_call(&db, &run.attempt, "c2").await;
    calls(&db)
        .start(start_call(&run.attempt, "c2", None))
        .await
        .unwrap();
    let foreign = effects(&db)
        .record(run.attempt.clone(), call("c2"), confirmed(&id, "x"))
        .await;
    assert!(matches!(foreign, Err(Error::Denied)));
    let new = db.takeover(&run.attempt.run).await;
    assert!(matches!(
        effects(&db)
            .record(run.attempt.clone(), call("c1"), confirmed(&id, "x"))
            .await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        effects(&db)
            .unresolved(run.attempt.clone(), call("c1"))
            .await,
        Err(Error::Stale)
    ));
    assert_eq!(new.attempt.generation, Generation(2));
}

#[tokio::test]
async fn a_new_generation_finds_the_old_step_held_and_does_not_resend() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let id = effects(&db)
        .request(step(&run.attempt, "c1", "upload", "{}"))
        .await
        .unwrap()
        .status
        .effect;
    let new = db.takeover(&run.attempt.run).await;
    // 이전 시도가 시작한 호출은 그대로 Started다(복구 판정은 브로커 몫). 같은 단계를 요청하려면 호출이 현재 세대 것이어야 한다
    let blocked = effects(&db)
        .request(step(&new.attempt, "c1", "upload", "{}"))
        .await;
    assert!(matches!(blocked, Err(Error::Conflict)));
    // 미실행이 입증돼 호출이 풀리고 다시 시작되면, 옛 세대가 남긴 started 단계는 불명(held)이다
    let release = FinishCall {
        attempt: new.attempt.clone(),
        call: call("c1"),
        outcome: Outcome::Released,
    };
    calls(&db).finish(release).await.unwrap();
    calls(&db)
        .start(start_call(&new.attempt, "c1", None))
        .await
        .unwrap();
    let permit = effects(&db)
        .request(step(&new.attempt, "c1", "upload", "{}"))
        .await
        .unwrap();
    assert!(!permit.send);
    assert_eq!(
        (permit.status.effect.clone(), permit.status.state),
        (id.clone(), EffectState::Held)
    );
    assert_eq!(
        db.scalar::<String>("select reason from bot_effects").await,
        "owner_lost"
    );
    assert_eq!(
        effects(&db)
            .unresolved(new.attempt.clone(), call("c1"))
            .await
            .unwrap()
            .len(),
        1
    );

    // 운영자가 미실행을 입증하면(판정 기록) 같은 단계를 현재 세대로 다시 보낼 수 있다
    db.sql("update bot_effects set resolution = 'not_executed'")
        .await;
    for _ in 0..2 {
        let seen = effects(&db)
            .status(new.attempt.clone(), call("c1"), id.clone())
            .await
            .unwrap();
        assert_eq!(seen.state, EffectState::Held);
        assert_eq!(seen.resolution, Some(EffectResolution::NotExecuted));
    }
    let resend = effects(&db)
        .request(step(&new.attempt, "c1", "upload", "{}"))
        .await
        .unwrap();
    assert!(resend.send);
    let after = effects(&db)
        .status(new.attempt.clone(), call("c1"), id.clone())
        .await
        .unwrap();
    assert_eq!(after.state, EffectState::Started);
    assert_eq!(after.resolution, None);
    assert_eq!(
        (resend.status.effect, resend.status.state),
        (id, EffectState::Started)
    );
    assert_eq!(
        db.scalar::<i64>("select generation from bot_effects").await,
        2
    );
}

#[tokio::test]
async fn record_rejects_malformed_outcomes() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = running(&db).await;
    let id = effects(&db)
        .request(step(&run.attempt, "c1", "upload", "{}"))
        .await
        .unwrap()
        .status
        .effect;
    let bad = [
        EffectStatus {
            effect: id.clone(),
            state: EffectState::Confirmed,
            resolution: None,
            receipt: None,
        },
        EffectStatus {
            effect: id.clone(),
            state: EffectState::Held,
            resolution: None,
            receipt: None,
        },
        EffectStatus {
            effect: id.clone(),
            state: EffectState::Rejected,
            resolution: None,
            receipt: Some("x".into()),
        },
        EffectStatus {
            effect: id.clone(),
            state: EffectState::Started,
            resolution: None,
            receipt: None,
        },
    ];
    for status in bad {
        assert!(matches!(
            effects(&db)
                .record(run.attempt.clone(), call("c1"), status)
                .await,
            Err(Error::Invalid)
        ));
    }
    let rejected = EffectStatus {
        effect: id.clone(),
        state: EffectState::Rejected,
        resolution: None,
        receipt: None,
    };
    effects(&db)
        .record(run.attempt.clone(), call("c1"), rejected)
        .await
        .unwrap();
    // 확정 거절은 최종이다
    let late = effects(&db)
        .record(run.attempt.clone(), call("c1"), confirmed(&id, "r"))
        .await;
    assert!(matches!(late, Err(Error::Conflict)));
    let unknown = EffectId("e-none".into());
    assert!(matches!(
        effects(&db)
            .record(run.attempt, call("c1"), confirmed(&unknown, "r"))
            .await,
        Err(Error::Missing)
    ));
}
