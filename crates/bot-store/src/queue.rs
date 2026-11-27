//! Run queue operations (`claim`, `renew`, `checkpoint`, `fail`, `history`, and stale-worker handover).

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::*,
    handover,
};
use bot_core::{
    api::ApiOrigin,
    command::{Beat, Failure, JsonPayload, Retry, Step, Turn, Work},
    id::*,
    port::Error,
    state::*,
};
use serde_json::Value;
use sqlx::Row;
use std::{collections::BTreeSet, time::Duration};

const MAX_BATCH: usize = 512;

struct Cand {
    run: RunRow,
    state: String,
    generation: i64,
    claim_set: Option<String>,
}

/// Locks the oldest claimable run via `SKIP LOCKED` (due `ready`/`retry` or lease-expired `leased`).
async fn pick(
    tx: &mut Tx<'_>,
    skipped_profiles: &[String],
    skipped_runs: &[String],
) -> R<Option<Cand>> {
    let row = sqlx::query(include_str!("queue/pick.sql"))
        .bind(skipped_profiles)
        .bind(skipped_runs)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.map(|r| Cand {
        run: RunRow {
            profile: r.get("profile"),
            id: r.get("id"),
            parent: r.get("parent"),
            parent_step: r.get("parent_step"),
            attempts: r.get("attempts"),
            cancel_requested: r.get("cancel_requested"),
            private: false,
        },
        state: r.get("state"),
        generation: r.get("generation"),
        claim_set: r.get("claim_set"),
    }))
}

pub(crate) async fn load(tx: &mut Tx<'_>, profile: &str, id: &str) -> R<RunContext> {
    let r = sqlx::query(&format!(
        "select {}, x.private_memory_read as tree_private, x.agent as root_agent,
                x.trigger as root_trigger, x.run_origin as root_origin,
                q.api_endpoint as root_api_endpoint, q.api_model as root_api_model
         from bot_runs r join bot_runs x on x.profile = r.profile and x.id = r.root
              left join bot_requests q on q.profile = x.profile and q.run = x.id and q.api_endpoint is not null
         where r.profile = $1 and r.id = $2",
        crate::cols::prefixed(crate::cols::RUNS, "r")
    ))
    .bind(profile)
    .bind(id)
    .fetch_one(&mut **tx)
    .await?;
    let steps =
        sqlx::query("select key, body from bot_steps where profile = $1 and run = $2 order by seq")
            .bind(profile)
            .bind(id)
            .fetch_all(&mut **tx)
            .await?
            .into_iter()
            .map(|s| Step {
                key: s.get("key"),
                body: JsonPayload(s.get("body")),
            })
            .collect();
    let target: Value = r.get("target");
    let scopes: Vec<String> = r.get("scopes");
    let thread: Option<String> = r.get("thread");
    Ok(RunContext {
        attempt: Attempt {
            profile: ProfileId(profile.to_owned()),
            run: RunId(id.to_owned()),
            claim: Some(ClaimEpoch(r.try_get::<i64, _>("claim_epoch")? as u64)),
            generation: Generation(r.get::<i64, _>("generation") as u64),
        },
        root: RunId(r.get("root")),
        origin: RunOrigin::from_recorded(r.get("root_origin")),
        principal: PrincipalId(r.get("principal")),
        agent: AgentId(r.get("agent")),
        conversation: Conversation {
            kind: kind_from(&r.get::<String, _>("conv_kind"))?,
            id: r.get("conv_id"),
            thread,
        },
        binding: BindingId(r.get("binding")),
        // Authenticated OpenAI-compatible API origin pinned on the root request row (inherited by children).
        api: r
            .get::<Option<String>, _>("root_api_endpoint")
            .zip(r.get::<Option<String>, _>("root_api_model"))
            .map(|(endpoint, model)| ApiOrigin { endpoint, model }),
        scopes: scopes.into_iter().collect::<BTreeSet<_>>(),
        private_memory_read: r.get("tree_private"),
        approver: r.get::<Option<String>, _>("approver").map(PrincipalId),
        trigger: r
            .get::<Option<String>, _>("root_trigger")
            .map(|id| TriggerRef {
                agent: AgentId(r.get("root_agent")),
                id,
            }),
        work: Work {
            revision: Revision(r.get("revision")),
            target: target_from(&target)?,
            input: JsonPayload(r.get("input")),
            attachments: r.get("attachments"),
            steps,
            pass_pos: r
                .get::<Option<i32>, _>("pass_pos")
                .map(|pos| u32::try_from(pos).unwrap_or(0)),
        },
    })
}

