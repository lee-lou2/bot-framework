//! Hold occurrence IDs: fresh ID per hold entry, stable while held, and idempotent resolution by `(id, target, operator, evidence hash)`.

use crate::support::{admin::*, *};
use bot_core::{
    admin::*,
    command::*,
    id::*,
    intake::{DeliveryOutcome, DeliveryScope, PushTransportId},
    ledger::*,
    port::*,
    state::{ConversationKind, EffectState},
};

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
        .expect("single-step held call must have a hold ID");
    assert_eq!(
        second[0].hold_id,
        Some(id),
        "same hold must keep the same ID across reads"
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
    assert_eq!(
        holds[0].hold_id, None,
        "multi-step call has no call-level hold ID"
    );
    let by_effect = |id: &EffectId| holds[0].steps.iter().find(|s| s.effect == *id).unwrap();
    assert_eq!(
        by_effect(&steps[0]).hold_id,
        None,
        "confirmed step is not judgeable"
    );
    let judgeable = by_effect(&steps[1])
        .hold_id
        .clone()
        .expect("held step must have its own hold ID");
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
        "run detail must carry the same hold ID"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_holds where effect is null")
            .await,
        0,
        "multi-step call must not create a call-level hold row"
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

    let mut unknown = target.clone();
    unknown.hold_id = HoldId("hold-none".into());
    assert!(matches!(
        resolve(&db, unknown, not_executed("missing")).await,
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
            resolve(&db, wrong, not_executed("missing")).await,
            Err(Error::Conflict)
        ));
    }
    assert_eq!(audits(&db).await, 0);

    assert_eq!(
        resolve(&db, target.clone(), not_executed("not at recipient"))
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
        db.scalar::<i64>(
            "select count(*) from bot_holds t where t::text like '%not at recipient%' "
        )
        .await,
        0,
        "raw evidence text must not be stored"
    );

    assert_eq!(
        resolve(&db, target.clone(), not_executed("not at recipient"))
            .await
            .unwrap(),
        HoldDecision::Repeated
    );
    assert_eq!(audits(&db).await, 1);
    assert!(matches!(
        resolve(&db, target.clone(), not_executed("other evidence")).await,
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
            not_executed("not at recipient")
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
    resolve(&db, h1.clone(), not_executed("not at recipient"))
        .await
        .unwrap();

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
    assert_ne!(
        h1.hold_id, h2.hold_id,
        "re-entering hold must allocate a new ID"
    );

    // Replaying H1 returns Repeated without affecting H2.
    assert_eq!(
        resolve(&db, h1, not_executed("not at recipient"))
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
                answers: vec![JsonPayload(r#"{"text":"first"}"#.into())],
            },
        )
        .await
        .unwrap();
    let slack = DeliveryScope::all(PushTransportId::SLACK);
    let sent = (&db.store as &dyn bot_core::intake::Outbox)
        .claim(&slack, 1)
        .await
        .unwrap()
        .remove(0);
    (&db.store as &dyn bot_core::intake::Outbox)
        .begin(sent.key.clone(), Revision("r1".into()))
        .await
        .unwrap();
    db.sql("update bot_outbox set lease_until_ms = bot_now_ms() - 1")
        .await;
    // Next claim sweeps the expired in-flight send into held state.
    (&db.store as &dyn bot_core::intake::Outbox)
        .claim(&slack, 1)
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
        "late receipt must clear the hold"
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
                answers: vec![JsonPayload(r#"{"text":"first"}"#.into())],
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
        "update bot_runs set private = true, reply_transport = null, state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null where id = '{}'",
        private_run.attempt.run.0
    ))
    .await;
    db.store
        .migrate()
        .await
        .expect("0011 migration should apply cleanly");

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
        "only unresolved holds should be backfilled, without call-level rows for multi-step calls"
    );
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
                answers: vec![JsonPayload(r#"{"text":"first"}"#.into())],
            },
        )
        .await
        .unwrap();
    db.sql("create table probe_holds (id text primary key, reason text not null)")
        .await;

    // Repeatedly cycle the same reply into new holds with distinct reasons and record `(id, reason)` atomically.
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
            let id = hold.hold_id.expect("open reply hold must have a hold ID");
            let paired: String = sqlx::query_scalar("select reason from probe_holds where id = $1")
                .bind(&id.0)
                .fetch_one(db.store.pool())
                .await
                .unwrap();
            assert_eq!(
                hold.reason, paired,
                "hold listing mixed reason from one hold with ID from another"
            );
            checked += 1;
        }
    }
    writer.await.unwrap();
    assert!(checked > 0, "expected at least one concurrent read");
}
