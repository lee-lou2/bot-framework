//! Operator hold resolution (`AdminAct`) for tool calls, effect steps, outbox replies, and private DM runs.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store, Tx},
    finish::wake,
};
use bot_core::{
    admin::{HoldDecision, HoldRef, HoldVerdict},
    id::*,
    port::Error,
};
use sha2::{Digest, Sha256};
use sqlx::Row;

fn verdict_ok(verdict: &HoldVerdict) -> R<()> {
    match verdict {
        HoldVerdict::Confirmed { receipt } if id_ok(receipt) && receipt.len() <= 512 => Ok(()),
        HoldVerdict::NotExecuted { evidence }
            if !evidence.trim().is_empty() && evidence.len() <= 1024 =>
        {
            Ok(())
        }
        _ => invalid(),
    }
}

/// Audit verdict label; private DM holds omit raw evidence and record only fixed vocabulary.
fn label(verdict: &HoldVerdict, private: bool) -> String {
    match verdict {
        HoldVerdict::Confirmed { .. } => "confirmed".into(),
        HoldVerdict::NotExecuted { .. } if private => "not_executed".into(),
        HoldVerdict::NotExecuted { evidence } => format!(
            "not_executed:{}",
            evidence.chars().take(200).collect::<String>()
        ),
    }
}

/// Verdict kind and SHA-256 evidence digest (raw evidence is never stored).
fn fingerprint(verdict: &HoldVerdict) -> (&'static str, Vec<u8>) {
    let (kind, text) = match verdict {
        HoldVerdict::Confirmed { receipt } => ("confirmed", receipt),
        HoldVerdict::NotExecuted { evidence } => ("not_executed", evidence),
    };
    (kind, Sha256::digest(text.as_bytes()).to_vec())
}