impl Store {
    /// Claims the oldest claimable run, bumping generation/claim epoch and recording the pinned active set.
    /// Forgives attempt increment on lease reclaim when the active set changed (`handover`), and dead-letters runs past `max_attempts`.
    pub(crate) async fn claim_run(&self) -> R<Option<RunContext>> {
        self.expire_approvals().await?;
        self.sweep_private().await?;
        let (mut skipped_profiles, mut skipped_runs) = (Vec::new(), Vec::new());
        for _ in 0..8 {
            let mut tx = self.begin_authorized().await?;
            let Some(c) = pick(&mut tx, &skipped_profiles, &skipped_runs).await? else {
                tx.commit().await?;
                return Ok(None);
            };
            let run = c.run;
            let reclaimed = c.state == "leased";
            let pinned_rev = self.pinned_revision();
            let pinned = pinned_rev.as_ref().map(|set| set.0.as_str());
            let forgiven = reclaimed && !handover::spends_attempt(c.claim_set.as_deref(), pinned);
            let attempts = run.attempts + i32::from(reclaimed && !forgiven);
            let end = if run.cancel_requested {
                End::Cancelled("cancelled")
            } else if reclaimed && attempts >= self.cfg.max_attempts as i32 {
                End::Dead("lease_expired", attempts)
            } else {
                match load(&mut tx, &run.profile, &run.id).await {
                    Ok(mut claimed) => {
                        if let Err(error) = self
                            .lease_run(&mut tx, &mut claimed.attempt, attempts)
                            .await
                        {
                            // Another run with the same serial key won the unique lease index.
                            return if error.is_unique_violation() {
                                Ok(None)
                            } else {
                                Err(error)
                            };
                        }
                        if reclaimed {
                            let mut audits = Vec::new();
                            crate::approvals::supersede_generation_into(
                                &mut tx,
                                &run.profile,
                                &run.id,
                                c.generation,
                                &mut audits,
                            )
                            .await?;
                            let reason = if forgiven {
                                "set_replaced"
                            } else {
                                "lease_expired"
                            };
                            audits.push(OwnedEntry::new(
                                &run.profile,
                                "system",
                                "run.reclaim",
                                &run.id,
                                reason,
                            ));
                            audit::append_many(&mut tx, &audits).await?;
                        }
                        tx.commit().await?;
                        return Ok(Some(claimed));
                    }
                    // Isolate corrupted persisted targets as `Dead`; propagate SQL errors by rolling back.
                    Err(Fail::Core(Error::Invalid)) => End::Dead("invalid_target", attempts),
                    Err(error) => return Err(error),
                }
            };
            if !try_lock_parent(&mut tx, &run.profile, run.parent.as_deref()).await? {
                // Skip rather than block to respect parent -> child lock order.
                tx.rollback().await?;
                skipped_profiles.push(run.profile);
                skipped_runs.push(run.id);
                continue;
            }
            end_run(&mut tx, &run, end, "system").await?;
            tx.commit().await?;
        }
        Ok(None)
    }

    async fn lease_run(&self, tx: &mut Tx<'_>, at: &mut Attempt, attempts: i32) -> R<()> {
        let (generation, claim): (i64, i64) = sqlx::query_as(
            "update bot_runs set state = 'leased',
                    generation = case when keep_generation then generation else generation + 1 end,
                    keep_generation = false, claim_epoch = claim_epoch + 1, lease_owner = $3,
                    lease_until_ms = bot_now_ms() + $4, attempts = $5, wait_kind = null, wait_child = null,
                    claim_set = $6, updated_ms = bot_now_ms()
             where profile = $1 and id = $2 returning generation, claim_epoch",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&self.cfg.worker)
        .bind(self.cfg.lease.as_millis() as i64)
        .bind(attempts)
        .bind(self.pinned_revision().map(|set| set.0))
        .fetch_one(&mut **tx)
        .await?;
        at.generation = Generation(generation as u64);
        at.claim = Some(ClaimEpoch(claim as u64));
        Ok(())
    }

    pub(crate) async fn renew_run(&self, at: &Attempt) -> R<Beat> {
        let mut tx = self.pool.begin().await?;
        let run = match lock_leased(&mut tx, at).await {
            Ok(run) if run.private => return Err(Error::Denied.into()),
            Err(Fail::Core(Error::Stale)) => {
                // Private DM runs are managed exclusively by `PrivateLedger` regardless of lease state.
                let private: Option<bool> = sqlx::query_scalar(
                    "select private from bot_runs where profile = $1 and id = $2",
                )
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .fetch_optional(&mut *tx)
                .await?;
                return Err(if private == Some(true) {
                    Error::Denied
                } else {
                    Error::Stale
                }
                .into());
            }
            other => other?,
        };
        sqlx::query(
            "update bot_runs set lease_until_ms = bot_now_ms() + $3, updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .bind(self.cfg.lease.as_millis() as i64)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Beat {
            cancel_requested: run.cancel_requested,
        })
    }

