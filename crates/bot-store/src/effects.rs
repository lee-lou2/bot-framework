//! Multi-step effect journal (`EffectJournal`): commits intent before granting `send=true` and fences restarts behind operator `not_executed` verdicts.

use crate::{
    audit::{self, Entry},
    calls::{fence, fence_settle, lock_call, record_of},
    codec::*,
    db::{R, Store},
};
use bot_core::{
    command::{EffectRequest, EffectStatus, JsonPayload},
    id::*,
    ledger::CallPhase,
    port::{EffectPermit, Error},
    state::{EffectResolution, EffectState},
};
use sqlx::{Row, postgres::PgRow};

pub(crate) fn state_str(state: EffectState) -> &'static str {
    match state {
        EffectState::Started => "started",
        EffectState::Confirmed => "confirmed",
        EffectState::Rejected => "rejected",
        EffectState::Held => "held",
    }
}

pub(crate) fn state_of(s: &str) -> R<EffectState> {
    Ok(match s {
        "started" => EffectState::Started,
        "confirmed" => EffectState::Confirmed,
        "rejected" => EffectState::Rejected,
        "held" => EffectState::Held,
        _ => return invalid(),
    })
}

pub(crate) fn args_digest(args: &JsonPayload) -> R<[u8; 32]> {
    Ok(sha256_text(&canonical(args)?.1))
}

pub(crate) fn status_of(row: &PgRow) -> R<EffectStatus> {
    let state = state_of(&row.get::<String, _>("state"))?;
    let resolution = (state == EffectState::Held
        && row.get::<Option<String>, _>("resolution").as_deref() == Some("not_executed"))
    .then_some(EffectResolution::NotExecuted);
    Ok(EffectStatus {
        effect: EffectId(row.get("id")),
        state,
        resolution,
        receipt: row.get("receipt"),
    })
}

