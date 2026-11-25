//! 사용자 예약 시험 도구.
#![allow(dead_code)]

use super::*;
use bot_core::{cron::Cron, port::Error, schedule::*};
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

/// DB 시각으로부터 `millis` 뒤 한 번.
pub async fn spec_in(db: &TestDb, millis: i64) -> ScheduleSpec {
    plain(
        Timing::At(at(db.now_ms().await + millis)),
        "일정을 확인해 줘",
    )
}

pub fn spec_cron(expression: &str, timezone: &str) -> ScheduleSpec {
    plain(
        Timing::Cron {
            expression: Cron::parse(expression).unwrap(),
            timezone: timezone.into(),
        },
        "아침 브리핑",
    )
}

/// 승인 카드가 보여 주는 명세의 봉인 지문(만들 수 없는 명세는 아무 값이다: 생성이 `Invalid`로 거절해야 한다).
pub fn sealed(spec: &ScheduleSpec) -> [u8; 32] {
    spec.seal_create().map_or([1; 32], |seal| seal.sha256)
}

/// 봉인한 명세의 승인 카드: `인자` 행에 정규 인자 JSON이 그대로 실린다(저장소가 카드에 보인 인자·승인·호출 지문을 대조한다).
/// `approval.args_sha256`은 `sealed(spec)`이어야 한다.
pub fn open_create(approval: Approval, spec: &ScheduleSpec) -> OpenApproval {
    open_create_text(approval, &spec.seal_create().unwrap().canonical)
}

/// 정규 인자 문자열을 직접 주는 같은 카드(어긋난 카드를 만들어 보려는 시험용). 긴 줄은 480바이트씩 이어 쓴다(이어지는 줄은 공백 두 칸으로 시작).
pub fn open_create_text(approval: Approval, canonical: &str) -> OpenApproval {
    let mut lines = vec!["일정 1건".to_owned()];
    let mut rest = canonical;
    let mut first = true;
    while !rest.is_empty() {
        let prefix = if first { "인자: " } else { "  " };
        first = false;
        let mut cut = (480 - prefix.len()).min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        lines.push(format!("{prefix}{head}"));
        rest = tail;
    }
    let card = serde_json::json!({"approval": {"nonce": approval.nonce.0, "title": "예약 생성 승인", "lines": lines}});
    OpenApproval {
        approval,
        reasons: vec!["irreversible".into()],
        card: bot_core::command::JsonPayload(card.to_string()),
        delivery: None,
    }
}

/// 새 호출을 열고 예약을 만든다.
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

/// 정확한 도구·봉인 지문으로 호출을 연다.
pub async fn begin_tool(db: &TestDb, run: &RunContext, call_id: &str, name: &str, hash: [u8; 32]) {
    calls(db)
        .begin(run.attempt.clone(), call(call_id), tool(name), hash)
        .await
        .unwrap();
}

/// 취소 호출의 봉인 지문(브로커와 같은 규칙: 인자 `{"id": …}`의 JCS SHA-256).
pub fn cancel_hash(id: &ScheduleId) -> [u8; 32] {
    let args = serde_json::json!({ "id": id.0 });
    bot_core::canon::sha256(&bot_core::canon::canonicalize(&args).unwrap())
}

/// 새 호출을 열고 예약을 취소한다.
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

/// 예약 행의 열 하나를 읽는다.
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

/// 예약을 이미 지난 시각으로 옮긴다(테스트에서 시간을 기다리지 않는다): 예정 시각·커서·다음 확인을 모두 `millis` 전으로.
pub async fn make_due(db: &TestDb, id: &ScheduleId, millis: i64) {
    db.sql(&format!(
        "update bot_user_schedules set at_ms = case when at_ms is null then null else bot_now_ms() - {millis} end,
                cursor_ms = bot_now_ms() - {millis} - 60000, next_ms = bot_now_ms() - {millis}
         where id = '{}'",
        id.0
    ))
    .await;
}
