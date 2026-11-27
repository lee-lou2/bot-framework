//! Pass chain (`bot/pass`) storage and monotonic cursor advancement, refusing passes once any side effect exists.

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, phase_of},
    codec::*,
    db::{R, Store, Tx},
    finish::{End, end_run_into},
};
use bot_core::{
    command::{Accept, Target},
    id::{AgentId, RunId},
    ledger::{CallPhase, FinishCall, Outcome},
    pass::{Candidate, PASS_AFTER_EFFECT, PassChain, PassNext, direct_call_id},
    port::Error,
};
use serde_json::{Value, json};
use sqlx::Row;
use std::collections::BTreeSet;

pub const MAX_CANDIDATES: usize = 32;

/// Validates a pass chain (first candidate matches `Accept.target`; candidates are direct tools; optional default is an agent).
fn chain_doc(a: &Accept, chain: &PassChain) -> R<Value> {
    let Some(first) = chain.candidates.first() else {
        return invalid();
    };
    if chain.candidates.len() > MAX_CANDIDATES || first.target != a.target {
        return invalid();
    }
    let mut candidates = Vec::new();
    for c in &chain.candidates {
        if !id_ok(&c.trigger) || !matches!(c.target, Target::Tool { .. }) {
            return invalid();
        }
        ids_ok(&[&agent_of(&c.target).0])?;
        candidates.push(json!({"t": c.trigger, "target": target_json(&c.target)?}));
    }
    let default = match &chain.default {
        Some(target @ Target::Agent { .. }) => {
            ids_ok(&[&agent_of(target).0])?;
            Some(target_json(target)?)
        }
        Some(_) => return invalid(),
        None => None,
    };
    Ok(json!({"c": candidates, "d": default}))
}

