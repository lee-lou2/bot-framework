//! Reply delivery outbox: claim, activation-fenced begin, and idempotent settlement.
//!
//! Delivery generation is independent of run generation and increments per claim. Unbegun lease expiries
//! are reclaimable; expired `sending` rows move to `held` to prevent duplicate external sends.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{R, Store},
};
use bot_core::{
    command::JsonPayload,
    id::*,
    intake::{DeliveryKey, DeliveryOutcome, DeliveryScope, NotSentReason, Outgoing},
    port::Error,
    state::Conversation,
};
use sqlx::Row;

const MAX_BATCH: u32 = 100;

fn receipt_ok(receipt: &str) -> bool {
    id_ok(receipt) && receipt.len() <= 512
}

fn bind_key<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    key: &'q DeliveryKey,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(&key.profile.0)
        .bind(&key.run.0)
        .bind(i32::try_from(key.seq).unwrap_or(i32::MAX))
}

impl Store {
    /// Moves expired `sending` rows to `held`, then claims up to `limit` eligible replies in per-run `seq` order.
    pub(crate) async fn outbox_claim(&self, scope: &DeliveryScope, limit: u32) -> R<Vec<Outgoing>> {
        if limit == 0 {
            return Ok(vec![]);
        }
        let (profile, binding) = scope
            .binding
            .as_ref()
            .map_or((None, None), |(profile, binding)| {
                (Some(profile.0.as_str()), Some(binding.0.as_str()))
            });
        let mut tx = self.begin_authorized().await?;
        let mut lost: Vec<(String, String, i32, String)> = sqlx::query_as(
            "with lost as (
                 select profile, run, seq from bot_outbox
                 where state = 'sending' and lease_until_ms < (select bot_now_ms())
                 order by profile, run, seq for update skip locked
             )
             update bot_outbox o set state = 'held', reason = 'owner_lost', updated_ms = bot_now_ms()
             from lost where o.profile = lost.profile and o.run = lost.run and o.seq = lost.seq
             returning o.profile, o.run, o.seq, o.call",
        )
        .fetch_all(&mut *tx)
        .await?;
        lost.sort_by(|a, b| (&a.0, &a.1, a.2).cmp(&(&b.0, &b.1, b.2)));
        let audits: Vec<OwnedEntry> = lost
            .into_iter()
            .map(|(profile, _, _, call)| {
                OwnedEntry::new(profile, "system", "outbox.held", call, "owner_lost")
            })
            .collect();
        // Preserve per-run reply order by skipping rows whose earlier `seq` is still pending or sending.
        let rows = sqlx::query(
            "select o.profile, o.run, o.seq from bot_outbox o
             where o.state = 'pending' and o.transport = $1
               and ($3::text is null or (o.profile = $3 and o.binding = $4))
               and o.not_before_ms <= (select bot_now_ms())
               and (o.lease_until_ms is null or o.lease_until_ms < (select bot_now_ms()))
               and not exists (select 1 from bot_outbox p
                               where p.profile = o.profile and p.run = o.run and p.seq < o.seq
                                 and p.state in ('pending', 'sending'))
             order by o.created_ms, o.profile, o.run, o.seq
             limit $2 for update of o skip locked",
        )
        .bind(scope.transport.as_str())
        .bind(i64::from(limit.min(MAX_BATCH)))
        .bind(profile)
        .bind(binding)
        .fetch_all(&mut *tx)
        .await?;
        let mut out = Vec::new();
        for row in rows {
            let (profile, run, seq): (String, String, i32) =
                (row.get("profile"), row.get("run"), row.get("seq"));
            let claimed = sqlx::query(
                "update bot_outbox set generation = generation + 1, lease_until_ms = bot_now_ms() + $4, updated_ms = bot_now_ms()
                 where profile = $1 and run = $2 and seq = $3
                 returning generation, call, binding, principal, conv_kind, conv_id, thread, body",
            )
            .bind(&profile)
            .bind(&run)
            .bind(seq)
            .bind(self.cfg.delivery_lease.as_millis() as i64)
            .fetch_one(&mut *tx)
            .await?;
            out.push(Outgoing {
                transport: scope.transport.clone(),
                key: DeliveryKey {
                    profile: ProfileId(profile),
                    run: RunId(run),
                    seq: seq as u32,
                    generation: Generation(claimed.get::<i64, _>("generation") as u64),
                },
                call: CallId(claimed.get("call")),
                binding: BindingId(claimed.get("binding")),
                principal: PrincipalId(claimed.get("principal")),
                destination: Conversation {
                    kind: kind_from(&claimed.get::<String, _>("conv_kind"))?,
                    id: claimed.get("conv_id"),
                    thread: claimed.get("thread"),
                },
                body: JsonPayload(claimed.get("body")),
            });
        }
        // Acquire the audit head lock once, right before commit, after all claim writes.
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(out)
    }

    /// Transitions a leased row to `sending` after checking the active revision and lease; retries return `Conflict`.
    pub(crate) async fn outbox_begin(&self, key: &DeliveryKey, checked: &Revision) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        // Serialize against activation swaps so the authorized profile revision is still active.
        crate::active::guard_active(&mut tx, &key.profile.0, checked).await?;
        let row = bind_key(
            sqlx::query("select state, generation, lease_until_ms, call from bot_outbox where profile = $1 and run = $2 and seq = $3 for update"),
            key,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let (state, generation): (String, i64) = (row.get("state"), row.get("generation"));
        if generation as u64 != key.generation.0 {
            return Err(Error::Stale.into());
        }
        if state == "sending" {
            return Err(Error::Conflict.into());
        }
        let leased = row
            .get::<Option<i64>, _>("lease_until_ms")
            .is_some_and(|until| until >= now);
        if state != "pending" || !leased {
            return Err(Error::Stale.into());
        }
        bind_key(
            sqlx::query("update bot_outbox set state = 'sending', updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
            key,
        )
        .execute(&mut *tx)
        .await?;
        let call: String = row.get("call");
        audit::append(
            &mut tx,
            &key.profile.0,
            &Entry::new("gateway", "outbox.begin", &call, &generation.to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Settles a `sending` delivery (or upgrades a `held` row on late confirmation) idempotently with audit.
    pub(crate) async fn outbox_settle(&self, key: &DeliveryKey, outcome: DeliveryOutcome) -> R<()> {
        if let DeliveryOutcome::Confirmed { receipt } = &outcome
            && !receipt_ok(receipt)
        {
            return invalid();
        }
        let mut tx = self.pool.begin().await?;
        let row = bind_key(
            sqlx::query("select state, generation, attempts, call, receipt from bot_outbox where profile = $1 and run = $2 and seq = $3 for update"),
            key,
        )
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let (state, call): (String, String) = (row.get("state"), row.get("call"));
        if row.get::<i64, _>("generation") as u64 != key.generation.0 {
            return Err(Error::Stale.into());
        }
        let (target, verdict) = match &outcome {
            DeliveryOutcome::Confirmed { .. } => ("confirmed", "confirmed".to_owned()),
            DeliveryOutcome::Rejected => ("rejected", "rejected".to_owned()),
            DeliveryOutcome::Held => ("held", "unknown_outcome".to_owned()),
            DeliveryOutcome::NotSent { reason, .. } => (
                "pending",
                format!(
                    "not_sent:{}",
                    match reason {
                        NotSentReason::BeforeWrite => "before_write",
                        NotSentReason::RateLimited => "rate_limited",
                    }
                ),
            ),
        };
        let late_confirm = matches!(outcome, DeliveryOutcome::Confirmed { .. }) && state == "held";
        if state == target && target != "pending" {
            let same_receipt = match &outcome {
                DeliveryOutcome::Confirmed { receipt } => {
                    row.get::<Option<String>, _>("receipt").as_deref() == Some(receipt)
                }
                _ => true,
            };
            tx.rollback().await?;
            return if same_receipt {
                Ok(())
            } else {
                Err(Error::Conflict.into())
            };
        }
        if state != "sending" && !late_confirm {
            return Err(Error::Stale.into());
        }
        let attempts: i32 = row.get("attempts");
        let mut verdict = verdict;
        match outcome {
            DeliveryOutcome::Confirmed { receipt } => {
                bind_key(
                    sqlx::query("update bot_outbox set state = 'confirmed', receipt = $4, reason = null, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                    key,
                )
                .bind(receipt)
                .execute(&mut *tx)
                .await?;
            }
            DeliveryOutcome::Rejected | DeliveryOutcome::Held => {
                let reason = (target == "held").then_some("unknown_outcome");
                bind_key(
                    sqlx::query("update bot_outbox set state = $4, reason = $5, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                    key,
                )
                .bind(target)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
            }
            DeliveryOutcome::NotSent { retry_after, .. } => {
                let attempts = attempts + 1;
                if attempts >= self.cfg.max_attempts as i32 {
                    // Retries exhausted: hold for operator visibility as a terminal failure.
                    verdict = "retries_exhausted".to_owned();
                    bind_key(
                        sqlx::query("update bot_outbox set state = 'held', reason = 'retries_exhausted', attempts = $4, lease_until_ms = null, updated_ms = bot_now_ms() where profile = $1 and run = $2 and seq = $3"),
                        key,
                    )
                    .bind(attempts)
                    .execute(&mut *tx)
                    .await?;
                } else {
                    // Delay next claim by the greater of exponential jitter backoff and provider `retry_after`.
                    let floor = retry_after.map_or(0, |d| d.as_millis() as i64);
                    bind_key(
                        sqlx::query(
                            "update bot_outbox set state = 'pending', attempts = $4, lease_until_ms = null, updated_ms = bot_now_ms(),
                                    not_before_ms = bot_now_ms() + greatest($7::bigint,
                                        (least($5::float8, $6::float8 * power(2::float8, ($4 - 1)::float8)) * (0.75 + random() / 2))::bigint)
                             where profile = $1 and run = $2 and seq = $3",
                        ),
                        key,
                    )
                    .bind(attempts)
                    .bind(self.cfg.backoff_cap.as_millis() as f64)
                    .bind(self.cfg.backoff_base.as_millis() as f64)
                    .bind(floor)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        audit::append(
            &mut tx,
            &key.profile.0,
            &Entry::new("gateway", "outbox.settle", &call, &verdict),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