impl Store {
    /// Verifies the hold ID and target, records the verdict, and appends audit atomically.
    /// Replaying the same operator, verdict, and evidence digest on a closed hold returns `Repeated`; mismatches return `Conflict`.
    /// Lock order: run -> call/effect or outbox row -> hold -> audit.
    pub(crate) async fn resolve_hold_run(
        &self,
        actor: &PrincipalId,
        hold: &HoldRef,
        verdict: &HoldVerdict,
    ) -> R<HoldDecision> {
        ids_ok(&[
            &actor.0,
            &hold.hold_id.0,
            &hold.profile.0,
            &hold.run.0,
            &hold.call.0,
        ])?;
        if let Some(effect) = &hold.effect {
            ids_ok(&[&effect.0])?;
        }
        verdict_ok(verdict)?;
        let (decided, digest) = fingerprint(verdict);
        let mut tx = self.begin_authorized().await?;
        // `profile` and `run` on a hold are immutable; read them first to lock the run before locking targets.
        let owner: (String, String) =
            sqlx::query_as("select profile, run from bot_holds where id = $1")
                .bind(&hold.hold_id.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Missing)?;
        if owner.0 != hold.profile.0 || owner.1 != hold.run.0 {
            return Err(Error::Conflict.into());
        }
        let (private, waiting_on_hold): (bool, bool) = sqlx::query_as(
            "select private, state = 'waiting' and wait_kind is not distinct from 'hold'
             from bot_runs where profile = $1 and id = $2 for update",
        )
        .bind(&hold.profile.0)
        .bind(&hold.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let row = sqlx::query(
            "select call, effect, kind, closed_ms, operator, verdict, evidence_sha256 from bot_holds where id = $1",
        )
        .bind(&hold.hold_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("call") != hold.call.0
            || row.get::<Option<String>, _>("effect") != hold.effect.as_ref().map(|e| e.0.clone())
        {
            return Err(Error::Conflict.into());
        }
        if row.get::<Option<i64>, _>("closed_ms").is_some() {
            let same = row.get::<Option<String>, _>("operator").as_deref() == Some(&actor.0)
                && row.get::<Option<String>, _>("verdict").as_deref() == Some(decided)
                && row.get::<Option<Vec<u8>>, _>("evidence_sha256").as_deref()
                    == Some(digest.as_slice());
            return if same {
                Ok(HoldDecision::Repeated)
            } else {
                Err(Error::Conflict.into())
            };
        }
        let locked = lock_target(&mut tx, hold, &row.get::<String, _>("kind"), private).await?;
        let claimed = sqlx::query(
            "update bot_holds set closed_ms = bot_now_ms(), operator = $2, verdict = $3, evidence_sha256 = $4
             where id = $1 and closed_ms is null",
        )
        .bind(&hold.hold_id.0)
        .bind(&actor.0)
        .bind(decided)
        .bind(&digest)
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() != 1 {
            return Err(Error::Conflict.into());
        }
        let target = match locked {
            LockedTarget::Reply {
                seq,
                state,
                transport,
            } => {
                self.resolve_reply(&mut tx, hold, seq, &state, &transport, verdict)
                    .await?
            }
            LockedTarget::Private => {
                self.resolve_private(&mut tx, hold, waiting_on_hold, verdict)
                    .await?
            }
            LockedTarget::Tool {
                call_state,
                call_resolution,
                effect,
            } => {
                self.resolve_call(
                    &mut tx,
                    hold,
                    &call_state,
                    call_resolution.as_deref(),
                    effect.as_ref().map(|(s, r)| (s.as_str(), r.as_deref())),
                    waiting_on_hold,
                    verdict,
                )
                .await?
            }
        };
        audit::append(
            &mut tx,
            &hold.profile.0,
            &Entry::new(&actor.0, "hold.resolve", &target, &label(verdict, private)),
        )
        .await?;
        tx.commit().await?;
        Ok(HoldDecision::Applied)
    }

    #[allow(clippy::too_many_arguments)]
    async fn resolve_call(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        call_state: &str,
        call_resolution: Option<&str>,
        effect_row: Option<(&str, Option<&str>)>,
        waiting_on_hold: bool,
        verdict: &HoldVerdict,
    ) -> R<String> {
        let (profile, run, call) = (&hold.profile.0, &hold.run.0, &hold.call.0);
        if call_state != "held" {
            return Err(Error::Conflict.into());
        }
        let target = match &hold.effect {
            None => {
                let (pending, confirmed) = step_evidence(tx, hold).await?;
                if pending || (confirmed && matches!(verdict, HoldVerdict::NotExecuted { .. })) {
                    return invalid();
                }
                if call_resolution.is_some() {
                    return Err(Error::Conflict.into());
                }
                let (resolution, receipt) = match verdict {
                    HoldVerdict::Confirmed { receipt } => ("executed", Some(receipt.as_str())),
                    HoldVerdict::NotExecuted { .. } => ("not_executed", None),
                };
                set_call_resolution(tx, hold, resolution, receipt).await?;
                call.clone()
            }
            Some(effect) => {
                let (step_state, step_resolution) = effect_row.ok_or(Error::Missing)?;
                if step_state != "held" || step_resolution.is_some() {
                    return Err(Error::Conflict.into());
                }
                match verdict {
                    HoldVerdict::Confirmed { receipt } => {
                        sqlx::query("update bot_effects set state = 'confirmed', receipt = $3, reason = null, updated_ms = bot_now_ms() where profile = $1 and id = $2")
                            .bind(profile)
                            .bind(&effect.0)
                            .bind(receipt)
                            .execute(&mut **tx)
                            .await?;
                    }
                    HoldVerdict::NotExecuted { .. } => {
                        sqlx::query("update bot_effects set resolution = 'not_executed', updated_ms = bot_now_ms() where profile = $1 and id = $2")
                            .bind(profile)
                            .bind(&effect.0)
                            .execute(&mut **tx)
                            .await?;
                    }
                }
                let (pending, confirmed) = step_evidence(tx, hold).await?;
                if pending || confirmed {
                    // Wait for remaining unresolved steps or a call-level receipt; the DB trigger opened the next hold.
                    return Ok(format!("{call}:{}", effect.0));
                }
                set_call_resolution(tx, hold, "not_executed", None).await?;
                format!("{call}:{}", effect.0)
            }
        };
        if waiting_on_hold {
            wake(tx, profile, run, "hold", None).await?;
        }
        Ok(target)
    }

    async fn resolve_reply(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        seq: i32,
        state: &str,
        transport: &str,
        verdict: &HoldVerdict,
    ) -> R<String> {
        let run = &hold.run.0;
        if hold.effect.is_some() {
            return invalid();
        }
        if state != "held" {
            return Err(Error::Conflict.into());
        }
        // Replies without a transport can never be claimed if reset to `pending`; allow `Confirmed` only.
        if transport.is_empty() && matches!(verdict, HoldVerdict::NotExecuted { .. }) {
            return Err(Error::Conflict.into());
        }
        let sql = match verdict {
            HoldVerdict::Confirmed { .. } => {
                "update bot_outbox set state = 'confirmed', receipt = $4, reason = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"
            }
            // Proven not executed: reset to `pending` for fresh delivery while keeping delivery generation monotonic.
            HoldVerdict::NotExecuted { .. } => {
                "update bot_outbox set state = 'pending', attempts = 0, not_before_ms = 0, lease_until_ms = null, reason = null,
                        updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3 and $4::text is null"
            }
        };
        let receipt = match verdict {
            HoldVerdict::Confirmed { receipt } => Some(receipt.as_str()),
            HoldVerdict::NotExecuted { .. } => None,
        };
        sqlx::query(sql)
            .bind(&hold.profile.0)
            .bind(run)
            .bind(seq)
            .bind(receipt)
            .execute(&mut **tx)
            .await?;
        Ok(hold.call.0.clone())
    }

    async fn resolve_private(
        &self,
        tx: &mut Tx<'_>,
        hold: &HoldRef,
        waiting_on_hold: bool,
        verdict: &HoldVerdict,
    ) -> R<String> {
        if hold.effect.is_some() {
            return invalid();
        }
        if !waiting_on_hold {
            return Err(Error::Conflict.into());
        }
        // Private runs store no payload and cannot be re-executed; settle as terminal state.
        let (state, error) = match verdict {
            HoldVerdict::Confirmed { .. } => ("succeeded", None),
            HoldVerdict::NotExecuted { .. } => ("dead", Some("not_executed")),
        };
        sqlx::query(
            "update bot_runs set state = $3, error = $4, wait_kind = null, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&hold.profile.0)
        .bind(&hold.run.0)
        .bind(state)
        .bind(error)
        .execute(&mut **tx)
        .await?;
        Ok(hold.call.0.clone())
    }
}

/// Returns `(has_unresolved_steps, has_confirmed_steps)` for a call.
async fn step_evidence(tx: &mut Tx<'_>, hold: &HoldRef) -> R<(bool, bool)> {
    Ok(sqlx::query_as(
        "select coalesce(bool_or(state = 'started' or (state = 'held' and resolution is null)), false),
                coalesce(bool_or(state = 'confirmed'), false)
         from bot_effects where profile = $1 and run = $2 and call = $3",
    )
    .bind(&hold.profile.0).bind(&hold.run.0).bind(&hold.call.0)
    .fetch_one(&mut **tx).await?)
}

async fn set_call_resolution(
    tx: &mut Tx<'_>,
    hold: &HoldRef,
    resolution: &str,
    receipt: Option<&str>,
) -> R<()> {
    sqlx::query(
        "update bot_calls set resolution = $4, receipt = $5, updated_ms = bot_now_ms() where profile = $1 and run = $2 and call = $3",
    )
    .bind(&hold.profile.0)
    .bind(&hold.run.0)
    .bind(&hold.call.0)
    .bind(resolution)
    .bind(receipt)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

enum LockedTarget {
    Tool {
        call_state: String,
        call_resolution: Option<String>,
        effect: Option<(String, Option<String>)>,
    },
    Reply {
        seq: i32,
        state: String,
        transport: String,
    },
    Private,
}

/// Locks target rows (`call -> effect` or `outbox`) with `FOR UPDATE`, rejecting kind/privacy mismatches with `Conflict`.
async fn lock_target(
    tx: &mut Tx<'_>,
    hold: &HoldRef,
    kind: &str,
    private_run: bool,
) -> R<LockedTarget> {
    let (profile, run, call) = (&hold.profile.0, &hold.run.0, &hold.call.0);
    match kind {
        "tool" if !private_run => {
            let (call_state, call_resolution): (String, Option<String>) = sqlx::query_as(
                "select state, resolution from bot_calls where profile = $1 and run = $2 and call = $3 for update",
            )
            .bind(profile)
            .bind(run)
            .bind(call)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(Error::Missing)?;
            let effect = match &hold.effect {
                Some(effect) => Some(
                    sqlx::query_as::<_, (String, Option<String>)>(
                        "select state, resolution from bot_effects where profile = $1 and id = $2 and run = $3 and call = $4 for update",
                    )
                    .bind(profile)
                    .bind(&effect.0)
                    .bind(run)
                    .bind(call)
                    .fetch_optional(&mut **tx)
                    .await?
                    .ok_or(Error::Missing)?,
                ),
                None => None,
            };
            Ok(LockedTarget::Tool {
                call_state,
                call_resolution,
                effect,
            })
        }
        "reply" if !private_run && hold.effect.is_none() => {
            let rows: Vec<(i32, String, String)> = sqlx::query_as(
                "select seq, state, transport from bot_outbox where profile = $1 and run = $2 and call = $3 for update",
            )
            .bind(profile)
            .bind(run)
            .bind(call)
            .fetch_all(&mut **tx)
            .await?;
            match rows.as_slice() {
                [] => Err(Error::Missing.into()),
                [(seq, state, transport)] => Ok(LockedTarget::Reply {
                    seq: *seq,
                    state: state.clone(),
                    transport: transport.clone(),
                }),
                _ => Err(Error::Conflict.into()),
            }
        }
        "private" if private_run && hold.effect.is_none() && *call == format!("private:{run}") => {
            Ok(LockedTarget::Private)
        }
        _ => Err(Error::Conflict.into()),
    }
}
