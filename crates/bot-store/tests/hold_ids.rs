//! 보류 발생 ID: 보류로 들어갈 때마다 새 ID, 같은 보류를 유지하는 동안은 같은 ID, 판정은 ID·대상·운영자·증거 지문으로 한 번만 커밋한다.

mod support;

use bot_core::{
    admin::*,
    command::*,
    id::*,
    intake::DeliveryOutcome,
    ledger::*,
    port::*,
    state::{ConversationKind, EffectState},
};
use support::{admin::*, *};

const ADMIN: &str = "local:admin";

fn actor() -> PrincipalId {
    PrincipalId(ADMIN.into())
}

fn not_executed(evidence: &str) -> HoldVerdict {
    HoldVerdict::NotExecuted {
        evidence: evidence.into(),
    }
}

async fn resolve(
    db: &TestDb,
    target: HoldRef,
    verdict: HoldVerdict,
) -> Result<HoldDecision, Error> {
    AdminAct::resolve_hold(&db.store, actor(), target, verdict).await
}

async fn audits(db: &TestDb) -> i64 {
    db.scalar::<i64>("select count(*) from bot_audit where action = 'hold.resolve'")
        .await
}

/// 단일 쓰기 호출(c1)을 시작해 보류로 넘긴다.
async fn hold_c1(db: &TestDb, at: &Attempt) {
    calls(db).start(start_call(at, "c1", None)).await.unwrap();
    calls(db)
        .finish(FinishCall {
            attempt: at.clone(),
            call: call("c1"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn a_hold_gets_an_id_on_entry_and_keeps_it_while_it_lasts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "single").await;
    begin_call(&db, &run.attempt, "c1").await;
    hold_c1(&db, &run.attempt).await;

    let first = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    let second = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    let id = first[0]
        .hold_id
        .clone()
        .expect("단계 없는 보류는 호출이 ID를 가진다");
    assert_eq!(
        second[0].hold_id,
        Some(id),
        "다시 읽어도 같은 보류는 같은 ID다"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_holds").await, 1);
}

#[tokio::test]
async fn each_judgeable_step_has_its_own_id_and_the_whole_call_has_none() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "multi").await;
    begin_call(&db, &run.attempt, "c1").await;
    calls(&db)
        .start(start_call(&run.attempt, "c1", None))
        .await
        .unwrap();
    let mut steps = Vec::new();
    for name in ["upload", "notify"] {
        let permit = effects(&db)
            .request(EffectRequest {
                attempt: run.attempt.clone(),
                call: call("c1"),
                step: name.into(),
                args: JsonPayload("{}".into()),
            })
            .await
            .unwrap();
        steps.push(permit.status.effect);
    }
    // 첫 단계는 영수증을 받아 확정, 둘째는 결과 불명으로 남은 채 호출이 보류된다
    effects(&db)
        .record(
            run.attempt.clone(),
            call("c1"),
            EffectStatus {
                effect: steps[0].clone(),
                state: EffectState::Confirmed,
                resolution: None,
                receipt: Some("rcpt-1".into()),
            },
        )
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

    let holds = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].hold_id, None, "묶음 호출 보류에는 ID가 없다");
    let by_effect = |id: &EffectId| holds[0].steps.iter().find(|s| s.effect == *id).unwrap();
    assert_eq!(
        by_effect(&steps[0]).hold_id,
        None,
        "확정된 단계는 판정 대상이 아니다"
    );
    let judgeable = by_effect(&steps[1])
        .hold_id
        .clone()
        .expect("보류 단계는 자기 ID를 가진다");
    let detail = AdminRead::detail(
        &db.store,
        &operator(),
        run.attempt.profile.clone(),
        run.attempt.run.clone(),
    )
    .await
    .unwrap();
    let in_detail = detail
        .effects
        .iter()
        .find(|e| e.effect == steps[1])
        .unwrap();
    assert_eq!(
        in_detail.hold_id,
        Some(judgeable),
        "run 상세도 같은 ID를 싣는다"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where effect is null")
            .await,
        0,
        "단계가 있는 호출은 호출 단위 발생을 만들지 않는다"
    );
}

