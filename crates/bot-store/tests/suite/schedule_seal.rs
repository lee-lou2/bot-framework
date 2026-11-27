//! Schedule creation seals and verifies the approved spec fingerprint; general `start` rejects tools with dedicated transactions.

use crate::support::{delivery, schedule::*, *};
use bot_core::{id::*, ledger::*, port::*, schedule::ScheduleSpec, state::*};

const OWNER: &str = "local:owner";

async fn approved(
    db: &TestDb,
    run: &RunContext,
    call_id: &str,
    spec: &ScheduleSpec,
) -> (Attempt, (Approval, Vec<String>)) {
    let hash = sealed(spec);
    let for_create = |attempt: &Attempt, nonce: &str| Approval {
        tool: tool("schedule.create"),
        args_sha256: hash,
        ..binding(attempt, call_id, nonce)
    };
    calls(db)
        .open_approval(open_create(for_create(&run.attempt, "n1"), spec))
        .await
        .unwrap();
    calls(db)
        .decide(Decision {
            nonce: ApprovalId("n1".into()),
            actor: PrincipalId(OWNER.into()),
            allow: true,
        })
        .await
        .unwrap();
    let resumed = runs(db).claim().await.unwrap().unwrap();
    let approval = (
        for_create(&resumed.attempt, "n1"),
        vec!["irreversible".to_owned()],
    );
    (resumed.attempt, approval)
}

async fn state_of(db: &TestDb) -> ApprovalState {
    calls(db)
        .approval(ApprovalId("n1".into()))
        .await
        .unwrap()
        .state
}

#[tokio::test]
async fn only_the_sealed_spec_is_created_and_every_other_spec_leaves_the_approval_unspent() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_cron("0 9 * * *", "Asia/Seoul");
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let (attempt, approval) = approved(&db, &run, "c1", &spec).await;
    let start = || start_call(&attempt, "c1", Some(approval.clone()));

    let mut other_zone = spec.clone();
    other_zone.timing = spec_cron("0 9 * * *", "UTC").timing;
    let mut other_text = spec.clone();
    other_text.instruction = "other instruction".into();
    let mut quiet = spec.clone();
    quiet.quiet = true;
    let one_shot = spec_in(&db, 60_000).await;
    for (name, other) in [
        ("zone", other_zone),
        ("text", other_text),
        ("quiet", quiet),
        ("at", one_shot),
    ] {
        assert!(
            matches!(
                users(&db).create(start(), other).await,
                Err(Error::Conflict)
            ),
            "{name}"
        );
        assert_eq!(state_of(&db).await, ApprovalState::Approved, "{name}");
    }
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'schedule.create'")
            .await,
        0
    );

    let id = users(&db).create(start(), spec.clone()).await.unwrap();
    assert_eq!(state_of(&db).await, ApprovalState::Consumed);
    assert_eq!(column::<String>(&db, &id, "timezone").await, "Asia/Seoul");

    let redelivered = start_call(&attempt, "c1", None);
    assert_eq!(
        users(&db).create(redelivered, spec.clone()).await.unwrap(),
        id
    );
    let mut changed = spec;
    changed.instruction = "changed after completion".into();
    assert!(matches!(
        users(&db)
            .create(start_call(&attempt, "c1", None), changed)
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        1
    );
}

#[tokio::test]
async fn a_call_sealed_with_an_old_fingerprint_can_no_longer_create() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    begin_tool(&db, &run, "c1", "schedule.create", [1; 32]).await;
    let spec = spec_in(&db, 60_000).await;
    assert!(matches!(
        users(&db)
            .create(start_call(&run.attempt, "c1", None), spec)
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );
}

#[tokio::test]
async fn the_general_start_refuses_every_tool_that_has_a_dedicated_transaction() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    for (n, name) in ["schedule.create", "schedule.cancel", "memory.share"]
        .into_iter()
        .enumerate()
    {
        let id = format!("d{n}");
        begin_tool(&db, &run, &id, name, [1; 32]).await;
        assert!(
            matches!(
                calls(&db).start(start_call(&run.attempt, &id, None)).await,
                Err(Error::Denied)
            ),
            "{name}"
        );
        assert_eq!(
            db.scalar::<String>(&format!("select state from bot_calls where call = '{id}'"))
                .await,
            "open",
            "{name}: denied start must leave call open"
        );
    }
    let delivery = delivery::destination_run_as(&db, "r2", ConversationKind::Direct).await;
    let draft = delivery::replies(&db)
        .draft(
            delivery.attempt.clone(),
            Some(delivery::answers(&["reply"])),
        )
        .await
        .unwrap()
        .unwrap();
    calls(&db)
        .begin(
            delivery.attempt.clone(),
            call("schedule-deliver"),
            tool("schedule.deliver"),
            draft.sha256().unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        calls(&db)
            .start(start_call(&delivery.attempt, "schedule-deliver", None))
            .await,
        Err(Error::Denied)
    ));
}

#[tokio::test]
async fn an_approval_must_be_the_one_the_recorded_call_points_at_and_match_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = run_in(&db, "r1", OWNER, ConversationKind::Direct).await;
    let spec = spec_cron("0 9 * * *", "Asia/Seoul");
    let hash = sealed(&spec);
    begin_tool(&db, &run, "c1", "schedule.create", hash).await;
    let (attempt, approval) = approved(&db, &run, "c1", &spec).await;
    assert!(matches!(
        users(&db)
            .create(start_call(&attempt, "c1", None), spec.clone())
            .await,
        Err(Error::Denied)
    ));
    db.sql("update bot_calls set approval = 'other' where call = 'c1'")
        .await;
    assert!(matches!(
        users(&db)
            .create(
                start_call(&attempt, "c1", Some(approval.clone())),
                spec.clone()
            )
            .await,
        Err(Error::Denied)
    ));
    db.sql("update bot_calls set approval = 'n1' where call = 'c1'")
        .await;
    db.sql(
        "update bot_approvals set args_sha256 = decode(repeat('09', 32), 'hex') where nonce = 'n1'",
    )
    .await;
    let mut forged = approval.clone();
    forged.0.args_sha256 = [9; 32];
    assert!(matches!(
        users(&db)
            .create(start_call(&attempt, "c1", Some(forged)), spec.clone())
            .await,
        Err(Error::Conflict)
    ));
    assert_eq!(state_of(&db).await, ApprovalState::Approved);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );

    db.sql("update bot_approvals set args_sha256 = (select args_sha256 from bot_calls where call = 'c1') where nonce = 'n1'")
        .await;
    users(&db)
        .create(start_call(&attempt, "c1", Some(approval)), spec)
        .await
        .unwrap();
    assert_eq!(state_of(&db).await, ApprovalState::Consumed);
}
