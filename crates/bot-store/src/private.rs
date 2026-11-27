//! Sensitive DM intake ledger (`PrivateLedger`); stores only request keys and HMAC digests, never payloads or receipts.
//! Lease-expired `leased` rows move to durable `waiting/hold` regardless of `private_started_ms` and require operator resolution.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    id::*,
    ledger::{PrivateOutcome, PrivatePermit, PrivateRequest, PrivateStart},
    port::Error,
    state::ConversationKind,
};
use sqlx::{Row, postgres::PgRow};

fn grant(
    profile: &ProfileId,
    run: &str,
    generation: i64,
    epoch: i64,
    allow_pass: bool,
) -> PrivatePermit {
    PrivatePermit::Start {
        attempt: Attempt {
            profile: profile.clone(),
            run: RunId(run.to_owned()),
            generation: Generation(generation as u64),
            claim: Some(ClaimEpoch(epoch as u64)),
        },
        allow_pass,
    }
}

async fn to_hold(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<()> {
    sqlx::query(
        "update bot_runs set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
                updated_ms = bot_now_ms() where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(run)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Checks generation and claim epoch without checking lease expiry.
fn owned(row: &PgRow, at: &Attempt) -> bool {
    row.get::<i64, _>("generation") as u64 == at.generation.0
        && at.claim == Some(ClaimEpoch(row.get::<i64, _>("claim_epoch") as u64))
}

impl Store {
    pub(crate) async fn private_begin(&self, r: PrivateRequest) -> R<PrivatePermit> {
        let k = &r.key;
        ids_ok(&[
            &k.profile.0,
            &k.binding.0,
            &k.principal.0,
            &k.request.0,
            &r.revision.0,
            &r.agent.0,
            &r.tool.0,
            &r.conversation.id,
        ])?;
        // Sensitive intake is accepted in direct messages only.
        if r.conversation.kind != ConversationKind::Direct
            || r.conversation.thread.as_deref().is_some_and(|t| !id_ok(t))
            || r.scopes.iter().any(|s| !id_ok(s))
        {
            return invalid();
        }
        let scopes: Vec<String> = r.scopes.iter().cloned().collect();
        let mut tx = self.begin_authorized().await?;
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        let fingerprint = r.pass_fingerprint.as_ref().unwrap_or(&r.fingerprint);
        let fresh = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run)
             values ($1, $2, $3, $4, $5, $6) on conflict do nothing",
        )
        .bind(&k.profile.0)
        .bind(&k.binding.0)
        .bind(&k.principal.0)
        .bind(&k.request.0)
        .bind(fingerprint.as_slice())
        .bind(&run)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !fresh {
            return self.private_replay(tx, &r).await;
        }
        // Validate active revision only for fresh requests.
        crate::active::guard_active(&mut tx, &k.profile.0, &r.revision).await?;
        let target = target_json(&bot_core::command::Target::Tool {
            agent: r.agent.clone(),
            tool: r.tool.clone(),
            args: bot_core::command::JsonPayload("{}".into()),
        })?;
        sqlx::query(
            "insert into bot_runs (profile, id, root, agent, principal, scopes, revision, binding, conv_kind, conv_id, thread,
                                   target, input, private, state, generation, claim_epoch, lease_owner, lease_until_ms, private_pass)
             values ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'null', true, 'leased', 1, 1, $12,
                     bot_now_ms() + $13, $14)",
        )
        .bind(&k.profile.0)
        .bind(&run)
        .bind(&r.agent.0)
        .bind(&k.principal.0)
        .bind(&scopes)
        .bind(&r.revision.0)
        .bind(&k.binding.0)
        .bind(kind_str(&r.conversation.kind))
        .bind(&r.conversation.id)
        .bind(&r.conversation.thread)
        .bind(&target)
        .bind(&self.cfg.worker)
        .bind(self.cfg.delivery_lease.as_millis() as i64)
        .bind(r.pass_fingerprint.map(|_| "bound_v2"))
        .execute(&mut *tx)
        .await?;
        let mut entry = Entry::new(&k.principal.0, "private.begin", &run, "started");
        entry.args = Some(*fingerprint);
        audit::append(&mut tx, &k.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(grant(&k.profile, &run, 1, 1, r.pass_fingerprint.is_some()))
    }

    /// Replays a duplicate key: rejects field mismatches with `Conflict`, reclaims explicitly returned `ready` rows, and holds lease-expired rows.
    async fn private_replay(&self, mut tx: Tx<'_>, r: &PrivateRequest) -> R<PrivatePermit> {
        let k = &r.key;
        let row = sqlx::query(
            "select q.input_sha256, u.private_pass, u.id, u.state, u.private, u.agent, u.conv_kind, u.conv_id, u.thread,
                    u.target->>'tool' as tool, u.lease_until_ms
             from bot_requests q join bot_runs u on u.profile = q.profile and u.id = q.run
             where q.profile = $1 and q.binding = $2 and q.principal = $3 and q.request = $4
             for update of u",
        )
        .bind(&k.profile.0)
        .bind(&k.binding.0)
        .bind(&k.principal.0)
        .bind(&k.request.0)
        .fetch_one(&mut *tx)
        .await?;
        // Require fingerprint, agent, tool, and conversation to match so redeliveries cannot redirect replies.
        let pass: Option<String> = row.get("private_pass");
        let fingerprint = match pass.as_deref() {
            None => r.fingerprint,
            Some("bound_v2" | "passed_v2") => r.pass_fingerprint.ok_or(Error::Conflict)?,
            Some(_) => return invalid(),
        };
        let same = row.get::<Vec<u8>, _>("input_sha256") == fingerprint
            && row.get::<bool, _>("private")
            && text(&row, "agent") == r.agent.0
            && row.get::<Option<String>, _>("tool").as_deref() == Some(r.tool.0.as_str())
            && text(&row, "conv_kind") == kind_str(&r.conversation.kind)
            && text(&row, "conv_id") == r.conversation.id
            && row.get::<Option<String>, _>("thread") == r.conversation.thread;
        if !same {
            tx.rollback().await?;
            return Err(Error::Conflict.into());
        }
        // Read DB clock only after acquiring the row lock.
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let run: String = row.get("id");
        let live = row
            .get::<Option<i64>, _>("lease_until_ms")
            .is_some_and(|until| until >= now);
        match (text(&row, "state").as_str(), live) {
            ("succeeded", _) if pass.as_deref() == Some("passed_v2") => {
                finish_read(tx, PrivatePermit::Passed).await
            }
            ("succeeded", _) => finish_read(tx, PrivatePermit::Completed).await,
            ("dead" | "cancelled", _) => finish_read(tx, PrivatePermit::Rejected).await,
            ("waiting", _) => finish_read(tx, PrivatePermit::Held).await,
            ("leased", true) => finish_read(tx, PrivatePermit::InProgress).await,
            ("leased", false) => {
                // Lease expired: outcome is uncertain regardless of start marker, so hold without re-executing.
                to_hold(&mut tx, &k.profile.0, &run).await?;
                let entry = Entry::new("system", "private.held", &run, "lease_expired");
                audit::append(&mut tx, &k.profile.0, &entry).await?;
                tx.commit().await?;
                Ok(PrivatePermit::Held)
            }
            ("ready", _) => {
                // Explicitly released before send: reclaim with a fresh claim epoch while preserving generation.
                let claimed = sqlx::query(
                    "update bot_runs set state = 'leased', claim_epoch = claim_epoch + 1,
                            lease_owner = $3, lease_until_ms = bot_now_ms() + $4, updated_ms = bot_now_ms()
                     where profile = $1 and id = $2 returning generation, claim_epoch",
                )
                .bind(&k.profile.0)
                .bind(&run)
                .bind(&self.cfg.worker)
                .bind(self.cfg.delivery_lease.as_millis() as i64)
                .fetch_one(&mut *tx)
                .await?;
                let entry = Entry::new(&k.principal.0, "private.begin", &run, "reclaimed");
                audit::append(&mut tx, &k.profile.0, &entry).await?;
                tx.commit().await?;
                Ok(grant(
                    &k.profile,
                    &run,
                    claimed.get("generation"),
                    claimed.get("claim_epoch"),
                    pass.is_some(),
                ))
            }
            _ => finish_read(tx, PrivatePermit::InProgress).await,
        }
    }

    /// Promotes lease-expired private runs to durable holds so they appear in operator queues without waiting for redelivery.
    pub(crate) async fn sweep_private(&self) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let mut lost = sqlx::query(
            "with lost as (
                 select profile, id from bot_runs
                 where private and state = 'leased' and lease_until_ms < bot_now_ms()
                 order by profile, id for update skip locked)
             update bot_runs r set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
                    updated_ms = bot_now_ms()
             from lost where r.profile = lost.profile and r.id = lost.id
             returning r.profile, r.id",
        )
        .fetch_all(&mut *tx)
        .await?
        .iter()
        .map(|row| (row.get::<String, _>("profile"), row.get::<String, _>("id")))
        .collect::<Vec<_>>();
        if lost.is_empty() {
            tx.rollback().await?;
            return Ok(());
        }
        lost.sort();
        let audits: Vec<OwnedEntry> = lost
            .into_iter()
            .map(|(profile, run)| {
                OwnedEntry::new(profile, "system", "private.held", run, "lease_expired")
            })
            .collect();
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Records the one-time pre-send start marker under active revision, lease, tool, and digest checks.
    pub(crate) async fn private_start(&self, s: PrivateStart) -> R<()> {
        ids_ok(&[&s.tool.0, &s.checked_revision.0])?;
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        // Lock order: `bot_active` -> run.
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let row = sqlx::query(
            "select r.private, r.state, r.generation, r.claim_epoch, r.private_started_ms, r.lease_until_ms,
                    r.principal, r.target->>'tool' as tool,
                    (select q.input_sha256 from bot_requests q where q.profile = r.profile and q.run = r.id) as fingerprint
             from bot_runs r where r.profile = $1 and r.id = $2 for update of r",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if !row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let live = text(&row, "state") == "leased"
            && owned(&row, at)
            && row
                .get::<Option<i64>, _>("lease_until_ms")
                .is_some_and(|until| until >= now);
        if !live {
            return Err(Error::Stale.into());
        }
        if row.get::<Option<i64>, _>("private_started_ms").is_some() {
            return Err(Error::Conflict.into());
        }
        if row.get::<Option<String>, _>("tool").as_deref() != Some(s.tool.0.as_str())
            || row.get::<Option<Vec<u8>>, _>("fingerprint").as_deref()
                != Some(s.fingerprint.as_slice())
        {
            return Err(Error::Denied.into());
        }
        sqlx::query(
            "update bot_runs set private_started_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .execute(&mut *tx)
        .await?;
        let principal: String = row.get("principal");
        let entry = Entry::new(&principal, "private.start", &at.run.0, "started");
        audit::append(&mut tx, &at.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Settles a private run; after lease expiry or hold, only late `Completed`/`Passed` on a started row is accepted.
    pub(crate) async fn private_finish(&self, permit: &Attempt, outcome: PrivateOutcome) -> R<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "select state, generation, claim_epoch, private, private_started_ms, private_pass, principal, lease_until_ms
             from bot_runs where profile = $1 and id = $2 for update",
        )
        .bind(&permit.profile.0)
        .bind(&permit.run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if !row.get::<bool, _>("private") {
            return Err(Error::Denied.into());
        }
        if !owned(&row, permit) {
            return Err(Error::Stale.into());
        }
        let state = text(&row, "state");
        let started = row.get::<Option<i64>, _>("private_started_ms").is_some();
        // Read DB clock only after acquiring the row lock.
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        let expired = state == "leased"
            && row
                .get::<Option<i64>, _>("lease_until_ms")
                .is_none_or(|until| until < now);
        if expired
            && !matches!(
                (&outcome, started),
                (PrivateOutcome::Completed | PrivateOutcome::Passed, true)
            )
        {
            // Lease expired: persist durable hold and audit before returning, never reverting to release/reject.
            to_hold(&mut tx, &permit.profile.0, &permit.run.0).await?;
            let entry = Entry::new("system", "private.held", &permit.run.0, "lease_expired");
            audit::append(&mut tx, &permit.profile.0, &entry).await?;
            tx.commit().await?;
            return match (&outcome, started) {
                (PrivateOutcome::Held, true) => Ok(()),
                _ => Err(Error::Conflict.into()),
            };
        }
        let (sql, verdict) = match (&outcome, state.as_str(), started) {
            (PrivateOutcome::RetryBeforeStart, "leased", false) => (RELEASE, "retry_before_start"),
            (PrivateOutcome::NotSent, "leased", true) => (RELEASE, "not_sent"),
            (PrivateOutcome::Completed, "leased" | "waiting", true) => (FINISH_OK, "completed"),
            (PrivateOutcome::Passed, "leased" | "waiting", true) => {
                if row.get::<Option<String>, _>("private_pass").as_deref() != Some("bound_v2") {
                    return Err(Error::Denied.into());
                }
                (FINISH_PASSED, "passed")
            }
            (PrivateOutcome::Held, "leased", true) => (HOLD, "held"),
            (PrivateOutcome::Rejected, "leased", _) => (FINISH_REJECTED, "rejected"),
            _ => return Err(Error::Conflict.into()),
        };
        sqlx::query(sql)
            .bind(&permit.profile.0)
            .bind(&permit.run.0)
            .execute(&mut *tx)
            .await?;
        let principal: String = row.get("principal");
        let entry = Entry::new(&principal, "private.finish", &permit.run.0, verdict);
        audit::append(&mut tx, &permit.profile.0, &entry).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Releases the lease and clears `private_started_ms` when confirmed unsent so redelivery can reclaim it.
const RELEASE: &str = "update bot_runs set state = 'ready', wait_kind = null, lease_until_ms = null, lease_owner = null,
        private_started_ms = null, updated_ms = bot_now_ms() where profile = $1 and id = $2";
const HOLD: &str = "update bot_runs set state = 'waiting', wait_kind = 'hold', lease_until_ms = null, lease_owner = null,
        updated_ms = bot_now_ms() where profile = $1 and id = $2";
const FINISH_OK: &str = "update bot_runs set state = 'succeeded',
        error = null, wait_kind = null, lease_until_ms = null, lease_owner = null,
        finished_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2";
const FINISH_PASSED: &str = "update bot_runs set state = 'succeeded', private_pass = 'passed_v2',
        error = null, wait_kind = null, lease_until_ms = null, lease_owner = null,
        finished_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2";
const FINISH_REJECTED: &str = "update bot_runs set state = 'dead', error = 'rejected', wait_kind = null, lease_until_ms = null, lease_owner = null,
        finished_ms = bot_now_ms(), updated_ms = bot_now_ms() where profile = $1 and id = $2";

async fn finish_read(tx: Tx<'_>, permit: PrivatePermit) -> R<PrivatePermit> {
    tx.rollback().await?;
    Ok(permit)
}

use crate::db::text;