#[tokio::test]
async fn a_decision_commits_once_and_repeats_write_nothing() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "once").await;
    begin_call(&db, &run.attempt, "c1").await;
    hold_c1(&db, &run.attempt).await;
    let target = hold_ref(&db, HoldKind::Tool, &run.attempt.run, &call("c1"), None).await;

    // 모르는 ID는 Missing, 다른 run·호출을 가리키면 Conflict, 아무것도 쓰지 않는다
    let mut unknown = target.clone();
    unknown.hold_id = HoldId("hold-none".into());
    assert!(matches!(
        resolve(&db, unknown, not_executed("없음")).await,
        Err(Error::Missing)
    ));
    for tweak in [
        |t: &mut HoldRef| t.call = CallId("c2".into()),
        |t: &mut HoldRef| t.run = RunId("other".into()),
        |t: &mut HoldRef| t.profile = ProfileId("work".into()),
        |t: &mut HoldRef| t.effect = Some(EffectId("e1".into())),
    ] {
        let mut wrong = target.clone();
        tweak(&mut wrong);
        assert!(matches!(
            resolve(&db, wrong, not_executed("없음")).await,
            Err(Error::Conflict)
        ));
    }
    assert_eq!(audits(&db).await, 0);

    assert_eq!(
        resolve(&db, target.clone(), not_executed("수신처에 없음"))
            .await
            .unwrap(),
        HoldDecision::Applied
    );
    assert_eq!(audits(&db).await, 1);
    assert_eq!(state_of_run(&db, &run.attempt.run).await, "ready");
    let (operator_id, verdict, hash_len): (String, String, i32) = sqlx::query_as(
        "select operator, verdict, octet_length(evidence_sha256) from bot_holds where closed_ms is not null",
    )
    .fetch_one(db.store.pool())
    .await
    .unwrap();
    assert_eq!(
        (operator_id.as_str(), verdict.as_str(), hash_len),
        (ADMIN, "not_executed", 32)
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds t where t::text like '%수신처에 없음%' ")
            .await,
        0,
        "증거 원문은 저장하지 않는다"
    );

    // 같은 운영자·같은 판정·같은 증거: Repeated, 감사·상태 변화 없음
    assert_eq!(
        resolve(&db, target.clone(), not_executed("수신처에 없음"))
            .await
            .unwrap(),
        HoldDecision::Repeated
    );
    assert_eq!(audits(&db).await, 1);
    // 값·운영자가 다르면 Conflict
    assert!(matches!(
        resolve(&db, target.clone(), not_executed("다른 증거")).await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        resolve(
            &db,
            target.clone(),
            HoldVerdict::Confirmed {
                receipt: "r-1".into()
            }
        )
        .await,
        Err(Error::Conflict)
    ));
    assert!(matches!(
        AdminAct::resolve_hold(
            &db.store,
            PrincipalId("local:other".into()),
            target,
            not_executed("수신처에 없음")
        )
        .await,
        Err(Error::Conflict)
    ));
    assert_eq!(audits(&db).await, 1);
}

#[tokio::test]
async fn an_old_id_only_repeats_after_the_same_target_is_held_again() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased(&db, "rehold").await;
    begin_call(&db, &run.attempt, "c1").await;
    hold_c1(&db, &run.attempt).await;
    let h1 = hold_ref(&db, HoldKind::Tool, &run.attempt.run, &call("c1"), None).await;
    resolve(&db, h1.clone(), not_executed("수신처에 없음"))
        .await
        .unwrap();

    // 새 claim이 같은 호출 ID를 이어서 다시 보내다 또 보류된다
    let second = runs(&db).claim().await.unwrap().unwrap();
    calls(&db)
        .finish(FinishCall {
            attempt: second.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Released,
        })
        .await
        .unwrap();
    hold_c1(&db, &second.attempt).await;
    let h2 = hold_ref(&db, HoldKind::Tool, &second.attempt.run, &call("c1"), None).await;
    assert_ne!(h1.hold_id, h2.hold_id, "새 보류 진입은 새 ID다");

    // H1을 다시 보내도 Repeated이고 H2는 건드리지 않는다
    assert_eq!(
        resolve(&db, h1, not_executed("수신처에 없음"))
            .await
            .unwrap(),
        HoldDecision::Repeated
    );
    assert_eq!(audits(&db).await, 1);
    assert_eq!(state_of_run(&db, &second.attempt.run).await, "waiting");
    let listed = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].hold_id, Some(h2.hold_id.clone()));
    // H2의 새 판정만 두 번째 전이다
    assert_eq!(
        resolve(
            &db,
            h2,
            HoldVerdict::Confirmed {
                receipt: "evt-2".into()
            }
        )
        .await
        .unwrap(),
        HoldDecision::Applied
    );
    assert_eq!(audits(&db).await, 2);
}

