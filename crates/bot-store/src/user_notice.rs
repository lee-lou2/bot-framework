//! 사용자 예약을 닫을 때 원래 대화에 남기는 고정 안내(공개해도 되는 문장만: 수신자·예약 내용은 넣지 않는다).
//!
//! 닫는 커밋과 같은 트랜잭션에서, 예약을 만든 run의 outbox 다음 순번으로 예약의 원래 대화에 한 건 쌓는다. 같은 종료를 다시 받으면 이미 닫힌 예약이라
//! 이 함수까지 오지 않으므로 안내는 한 번뿐이다. 푸시 대화(direct·channel)가 아닌 원래 대화(CLI·예약)에는 보낼 곳이 없어 쌓지 않는다.

use crate::{
    db::{R, Tx},
    replies,
};
use bot_core::command::JsonPayload;
use sqlx::{Row, postgres::PgRow};

/// 안내가 들어갈 run(예약을 만든 run)의 행을 잠근다. **예약 행을 잠그기 전에** 부른다: 이 기계의 잠금 순서는 run → 예약이다(취소 호출은 자기 run을 먼저 잠그고 예약 행을
/// 잠근다). 예약 행을 잡은 채 run을 기다리면 취소와 서로 기다리는 교착이 된다. 순번은 이 run 잠금 아래에서 정한다(`push`): 일반 회신(`finish`)도 run을 잠근 채
/// 순번을 정하므로 같은 run의 두 쓰기가 같은 순번을 고르지 않는다. 만든 run은 예약 행에서 읽되 잠그지 않는다(바뀌지 않는 열이다).
pub(crate) async fn lock_origin_run(tx: &mut Tx<'_>, profile: &str, schedule: &str) -> R<()> {
    let run: Option<String> = sqlx::query_scalar(
        "select origin_run from bot_user_schedules where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(schedule)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(run) = run {
        sqlx::query("select 1 from bot_runs where profile = $1 and id = $2 for update")
            .bind(profile)
            .bind(&run)
            .fetch_optional(&mut **tx)
            .await?;
    }
    Ok(())
}

/// 안내 한 건을 쌓고, 보류 감사가 생기면 `audits`에 모은다.
/// `schedule`은 잠근 `bot_user_schedules` 행이고, 호출자가 `lock_origin_run`으로 그 run의 행을 이미 잠갔다(순번은 그 잠금 아래에서 정한다).
pub(crate) async fn push_into(
    tx: &mut Tx<'_>,
    schedule: &PgRow,
    text: &str,
    audits: &mut Vec<crate::audit::OwnedEntry>,
) -> R<()> {
    let kind: String = schedule.get("origin_kind");
    if !matches!(kind.as_str(), "direct" | "channel") {
        return Ok(());
    }
    let body = replies::answer(&JsonPayload(
        serde_json::json!({ "text": text }).to_string(),
    ))?;
    let (profile, run): (String, String) = (schedule.get("profile"), schedule.get("origin_run"));
    let inserted: Option<(String, String)> = sqlx::query_as(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, thread, body, state, reason, transport)
         select $1, $2, n.seq, 'reply:' || $2 || ':' || n.seq::text, $3, $4, $5, $6, $7, $8,
                case when coalesce(r.reply_transport, '') = '' then 'held' else 'pending' end,
                case when coalesce(r.reply_transport, '') = '' then 'missing_transport' else null end,
                coalesce(r.reply_transport, '')
         from bot_runs r, lateral (select coalesce(max(o.seq) + 1, 0) as seq from bot_outbox o where o.profile = $1 and o.run = $2) n
         where r.profile = $1 and r.id = $2
         returning call, state",
    )
    .bind(&profile)
    .bind(&run)
    .bind(schedule.get::<String, _>("origin_binding"))
    .bind(schedule.get::<String, _>("owner"))
    .bind(&kind)
    .bind(schedule.get::<String, _>("origin_id"))
    .bind(schedule.get::<Option<String>, _>("origin_thread"))
    .bind(body)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((call, state)) = inserted
        && state == "held"
    {
        crate::finish::hold_missing_transport_into(tx, &profile, &run, &call, audits).await?;
    }
    Ok(())
}
