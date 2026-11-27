//! User schedule test helpers.
#![allow(dead_code)]

use super::*;
use bot_core::{cards::LABEL_ARGS, cron::Cron, port::Error, schedule::*};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn at(ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms as u64)
}

pub fn ms(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

pub fn users(db: &TestDb) -> &dyn UserSchedules {
    &db.store
}

pub fn plain(timing: Timing, text: &str) -> ScheduleSpec {
    ScheduleSpec {
        timing,
        instruction: text.into(),
        quiet: false,
        destination: None,
    }
}

pub async fn spec_in(db: &TestDb, millis: i64) -> ScheduleSpec {
    plain(
        Timing::At(at(db.now_ms().await + millis)),
        "Check the schedule",
    )
}

pub fn spec_cron(expression: &str, timezone: &str) -> ScheduleSpec {
    plain(
        Timing::Cron {
            expression: Cron::parse(expression).unwrap(),
            timezone: timezone.into(),
        },
        "Morning briefing",
    )
}

/// Returns the sealed fingerprint of `spec`, or a placeholder when `spec` is invalid so `create` fails with `Invalid`.
pub fn sealed(spec: &ScheduleSpec) -> [u8; 32] {
    spec.seal_create().map_or([1; 32], |seal| seal.sha256)
}

/// Builds a schedule-creation approval card whose `Args` line carries the canonical JSON of `spec`.
pub fn open_create(approval: Approval, spec: &ScheduleSpec) -> OpenApproval {
    open_create_text(approval, &spec.seal_create().unwrap().canonical)
}

/// Builds a schedule-creation approval card from raw `canonical` JSON, wrapping at 480-byte UTF-8 boundaries with `"  "` continuations.
pub fn open_create_text(approval: Approval, canonical: &str) -> OpenApproval {
    let mut lines = vec!["1 event".to_owned()];
    let mut rest = canonical;
    let mut first = true;
    while !rest.is_empty() {
        let prefix = if first {
            format!("{LABEL_ARGS}: ")
        } else {
            "  ".to_owned()
        };
        first = false;
        let mut cut = (480 - prefix.len()).min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        lines.push(format!("{prefix}{head}"));
        rest = tail;
    }
    let card = serde_json::json!({"approval": {"nonce": approval.nonce.0, "title": "Approval request: schedule.create", "lines": lines}});
    OpenApproval {
        approval,
        reasons: vec!["irreversible".into()],
        card: bot_core::command::JsonPayload(card.to_string()),
        delivery: None,
    }
}

pub async fn make(
    db: &TestDb,
    run: &RunContext,
    call_id: &str,
    spec: ScheduleSpec,
) -> Result<ScheduleId, Error> {
    begin_tool(db, run, call_id, "schedule.create", sealed(&spec)).await;
    users(db)
        .create(start_call(&run.attempt, call_id, None), spec)
        .await
}

pub async fn begin_tool(db: &TestDb, run: &RunContext, call_id: &str, name: &str, hash: [u8; 32]) {
    calls(db)
        .begin(run.attempt.clone(), call(call_id), tool(name), hash)
        .await
        .unwrap();
}

pub fn cancel_hash(id: &ScheduleId) -> [u8; 32] {
    let args = serde_json::json!({ "id": id.0 });
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&args).unwrap())
}

pub async fn cancel_with(
    db: &TestDb,
    run: &RunContext,
    call_id: &str,
    id: &ScheduleId,
) -> Result<(), Error> {
    begin_tool(db, run, call_id, "schedule.cancel", cancel_hash(id)).await;
    users(db)
        .cancel(start_call(&run.attempt, call_id, None), id.clone())
        .await
}

pub async fn column<T>(db: &TestDb, id: &ScheduleId, column: &str) -> T
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send + Unpin,
{
    db.scalar::<T>(&format!(
        "select {column} from bot_user_schedules where id = '{}'",
        id.0
    ))
    .await
}

/// Shifts schedule timestamps into the past by `millis` so tests do not wait on wall-clock time.
pub async fn make_due(db: &TestDb, id: &ScheduleId, millis: i64) {
    db.sql(&format!(
        "update bot_user_schedules set at_ms = case when at_ms is null then null else bot_now_ms() - {millis} end,
                cursor_ms = bot_now_ms() - {millis} - 60000, next_ms = bot_now_ms() - {millis}
         where id = '{}'",
        id.0
    ))
    .await;
}