async fn lock_effect(
    tx: &mut crate::db::Tx<'_>,
    at: &Attempt,
    call: &CallId,
    step: &str,
) -> R<Option<PgRow>> {
    Ok(sqlx::query(
        &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and step = $4 for update", crate::cols::EFFECTS),
    )
    .bind(&at.profile.0)
    .bind(&at.run.0)
    .bind(&call.0)
    .bind(step)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn write_refused(
    tx: &mut crate::db::Tx<'_>,
    granted: &Option<(String, i64)>,
    args: &JsonPayload,
) -> R<Option<crate::authority::Refused>> {
    let Some((grant, epoch)) = granted else {
        return Ok(None);
    };
    Ok(crate::authority::permit_write(tx, grant, *epoch, args)
        .await?
        .err())
}

fn granted_verdict(step: &str, granted: &Option<(String, i64)>) -> String {
    granted
        .as_ref()
        .map_or_else(|| step.to_owned(), |(id, _)| format!("{step};grant={id}"))
}

/// Commits an audit-only refusal row (no usage or effect written) and returns the mapped wire error.
async fn refuse(
    mut tx: crate::db::Tx<'_>,
    profile: &str,
    principal: &str,
    step: &str,
    digest: [u8; 32],
    why: crate::authority::Refused,
) -> R<EffectPermit> {
    let mut entry = Entry::new(principal, "effect.refuse", step, why.reason());
    entry.args = Some(digest);
    audit::append(&mut tx, profile, &entry).await?;
    tx.commit().await?;
    Err(why.error().into())
}

impl Store {
    pub(crate) async fn effect_request(&self, r: EffectRequest) -> R<EffectPermit> {
        ids_ok(&[&r.call.0, &r.step])?;
        let digest = args_digest(&r.args)?;
        let at = &r.attempt;
        let mut tx = self.pool.begin().await?;
        // Lock order for grant-bound calls: active set -> run -> call -> effect -> grant -> pause -> usage -> audit.
        let granted: Option<(String, i64)> = sqlx::query(
            "select grant_id, grant_epoch from bot_calls where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&r.call.0)
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|row| row.get::<Option<String>, _>("grant_id").zip(row.get::<Option<i64>, _>("grant_epoch")));
        if granted.is_some() {
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Stale)?;
        }
        let run = fence(&mut tx, at).await?;
        // Effects may only be requested within a call started by the current generation.
        let call = lock_call(&mut tx, at, &r.call).await?;
        match record_of(&call)?.phase {
            CallPhase::Started { generation, .. } if generation == at.generation => {}
            _ => return Err(Error::Conflict.into()),
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let permit = match lock_effect(&mut tx, at, &r.call, &r.step).await? {
            None => {
                if let Some(refused) = write_refused(&mut tx, &granted, &r.args).await? {
                    return refuse(tx, &run.profile, &principal, &r.step, digest, refused).await;
                }
                let id: String = sqlx::query_scalar(
                    "insert into bot_effects (profile, id, run, call, step, args_sha256, generation, grant_id)
                     values ($1, 'e-' || gen_random_uuid()::text, $2, $3, $4, $5, $6, $7) returning id",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(&r.call.0)
                .bind(&r.step)
                .bind(digest.as_slice())
                .bind(at.generation.0 as i64)
                .bind(granted.as_ref().map(|(id, _)| id.as_str()))
                .fetch_one(&mut *tx)
                .await?;
                let verdict = granted_verdict(&r.step, &granted);
                let mut entry = Entry::new(&principal, "effect.start", &id, &verdict);
                entry.args = Some(digest);
                audit::append(&mut tx, &run.profile, &entry).await?;
                EffectPermit {
                    status: EffectStatus {
                        effect: EffectId(id),
                        state: EffectState::Started,
                        resolution: None,
                        receipt: None,
                    },
                    send: true,
                }
            }
            Some(row) => {
                if row.get::<Vec<u8>, _>("args_sha256") != digest {
                    // Report stale grant before argument conflict so revoked/superseded grants surface cleanly.
                    if let Some((grant, epoch)) = &granted
                        && crate::authority::stale(&mut tx, grant, *epoch).await?
                    {
                        let stale = crate::authority::Refused::Stale;
                        return refuse(tx, &run.profile, &principal, &r.step, digest, stale).await;
                    }
                    return Err(Error::Conflict.into());
                }
                let id: String = row.get("id");
                let (state, held_before) = (
                    state_of(&row.get::<String, _>("state"))?,
                    row.get::<String, _>("state") == "held",
                );
                match state {
                    EffectState::Started => {
                        // Prior attempt outcome is unknown: hold on generation advance and never re-send.
                        if row.get::<i64, _>("generation") < at.generation.0 as i64 {
                            sqlx::query("update bot_effects set state = 'held', reason = 'owner_lost', updated_ms = bot_now_ms() where profile = $1 and id = $2")
                                .bind(&run.profile)
                                .bind(&id)
                                .execute(&mut *tx)
                                .await?;
                            audit::append(
                                &mut tx,
                                &run.profile,
                                &Entry::new("system", "effect.held", &id, "owner_lost"),
                            )
                            .await?;
                        }
                        EffectPermit {
                            status: EffectStatus {
                                effect: EffectId(id),
                                state: EffectState::Held,
                                resolution: None,
                                receipt: None,
                            },
                            send: false,
                        }
                    }
                    EffectState::Held
                        if held_before
                            && row.get::<Option<String>, _>("resolution").as_deref()
                                == Some("not_executed") =>
                    {
                        // Operator proved non-execution: restart under the current generation (counts as a fresh grant permit).
                        if let Some(refused) = write_refused(&mut tx, &granted, &r.args).await? {
                            return refuse(tx, &run.profile, &principal, &r.step, digest, refused)
                                .await;
                        }
                        sqlx::query(
                            "update bot_effects set state = 'started', generation = $3, reason = null, resolution = null, updated_ms = bot_now_ms(),
                                    grant_id = coalesce($4, grant_id)
                             where profile = $1 and id = $2",
                        )
                        .bind(&run.profile)
                        .bind(&id)
                        .bind(at.generation.0 as i64)
                        .bind(granted.as_ref().map(|(id, _)| id.as_str()))
                        .execute(&mut *tx)
                        .await?;
                        let verdict = granted_verdict(&r.step, &granted);
                        audit::append(
                            &mut tx,
                            &run.profile,
                            &Entry::new(&principal, "effect.restart", &id, &verdict),
                        )
                        .await?;
                        EffectPermit {
                            status: EffectStatus {
                                effect: EffectId(id),
                                state: EffectState::Started,
                                resolution: None,
                                receipt: None,
                            },
                            send: true,
                        }
                    }
                    _ => EffectPermit {
                        status: status_of(&row)?,
                        send: false,
                    },
                }
            }
        };
        tx.commit().await?;
        Ok(permit)
    }

    pub(crate) async fn effect_status(
        &self,
        at: &Attempt,
        call: &CallId,
        effect: &EffectId,
    ) -> R<EffectStatus> {
        let mut tx = self.pool.begin().await?;
        fence_settle(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {} from bot_effects where profile = $1 and id = $2 and run = $3 and call = $4",
            crate::cols::EFFECTS
        ))
        .bind(&at.profile.0)
        .bind(&effect.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        tx.rollback().await?;
        status_of(&row)
    }

    /// Lists `started` or unresolved `held` steps that block completing the call.
    pub(crate) async fn effect_unresolved(
        &self,
        at: &Attempt,
        call: &CallId,
    ) -> R<Vec<EffectStatus>> {
        let mut tx = self.pool.begin().await?;
        fence_settle(&mut tx, at).await?;
        let rows = sqlx::query(
            &format!("select {} from bot_effects where profile = $1 and run = $2 and call = $3 and state in ('started', 'held')
               and not (state = 'held' and resolution is not distinct from 'not_executed') order by created_ms, id", crate::cols::EFFECTS),
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&call.0)
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;
        rows.iter().map(status_of).collect()
    }

    /// Records step outcome (`Confirmed` receipt, `Rejected`, or `Held` reason code carried in `status.receipt`).
    pub(crate) async fn effect_record(
        &self,
        at: &Attempt,
        call: &CallId,
        status: EffectStatus,
    ) -> R<()> {
        let text = status.receipt.as_deref();
        match (status.state, text) {
            (EffectState::Confirmed, Some(receipt)) if id_ok(receipt) && receipt.len() <= 512 => {}
            (EffectState::Held, Some(code)) if id_ok(code) && code.len() <= 64 => {}
            (EffectState::Rejected, None) => {}
            _ => return invalid(),
        }
        let mut tx = self.pool.begin().await?;
        let run = fence_settle(&mut tx, at).await?;
        let row = sqlx::query(&format!(
            "select {} from bot_effects where profile = $1 and id = $2 for update",
            crate::cols::EFFECTS
        ))
        .bind(&at.profile.0)
        .bind(&status.effect.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("run") != at.run.0 || row.get::<String, _>("call") != call.0 {
            return Err(Error::Denied.into());
        }
        let current = state_of(&row.get::<String, _>("state"))?;
        if current == status.state
            && row.get::<Option<String>, _>("receipt").as_deref()
                == text.filter(|_| status.state == EffectState::Confirmed)
        {
            tx.rollback().await?;
            return Ok(());
        }
        match (current, status.state) {
            (
                EffectState::Started | EffectState::Held,
                EffectState::Confirmed | EffectState::Rejected,
            ) => {}
            (EffectState::Started, EffectState::Held) => {}
            _ => return Err(Error::Conflict.into()),
        }
        let principal: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&run.profile)
                .bind(&run.id)
                .fetch_one(&mut *tx)
                .await?;
        let (receipt, reason) = match status.state {
            EffectState::Confirmed => (text, None),
            EffectState::Held => (None, text),
            _ => (None, None),
        };
        sqlx::query(
            "update bot_effects set state = $3, receipt = $4, reason = $5, resolution = null, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&status.effect.0)
        .bind(state_str(status.state))
        .bind(receipt)
        .bind(reason)
        .execute(&mut *tx)
        .await?;
        let verdict = reason.map_or_else(
            || state_str(status.state).to_owned(),
            |code| format!("held:{code}"),
        );
        let mut entry = Entry::new(&principal, "effect.record", &status.effect.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(())
    }
}
