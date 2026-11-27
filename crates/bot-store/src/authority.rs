//! Verifies pre-approval grants and human-origin evidence during call start and effect write authorization.
//!
//! Lock order: active set (held by caller) -> run -> call -> grant (share) -> pause (share) -> usage -> evidence use -> audit head.

use crate::{
    codec::*,
    db::{R, Tx},
    finish::RunRow,
};
use bot_core::{
    command::JsonPayload,
    grant::{Authority, reason},
    ledger::StartCall,
    port::Error,
};
use serde_json::Value;
use sqlx::{Row, postgres::PgRow};

/// Verified authority metadata recorded on the call row and audit trail.
#[derive(Default)]
pub(crate) struct Mark {
    /// `(grant_id, bound_pause_epoch)` for pre-approved runs.
    pub grant: Option<(String, i64)>,
    /// `team/event_id` for consumed human-origin evidence.
    pub evidence: Option<String>,
}

impl Mark {
    /// Appends authority IDs (never payloads) to the audit verdict.
    pub(crate) fn verdict(&self, base: &str) -> String {
        match (&self.grant, &self.evidence) {
            (Some((id, _)), _) => format!("{base}:preapproved;grant={id}"),
            (_, Some(event)) => format!("{base}:human;evidence={event}"),
            _ => base.to_owned(),
        }
    }
}

/// Direct tool invocation on a root run (`{run}:tool`); excludes delegated children and model-planned calls.
fn direct(run: &RunRow, call: &str) -> bool {
    run.parent.is_none() && call == format!("{}:tool", run.id)
}

/// Validates start authority; `Ok(Err(reason))` denies without mutating state, `Err` is a store error.
pub(crate) async fn check_start(
    tx: &mut Tx<'_>,
    run: &RunRow,
    call: &PgRow,
    start: &StartCall,
    pointer: bool,
    authority: Option<&Authority>,
) -> R<Result<Mark, &'static str>> {
    let is_direct = direct(run, &start.call.0);
    let bound: Option<(String, i64)> =
        sqlx::query("select grant_id, epoch from bot_run_grants where profile = $1 and run = $2")
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_optional(&mut **tx)
            .await?
            .map(|row| (row.get("grant_id"), row.get("epoch")));
    match authority {
        // Grant-bound direct calls cannot bypass grant verification.
        None if bound.is_some() && is_direct => Ok(Err(reason::STALE)),
        None => Ok(Ok(Mark::default())),
        Some(_) if start.approval.is_some() => Err(Error::Conflict.into()),
        Some(Authority::Grant { fingerprint }) => {
            let Some((grant, epoch)) = bound.filter(|_| is_direct) else {
                return Ok(Err(reason::STALE));
            };
            Ok(granted(tx, call, start, fingerprint, &grant, epoch).await?)
        }
        Some(Authority::Human { actor }) => {
            if !is_direct || pointer || bound.is_some() {
                return Ok(Err(reason::HUMAN_MISMATCH));
            }
            Ok(human(tx, run, call, start, &actor.0).await?)
        }
    }
}

async fn granted(
    tx: &mut Tx<'_>,
    call: &PgRow,
    start: &StartCall,
    fingerprint: &[u8; 32],
    grant: &str,
    epoch: i64,
) -> R<Result<Mark, &'static str>> {
    let row = sqlx::query(
        "select profile, agent, trigger, state, fingerprint, tool, tool_revision, args_sha256
         from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Ok(Err(reason::STALE));
    };
    let same = row.get::<String, _>("state") == "active"
        && row.get::<Vec<u8>, _>("fingerprint") == fingerprint.as_slice()
        && row.get::<String, _>("tool") == call.get::<String, _>("tool")
        && row.get::<String, _>("tool_revision") == start.tool_revision.0
        && row.get::<Vec<u8>, _>("args_sha256") == call.get::<Vec<u8>, _>("args_sha256");
    if !same {
        return Ok(Err(reason::STALE));
    }
    match pause_state(tx, &row).await? {
        (true, _) => Ok(Err(reason::PAUSED)),
        (false, current) if current != epoch => Ok(Err(reason::STALE)),
        (false, _) => Ok(Ok(Mark {
            grant: Some((grant.to_owned(), epoch)),
            evidence: None,
        })),
    }
}

/// Reads `(paused, epoch)` with `FOR SHARE` to serialize against pause/resume commits; a missing row fails closed as paused.
async fn pause_state(tx: &mut Tx<'_>, grant: &PgRow) -> R<(bool, i64)> {
    let row = sqlx::query(
        "select epoch, paused from bot_trigger_pause where profile = $1 and agent = $2 and trigger = $3 for share",
    )
    .bind(grant.get::<String, _>("profile"))
    .bind(grant.get::<String, _>("agent"))
    .bind(grant.get::<String, _>("trigger"))
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map_or((true, 0), |r| (r.get("paused"), r.get("epoch"))))
}

async fn human(
    tx: &mut Tx<'_>,
    run: &RunRow,
    call: &PgRow,
    start: &StartCall,
    actor: &str,
) -> R<Result<Mark, &'static str>> {
    let evidence = sqlx::query(
        "select team, event_id, actor from bot_run_evidence where profile = $1 and run = $2",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(evidence) = evidence.filter(|e| e.get::<String, _>("actor") == actor) else {
        return Ok(Err(reason::HUMAN_MISMATCH));
    };
    let (team, event): (String, String) = (evidence.get("team"), evidence.get("event_id"));
    // Single-use per (profile, team, event_id) so one event cannot authorize a second candidate or restart.
    let used = sqlx::query(
        "insert into bot_evidence_uses (profile, team, event_id, run, call, tool, tool_revision, args_sha256, actor)
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9) on conflict do nothing",
    )
    .bind(&run.profile)
    .bind(&team)
    .bind(&event)
    .bind(&run.id)
    .bind(&start.call.0)
    .bind(call.get::<String, _>("tool"))
    .bind(&start.tool_revision.0)
    .bind(call.get::<Vec<u8>, _>("args_sha256"))
    .bind(actor)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if used != 1 {
        return Ok(Err(reason::HUMAN_MISMATCH));
    }
    Ok(Ok(Mark {
        grant: None,
        evidence: Some(format!("{team}/{event}")),
    }))
}