    /// Idempotently writes a batch of checkpoints under the active lease; rolls back with `Conflict` on body mismatch.
    pub(crate) async fn checkpoint_batch_run(&self, at: &Attempt, steps: Vec<Step>) -> R<()> {
        if steps.is_empty() || steps.len() > MAX_BATCH {
            return invalid();
        }
        let mut bodies = Vec::with_capacity(steps.len());
        for step in &steps {
            ids_ok(&[&step.key])?;
            bodies.push(canonical(&step.body)?.1);
        }
        let mut tx = self.pool.begin().await?;
        if lock_leased(&mut tx, at).await?.private {
            return Err(Error::Denied.into());
        }
        for (step, body) in steps.iter().zip(&bodies) {
            let fresh = sqlx::query("insert into bot_steps (profile, run, key, body) values ($1, $2, $3, $4) on conflict do nothing")
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .bind(&step.key)
                .bind(body)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                == 1;
            if !fresh {
                let stored: String = sqlx::query_scalar(
                    "select body from bot_steps where profile = $1 and run = $2 and key = $3",
                )
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .bind(&step.key)
                .fetch_one(&mut *tx)
                .await?;
                if stored != *body {
                    return Err(Error::Conflict.into());
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn fail_run(&self, at: &Attempt, failure: Failure) -> R<Retry> {
        let mut tx = self.pool.begin().await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        let outcome = self.settle_failure(&mut tx, &run, failure).await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// Loads prior agent turns in chronological order scoped by conversation and principal boundaries, propagating private-recall taint.
    pub(crate) async fn history_of(&self, at: &Attempt, limit: u32) -> R<Vec<Turn>> {
        let (mut tx, me) = crate::memory::begin_read(self, at).await?;
        let root: String = me.get("root");
        let kind: String = me.get("conv_kind");
        // Delegated children inherit the root run's agent and trigger for history scoping.
        let origin =
            sqlx::query("select agent, trigger from bot_runs where profile = $1 and id = $2")
                .bind(&at.profile.0)
                .bind(&root)
                .fetch_one(&mut *tx)
                .await?;
        // Isolate HTTP turns by principal and scheduled turns by `(root_agent, trigger, principal)`.
        let rows = sqlx::query(
            "select u.principal, u.input, u.result,
                    exists (select 1 from bot_runs t where t.profile = u.profile and t.id = u.root and t.private_memory_read) as tainted
             from bot_runs u
             where u.profile = $1 and u.binding = $2 and u.conv_kind = $3 and u.conv_id = $4
               and u.thread is not distinct from $5 and u.parent is null and u.state = 'succeeded' and not u.private
               and u.target->>'kind' = 'agent' and u.id <> $6 and u.created_ms <= $7
               and ($3 <> 'http' or u.principal = $8)
               and ($3 <> 'scheduled' or (u.agent = $9 and u.trigger is not distinct from $10 and u.principal = $8))
             order by u.created_ms desc, u.id desc limit $11",
        )
        .bind(&at.profile.0)
        .bind(me.get::<String, _>("binding"))
        .bind(&kind)
        .bind(me.get::<String, _>("conv_id"))
        .bind(me.get::<Option<String>, _>("thread"))
        .bind(&at.run.0)
        .bind(me.get::<i64, _>("created_ms"))
        .bind(me.get::<String, _>("principal"))
        .bind(origin.get::<String, _>("agent"))
        .bind(origin.get::<Option<String>, _>("trigger"))
        .bind(i64::from(limit.min(200)))
        .fetch_all(&mut *tx)
        .await?;
        // Propagate private-memory taint into this run tree before returning tainted prior turns.
        if rows.iter().any(|row| row.get::<bool, _>("tainted")) {
            crate::memory::flag_private(&mut tx, at, &root).await?;
        }
        tx.commit().await?;
        let mut turns = Vec::new();
        for row in rows.into_iter().rev() {
            turns.push(Turn {
                from: Some(PrincipalId(row.get("principal"))),
                text: text_of(&row.get::<String, _>("input")),
            });
            let result: Option<String> = row.get("result");
            turns.push(Turn {
                from: None,
                text: result.map(|r| text_of(&r)).unwrap_or_default(),
            });
        }
        Ok(turns)
    }
}

impl Store {
    /// Checks under `bot_active FOR SHARE` whether this handle's pinned active set has been replaced.
    /// Lock order: `bot_active` -> (parent) -> run -> audit head.
    async fn superseded(&self, tx: &mut Tx<'_>) -> R<bool> {
        let Some(pinned) = self.pinned_revision() else {
            return Ok(false);
        };
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active where singleton for share")
                .fetch_optional(&mut **tx)
                .await?;
        Ok(
            matches!(current.as_deref(), Some(revision) if !revision.is_empty() && revision != pinned.0),
        )
    }

    async fn requeue_run(&self, tx: &mut Tx<'_>, run: &RunRow) -> R<()> {
        sqlx::query(
            "update bot_runs set state = 'ready', lease_until_ms = null, lease_owner = null,
                    updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .execute(&mut **tx)
        .await?;
        audit::append(
            tx,
            &run.profile,
            &Entry::new("system", "run.handover", &run.id, "stale_worker"),
        )
        .await?;
        Ok(())
    }

    /// Requeues a leased run without spending an attempt only when this handle's pinned active set has been superseded.
    pub(crate) async fn release_stale_run(&self, at: &Attempt) -> R<()> {
        let mut tx = self.pool.begin().await?;
        if !self.superseded(&mut tx).await? {
            return Err(Error::Denied.into());
        }
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(Error::Denied.into());
        }
        self.requeue_run(&mut tx, &run).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn release_stale(&self, at: &Attempt) -> Result<(), Error> {
        self.release_stale_run(at).await.map_err(Fail::into_core)
    }

    /// Atomically decides under `bot_active FOR SHARE` between stale-worker handover (`Retry::After(0)`) and normal failure settlement.
    pub(crate) async fn fail_or_release_run(&self, at: &Attempt, failure: Failure) -> R<Retry> {
        let mut tx = self.pool.begin().await?;
        let stale = self.superseded(&mut tx).await?;
        let parent = parent_of(&mut tx, at).await?;
        lock_parent(&mut tx, &at.profile.0, parent.as_deref()).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private {
            return Err(Error::Denied.into());
        }
        let outcome = if stale && !run.cancel_requested {
            self.requeue_run(&mut tx, &run).await?;
            Retry::After(Duration::ZERO)
        } else {
            self.settle_failure(&mut tx, &run, failure).await?
        };
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn fail_or_release(&self, at: &Attempt, failure: Failure) -> Result<Retry, Error> {
        self.fail_or_release_run(at, failure)
            .await
            .map_err(Fail::into_core)
    }
}

impl Store {
    async fn settle_failure(&self, tx: &mut Tx<'_>, run: &RunRow, failure: Failure) -> R<Retry> {
        Ok(match failure {
            Failure::Permanent("cancelled") | Failure::Transient(_) if run.cancel_requested => {
                end_run(tx, run, End::Cancelled("cancelled"), "system").await?;
                Retry::Dead
            }
            Failure::Permanent(code) => {
                end_run(tx, run, End::Dead(code, run.attempts), "system").await?;
                Retry::Dead
            }
            Failure::Transient(code) => {
                let attempts = run.attempts + 1;
                if attempts >= self.cfg.max_attempts as i32 {
                    end_run(tx, run, End::Dead(code, attempts), "system").await?;
                    Retry::Dead
                } else {
                    let delay: i64 = sqlx::query_scalar(
                        "update bot_runs set state = 'retry', attempts = $3, error = $4, lease_until_ms = null, lease_owner = null,
                                not_before_ms = bot_now_ms()
                                    + (least($5::float8, $6::float8 * power(2::float8, ($3 - 1)::float8)) * (0.75 + random() / 2))::bigint,
                                updated_ms = bot_now_ms()
                         where profile = $1 and id = $2
                         returning not_before_ms - bot_now_ms()",
                    )
                    .bind(&run.profile)
                    .bind(&run.id)
                    .bind(attempts)
                    .bind(code)
                    .bind(self.cfg.backoff_cap.as_millis() as f64)
                    .bind(self.cfg.backoff_base.as_millis() as f64)
                    .fetch_one(&mut **tx)
                    .await?;
                    audit::append(
                        tx,
                        &run.profile,
                        &Entry::new("system", "run.retry", &run.id, code),
                    )
                    .await?;
                    Retry::After(Duration::from_millis(delay.max(0) as u64))
                }
            }
        })
    }
}
