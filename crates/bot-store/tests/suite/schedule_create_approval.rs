//! Verifies the real broker card and PG approve and consume the exact same sealed schedule arguments.

use crate::support::*;
use bot_broker::{
    service::{BrokerService, Config, Parts},
    sys::{OsEntropy, SystemClock},
    testing::{FakeCatalog, FakeRunner, FakeSealer, RecordingTracer, snapshot},
};
use bot_core::{
    canon,
    cards::LABEL_ARGS,
    command::{CallResult, JsonPayload, ToolCall},
    cron::Cron,
    id::*,
    ledger::ApprovalState,
    port::{Broker, Error},
    schedule::{ScheduleSpec, Timing, UserSchedules},
};
use std::{
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

const OWNER: &str = "local:owner";
const TOOL: &str = "schedule.create";
const ARGS: &str = r#"{"cron":"@daily","instruction":"morning check"}"#;
const SEALED: &str =
    r#"{"cron":"0 0 * * *","instruction":"morning check","quiet":false,"timezone":"Asia/Seoul"}"#;

fn broker(db: &TestDb) -> BrokerService {
    let catalog = Arc::new(FakeCatalog::new());
    catalog.publish(
        snapshot("personal", "r1")
            .timezone("Asia/Seoul")
            .role("owner", &["run", TOOL])
            .member(OWNER, &["owner"])
            .policy_scopes(&["run", TOOL])
            .approve(&[TOOL])
            .agent("main")
            .build(),
    );
    let pg = Arc::new(db.store.clone());
    BrokerService::new(
        Parts {
            catalog,
            ledger: pg.clone(),
            effects: pg.clone(),
            memory: pg.clone(),
            audit: pg.clone(),
            runner: Arc::new(FakeRunner::new()),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: Arc::new(SystemClock),
            entropy: Arc::new(OsEntropy),
        },
        Config::default(),
    )
    .with_schedules(pg)
}

fn request(attempt: &Attempt, args: &str) -> ToolCall {
    ToolCall {
        attempt: attempt.clone(),
        call: call("daily"),
        tool: tool(TOOL),
        args: JsonPayload(args.into()),
    }
}

async fn owner_attempt(db: &TestDb) -> Attempt {
    db.publish().await;
    let mut command = accept("personal", "morning", "morning schedule");
    command.scopes = scopes(&["run", TOOL]);
    command.approver = Some(PrincipalId(OWNER.into()));
    runs(db).accept(command).await.unwrap();
    runs(db).claim().await.unwrap().unwrap().attempt
}

fn card_args(card: &JsonPayload) -> String {
    let prefix = format!("{LABEL_ARGS}: ");
    let value = canon::parse(&card.0).unwrap();
    let args: Vec<_> = value["approval"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|line| line.as_str()?.strip_prefix(&prefix))
        .collect();
    assert_eq!(args.len(), 1);
    args[0].into()
}

#[tokio::test]
async fn the_approved_card_binds_the_timezone_in_postgres_before_the_receipt_is_committed() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let service = broker(&db);
    let first = owner_attempt(&db).await;
    let CallResult::Awaiting(nonce) = service.call(request(&first, ARGS)).await.unwrap() else {
        panic!("expected Awaiting approval")
    };
    let stored = calls(&db).approval(nonce.clone()).await.unwrap();
    assert_eq!(card_args(&stored.card), SEALED);
    assert_eq!(stored.approval.args_sha256, canon::sha256(SEALED));
    service
        .decide(nonce.clone(), PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap().attempt;

    let start = |approval| {
        let mut start = start_call(&resumed, "daily", approval);
        start.risk = stored.approval.risk;
        start.tool_revision = stored.approval.tool_revision.clone();
        start
    };
    let spec = |zone: &str| ScheduleSpec {
        timing: Timing::Cron {
            expression: Cron::parse("@daily").unwrap(),
            timezone: zone.into(),
        },
        instruction: "morning check".into(),
        quiet: false,
        destination: None,
    };
    assert!(matches!(
        db.store.create(start(None), spec("Asia/Seoul")).await,
        Err(Error::Denied | Error::Conflict)
    ));
    let approved = start(Some((stored.approval.clone(), stored.reasons)));
    assert!(matches!(
        db.store.create(approved, spec("UTC")).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        calls(&db).approval(nonce.clone()).await.unwrap().state,
        ApprovalState::Approved
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        0
    );

    let CallResult::Done(receipt) = service.call(request(&resumed, ARGS)).await.unwrap() else {
        panic!("expected Done receipt after approval")
    };
    assert_eq!(
        calls(&db).approval(nonce).await.unwrap().state,
        ApprovalState::Consumed
    );
    assert_eq!(
        db.scalar::<String>("select timezone from bot_user_schedules")
            .await,
        "Asia/Seoul"
    );
    let CallResult::Done(repeated) = service.call(request(&resumed, SEALED)).await.unwrap() else {
        panic!("expected Done receipt on identical redelivery")
    };
    assert_eq!(repeated, receipt);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_user_schedules")
            .await,
        1
    );
}

#[tokio::test]
async fn fractional_seconds_on_the_card_match_the_millisecond_actually_scheduled() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let service = broker(&db);
    let first = owner_attempt(&db).await;
    let ms = db.now_ms().await / 1_000 * 1_000 + 60_250;
    let expected_at =
        bot_core::schedule::utc::format_utc(UNIX_EPOCH + Duration::from_millis(ms as u64));
    let raw_at = expected_at.replace(".250Z", ".250999999Z");
    assert_ne!(raw_at, expected_at);
    let raw = serde_json::json!({"instruction":"check", "at":raw_at}).to_string();
    let CallResult::Awaiting(nonce) = service.call(request(&first, &raw)).await.unwrap() else {
        panic!("expected Awaiting approval")
    };
    let stored = calls(&db).approval(nonce.clone()).await.unwrap();
    let shown = card_args(&stored.card);
    assert_eq!(
        canon::parse(&shown).unwrap(),
        serde_json::json!({"at":expected_at, "instruction":"check", "quiet":false})
    );
    assert_eq!(canon::sha256(&shown), stored.approval.args_sha256);
    service
        .decide(nonce, PrincipalId(OWNER.into()), true)
        .await
        .unwrap();
    let resumed = runs(&db).claim().await.unwrap().unwrap().attempt;
    assert!(matches!(
        service.call(request(&resumed, &shown)).await.unwrap(),
        CallResult::Done(_)
    ));
    assert_eq!(
        db.scalar::<i64>("select at_ms from bot_user_schedules")
            .await,
        ms
    );
}