pub(crate) async fn store_chain(
    tx: &mut Tx<'_>,
    a: &Accept,
    run: &str,
    chain: &PassChain,
) -> R<()> {
    let doc = chain_doc(a, chain)?;
    sqlx::query("update bot_runs set pass_chain = $3, pass_pos = 0 where profile = $1 and id = $2")
        .bind(&a.event.profile.0)
        .bind(run)
        .bind(&doc)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn read_chain(doc: &Value) -> R<PassChain> {
    let mut candidates = Vec::new();
    for c in doc
        .get("c")
        .and_then(Value::as_array)
        .ok_or(Error::Invalid)?
    {
        let trigger = c.get("t").and_then(Value::as_str).ok_or(Error::Invalid)?;
        candidates.push(Candidate {
            trigger: trigger.to_owned(),
            target: target_from(c.get("target").ok_or(Error::Invalid)?)?,
        });
    }
    let default = match doc.get("d") {
        None | Some(Value::Null) => None,
        Some(target) => Some(target_from(target)?),
    };
    Ok(PassChain {
        candidates,
        default,
    })
}

/// Checks whether the run or call already left side-effect traces (effects, outbox, approvals, or delivery drafts).
async fn effect_traces(tx: &mut Tx<'_>, profile: &str, run: &str, call: &str) -> R<bool> {
    let found: bool = sqlx::query_scalar(
        "select exists (select 1 from bot_effects where profile = $1 and run = $2 and call = $3)
             or exists (select 1 from bot_outbox where profile = $1 and run = $2)
             or exists (select 1 from bot_approvals where profile = $1 and run = $2)
             or exists (select 1 from bot_deliveries where profile = $1 and run = $2)
             or exists (select 1 from bot_calls where profile = $1 and run = $2 and call = $3 and approval is not null)",
    )
    .bind(profile)
    .bind(run)
    .bind(call)
    .fetch_one(&mut **tx)
    .await?;
    Ok(found)
}

impl Store {
    /// Atomically completes the passed call, advances `pass_pos`, and either requeues the next target or ends the run silently.
    pub(crate) async fn call_finish_pass(
        &self,
        f: FinishCall,
        pos: u32,
        runnable: BTreeSet<AgentId>,
    ) -> R<PassNext> {
        let Outcome::Done(payload) = &f.outcome else {
            return invalid();
        };
        let result = canonical(payload)?.1;
        let at = &f.attempt;
        let mut tx = self.pool.begin().await?;
        let run = fence(&mut tx, at).await?;
        if run.parent.is_some() {
            return invalid();
        }
        let row = lock_call(&mut tx, at, &f.call).await?;
        match phase_of(&row)? {
            CallPhase::Started { generation, .. } if generation.0 <= at.generation.0 => {}
            _ => return Err(Error::Conflict.into()),
        }
        let cursor = sqlx::query(
            "select pass_chain, pass_pos, principal, scopes from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (Some(doc), Some(current)) = (
            cursor.get::<Option<Value>, _>("pass_chain"),
            cursor.get::<Option<i32>, _>("pass_pos"),
        ) else {
            return Err(Error::Stale.into());
        };
        if u32::try_from(current) != Ok(pos)
            || f.call != direct_call_id(&RunId(run.id.clone()), pos)
        {
            return Err(Error::Stale.into());
        }
        let PassChain {
            candidates,
            default,
        } = read_chain(&doc)?;
        let index = usize::try_from(pos).map_err(|_| Error::Invalid)?;
        let Some(Candidate { trigger, .. }) = candidates.get(index) else {
            return Err(Error::Stale.into());
        };
        if effect_traces(&mut tx, &run.profile, &run.id, &f.call.0).await? {
            tx.rollback().await?;
            return Ok(PassNext::Refused(PASS_AFTER_EFFECT));
        }
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, reason = null, updated_ms = bot_now_ms(), finished_ms = bot_now_ms()
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .bind(&f.call.0)
        .bind(&result)
        .execute(&mut *tx)
        .await?;
        let next_pos = pos + 1;
        let next_index = usize::try_from(next_pos).map_err(|_| Error::Invalid)?;
        // Default agent fallback requires `RUN_SCOPE` both at intake and in the current revision (`runnable`); otherwise end silently.
        let granted = cursor
            .get::<Vec<String>, _>("scopes")
            .iter()
            .any(|scope| scope == bot_core::scope::RUN_SCOPE);
        let may_run = |target: &Target| granted && runnable.contains(agent_of(target));
        let (next, label) = match (candidates.get(next_index), &default) {
            (Some(next), _) => (
                PassNext::Candidate(next_pos),
                (
                    Some(next.target.clone()),
                    format!("candidate:{}@{next_pos}", next.trigger),
                ),
            ),
            (None, Some(target)) if may_run(target) => (
                PassNext::Default,
                (Some(target.clone()), format!("default@{next_pos}")),
            ),
            (None, Some(_)) => (PassNext::End, (None, format!("default_denied@{next_pos}"))),
            (None, None) => (PassNext::End, (None, format!("end@{next_pos}"))),
        };
        let (target, label) = label;
        let mut audits = Vec::new();
        match &target {
            Some(target) => {
                // Requeue for the next candidate or default agent; the next claim bumps generation and claim epoch.
                sqlx::query(
                    "update bot_runs set target = $3, agent = $4, pass_pos = $5, state = 'ready', lease_until_ms = null, lease_owner = null,
                            updated_ms = bot_now_ms()
                     where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(target_json(target)?)
                .bind(&agent_of(target).0)
                .bind(i32::try_from(next_pos).map_err(|_| Error::Invalid)?)
                .execute(&mut *tx)
                .await?;
            }
            None => {
                sqlx::query(
                    "update bot_runs set pass_pos = $3, updated_ms = bot_now_ms() where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(i32::try_from(next_pos).map_err(|_| Error::Invalid)?)
                .execute(&mut *tx)
                .await?;
                // End silently without enqueueing outbox replies.
                end_run_into(
                    &mut tx,
                    &run,
                    End::Succeeded(&result),
                    "system",
                    &mut audits,
                )
                .await?;
            }
        }
        let principal: String = cursor.get("principal");
        let verdict = format!("{trigger}@{pos}");
        let mut entry = OwnedEntry::new(&run.profile, &principal, "call.pass", &f.call.0, &verdict);
        entry.args = Some(to32(row.get("args_sha256"))?);
        audits.push(entry);
        audits.push(OwnedEntry::new(
            &run.profile,
            "system",
            "trigger.next",
            &run.id,
            &label,
        ));
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(next)
    }
}
