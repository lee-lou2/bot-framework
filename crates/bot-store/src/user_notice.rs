//! Fixed origin-conversation notices enqueued when closing a user schedule (no recipient or schedule content).

use crate::{
    db::{R, Tx},
    replies,
};
use bot_core::command::JsonPayload;
use sqlx::{Row, postgres::PgRow};

/// Locks the schedule's immutable `origin_run` before locking the schedule row to keep `run -> schedule` lock order and serialize outbox `seq` assignment.
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

/// Enqueues a fixed notice to a push-capable origin conversation (`direct`/`channel`); caller must hold `lock_origin_run`.
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