/// Closes an open call as `denied` with an audit row inside the caller's transaction.
pub(crate) async fn deny_in(tx: &mut Tx<'_>, run: &RunRow, call: &PgRow, why: &str) -> R<()> {
    sqlx::query(
        "update bot_calls set state = 'denied', updated_ms = bot_now_ms(), finished_ms = bot_now_ms()
         where profile = $1 and run = $2 and call = $3 and state = 'open'",
    )
    .bind(&run.profile)
    .bind(&run.id)
    .bind(call.get::<String, _>("call"))
    .execute(&mut **tx)
    .await?;
    let principal: String =
        sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut **tx)
            .await?;
    let id: String = call.get("call");
    let mut entry = crate::audit::Entry::new(&principal, "call.deny", &id, why);
    entry.args = Some(to32(call.get("args_sha256"))?);
    crate::audit::append(tx, &run.profile, &entry).await?;
    Ok(())
}

/// Reason a new pre-approved external write was refused, mapping to fixed audit reasons and wire errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refused {
    Stale,
    Paused,
    Limit,
    /// `(operation, target)` pair is not in the grant's allowlist.
    Target,
    /// Args do not match the exact `{operation, target, params}` object shape.
    Shape,
}

impl Refused {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Stale => reason::STALE,
            Self::Paused => reason::PAUSED,
            Self::Limit => reason::LIMIT,
            Self::Target | Self::Shape => reason::TARGET_DENIED,
        }
    }

    pub(crate) fn error(self) -> Error {
        match self {
            Self::Stale => Error::Stale,
            Self::Shape => Error::Invalid,
            Self::Paused | Self::Limit | Self::Target => Error::Denied,
        }
    }
}

/// Requires exact `{operation: str, target: str, params: object}` shape.
fn shape(args: &JsonPayload) -> Option<(String, String)> {
    let Ok((Value::Object(map), _)) = canonical(args) else {
        return None;
    };
    let (operation, target) = (
        map.get("operation")?.as_str()?,
        map.get("target")?.as_str()?,
    );
    (map.len() == 3 && map.get("params")?.is_object())
        .then(|| (operation.to_owned(), target.to_owned()))
}

fn listed(pairs: &Value, operation: &str, target: &str) -> bool {
    pairs.as_array().is_some_and(|list| {
        list.iter()
            .any(|p| p[0].as_str() == Some(operation) && p[1].as_str() == Some(target))
    })
}

/// Returns true if the grant is inactive or the trigger's pause epoch changed since call start (checked before args conflict).
pub(crate) async fn stale(tx: &mut Tx<'_>, grant: &str, epoch: i64) -> R<bool> {
    let row = sqlx::query(
        "select profile, agent, trigger, state from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row.filter(|r| r.get::<String, _>("state") == "active") else {
        return Ok(true);
    };
    Ok(pause_state(tx, &row).await?.1 != epoch)
}

/// Authorizes a new pre-approved external write and increments today's UTC usage; caller holds active/run/call/effect locks.
pub(crate) async fn permit_write(
    tx: &mut Tx<'_>,
    grant: &str,
    epoch: i64,
    args: &JsonPayload,
) -> R<Result<(), Refused>> {
    let row = sqlx::query(
        "select profile, agent, trigger, state, pairs, max_per_day from bot_grants where id = $1 for share",
    )
    .bind(grant)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row.filter(|r| r.get::<String, _>("state") == "active") else {
        return Ok(Err(Refused::Stale));
    };
    match pause_state(tx, &row).await? {
        (true, _) => return Ok(Err(Refused::Paused)),
        (false, current) if current != epoch => return Ok(Err(Refused::Stale)),
        (false, _) => {}
    }
    let Some((operation, target)) = shape(args) else {
        return Ok(Err(Refused::Shape));
    };
    if !listed(&row.get::<Value, _>("pairs"), &operation, &target) {
        return Ok(Err(Refused::Target));
    }
    let (profile, agent, trigger): (String, String, String) =
        (row.get("profile"), row.get("agent"), row.get("trigger"));
    // Read the UTC day once in this transaction to avoid a midnight split across insert/update.
    let day: i64 = sqlx::query_scalar("select bot_now_ms() / 86400000")
        .fetch_one(&mut **tx)
        .await?;
    sqlx::query(
        "insert into bot_grant_usage (profile, agent, trigger, day, used) values ($1, $2, $3, $4, 0)
         on conflict do nothing",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    let used: i32 = sqlx::query_scalar(
        "select used from bot_grant_usage where profile = $1 and agent = $2 and trigger = $3 and day = $4 for update",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .fetch_one(&mut **tx)
    .await?;
    if used >= row.get::<i32, _>("max_per_day") {
        return Ok(Err(Refused::Limit));
    }
    sqlx::query(
        "update bot_grant_usage set used = used + 1 where profile = $1 and agent = $2 and trigger = $3 and day = $4",
    )
    .bind(&profile)
    .bind(&agent)
    .bind(&trigger)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    Ok(Ok(()))
}