#[tokio::test]
async fn a_reply_hold_closed_by_a_late_receipt_cannot_be_judged_afterwards() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased_in(&db, "reply", ConversationKind::Direct).await;
    runs(&db)
        .finish(
            run.attempt.clone(),
            Finish {
                result: JsonPayload("{}".into()),
                answers: vec![JsonPayload(r#"{"text":"첫째"}"#.into())],
            },
        )
        .await
        .unwrap();
    let dm = [ConversationKind::Direct];
    let sent = (&db.store as &dyn bot_core::intake::Outbox)
        .claim(&dm, 1)
        .await
        .unwrap()
        .remove(0);
    (&db.store as &dyn bot_core::intake::Outbox)
        .begin(sent.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1")
        .await;
    // 리스를 잃은 전송은 다음 claim 때 보류가 된다
    (&db.store as &dyn bot_core::intake::Outbox)
        .claim(&dm, 1)
        .await
        .unwrap();
    let call_id = CallId(db.scalar::<String>("select call from bot_outbox").await);
    let target = hold_ref(&db, HoldKind::Reply, &run.attempt.run, &call_id, None).await;

    (&db.store as &dyn bot_core::intake::Outbox)
        .settle(
            sent.key,
            DeliveryOutcome::Confirmed {
                receipt: "1700.1".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        AdminRead::holds(&db.store, &operator(), None)
            .await
            .unwrap()
            .is_empty(),
        "늦게 온 영수증으로 보류가 풀렸다"
    );
    assert!(matches!(
        resolve(
            &db,
            target,
            HoldVerdict::Confirmed {
                receipt: "x".into()
            }
        )
        .await,
        Err(Error::Conflict)
    ));
    assert_eq!(audits(&db).await, 0);
}

#[tokio::test]
async fn upgrading_gives_every_unresolved_old_hold_an_id_and_skips_judged_ones() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 구판에서 보류가 된 것들: 단일 쓰기(c1), 이미 판정한 단일 쓰기(c2), 다단계(c3: 한 단계는 미실행으로 판정), 회신 발송, 민감 DM
    let run = leased(&db, "legacy").await;
    for id in ["c1", "c2", "c3"] {
        begin_call(&db, &run.attempt, id).await;
    }
    calls(&db)
        .start(start_call(&run.attempt, "c3", None))
        .await
        .unwrap();
    let mut steps = Vec::new();
    for name in ["upload", "notify"] {
        let permit = effects(&db)
            .request(EffectRequest {
                attempt: run.attempt.clone(),
                call: call("c3"),
                step: name.into(),
                args: JsonPayload("{}".into()),
            })
            .await
            .unwrap();
        steps.push(permit.status.effect);
    }
    calls(&db)
        .finish(FinishCall {
            attempt: run.attempt.clone(),
            call: call("c3"),
            outcome: Outcome::Held("timeout".into()),
        })
        .await
        .unwrap();
    let reply_run = leased_in(&db, "reply", ConversationKind::Direct).await;
    runs(&db)
        .finish(
            reply_run.attempt.clone(),
            Finish {
                result: JsonPayload("{}".into()),
                answers: vec![JsonPayload(r#"{"text":"첫째"}"#.into())],
            },
        )
        .await
        .unwrap();
    let private_run = leased_in(&db, "dm", ConversationKind::Direct).await;

    rewind_holds(&db).await;
    db.sql("update bot_calls set state = 'held' where call in ('c1', 'c2')")
        .await;
    db.sql("update bot_calls set resolution = 'not_executed' where call = 'c2'")
        .await;
    db.sql(&format!(
        "update bot_effects set resolution = 'not_executed' where id = '{}'",
        steps[0].0
    ))
    .await;
    db.sql("update bot_outbox set state = 'held'").await;
    db.sql(&format!(
        "update bot_runs set private = true, state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null where id = '{}'",
        private_run.attempt.run.0
    ))
    .await;
    db.store
        .migrate()
        .await
        .expect("0011이 제자리에서 올라간다");

    let found: Vec<(String, String, Option<String>)> =
        sqlx::query_as("select kind, call, effect from bot_holds where closed_ms is null order by kind, call, effect")
            .fetch_all(db.store.pool())
            .await
            .unwrap();
    let reply_call = db.scalar::<String>("select call from bot_outbox").await;
    assert_eq!(
        found,
        [
            (
                "private".into(),
                format!("private:{}", private_run.attempt.run.0),
                None
            ),
            ("reply".into(), reply_call, None),
            ("tool".into(), "c1".into(), None),
            ("tool".into(), "c3".into(), Some(steps[1].0.clone())),
        ],
        "판정하지 않은 호출·단계·회신·민감 DM만, 단계 있는 호출은 호출 단위 발생 없이"
    );
    // 이어서 운영자 목록이 같은 대상에 ID를 싣는다
    let listed = AdminRead::holds(&db.store, &operator(), None)
        .await
        .unwrap();
    assert!(
        listed
            .iter()
            .all(|h| h.hold_id.is_some() || !h.steps.is_empty())
    );
    assert_eq!(
        listed.iter().filter(|h| h.kind == HoldKind::Tool).count(),
        2
    );
}

#[tokio::test]
async fn a_listing_never_mixes_one_holds_reason_with_the_next_holds_id() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = leased_in(&db, "race", ConversationKind::Direct).await;
    runs(&db)
        .finish(
            run.attempt.clone(),
            Finish {
                result: JsonPayload("{}".into()),
                answers: vec![JsonPayload(r#"{"text":"첫째"}"#.into())],
            },
        )
        .await
        .unwrap();
    db.sql("create table probe_holds (id text primary key, reason text not null)")
        .await;

    // 같은 회신을 사유가 다른 보류로 계속 다시 들여보낸다(들어갈 때마다 발생 ID가 바뀐다). 사유와 ID는 한 트랜잭션에서 짝으로 남긴다
    let pool = db.store.pool().clone();
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    let writer = tokio::spawn(async move {
        for n in 0..400 {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("update bot_outbox set state = 'pending', reason = null")
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query(&format!(
                "update bot_outbox set state = 'held', reason = 'cycle-{n}'"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query(&format!(
                "insert into probe_holds select id, 'cycle-{n}' from bot_holds where kind = 'reply' and closed_ms is null"
            ))
            .execute(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        flag.store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let mut checked = 0;
    while !done.load(std::sync::atomic::Ordering::SeqCst) {
        for hold in AdminRead::holds(&db.store, &operator(), None)
            .await
            .unwrap()
        {
            if hold.kind != HoldKind::Reply {
                continue;
            }
            let id = hold.hold_id.expect("열린 회신 보류는 발생 ID를 갖는다");
            let paired: String = sqlx::query_scalar("select reason from probe_holds where id = $1")
                .bind(&id.0)
                .fetch_one(db.store.pool())
                .await
                .unwrap();
            assert_eq!(
                hold.reason, paired,
                "한 카드에 다른 보류의 사유와 ID가 섞였다"
            );
            checked += 1;
        }
    }
    writer.await.unwrap();
    assert!(checked > 0, "경합 중에 목록을 한 번도 읽지 못했다");
}
