//! Owner-DM approval card outbox (`bot_card_jobs`): claim, first-sender begin, settlement, and receipts.
//!
//! Cards stay closed until a `Posted` receipt commits, opening the card and enqueuing `DM_SENT_NOTICE`.
//! `Unavailable` and `Held` (including stalled `sending` jobs) close the approval as `approval_dm_unavailable`
//! without auto-reposting. Lock order: active set -> run -> call -> approval -> job -> draft -> audit head.

mod closing;

use crate::{
    approvals::lock_approval,
    db::{Fail, R, Store, Tx},
    finish::push_reply,
    replies,
    unsent::lock_run,
};
use bot_core::{
    api::ApprovalBlock,
    cards::{
        CardJob, CardKey, CardOrigin, CardOutbox, CardOutcome, CardReceipt, CardSource,
        DM_SENT_NOTICE,
    },
    command::JsonPayload,
    id::*,
    intake::PushTransportId,
    port::{Error, Reply},
};
use closing::Closing;
use sqlx::{Row, postgres::PgRow};

/// Claim lease duration before another dispatcher may reclaim an unbegun job.
const LEASE_MS: i64 = 90_000;
const BACKOFF_MS: i64 = 5_000;
/// Max claim attempts before closing the job as unavailable.
const MAX_ATTEMPTS: i32 = 6;

struct Locks {
    active: Option<String>,
    run: crate::unsent::Locked,
    call_state: Option<String>,
    approval: PgRow,
    job: PgRow,
}

/// Locks active set -> run -> call -> approval -> job in order, verifying the claim attempt matches.
async fn lock_attempt(tx: &mut Tx<'_>, key: &CardKey) -> R<Locks> {
    let active = lock_active_set(tx).await?;
    let nonce = &key.nonce.0;
    let seen = sqlx::query("select profile, run, call from bot_approvals where nonce = $1")
        .bind(nonce)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Missing)?;
    let (profile, run_id, call): (String, String, String) =
        (seen.get("profile"), seen.get("run"), seen.get("call"));
    let run = lock_run(tx, &profile, &run_id).await?;
    let call_state = sqlx::query_scalar::<_, String>(
        "select state from bot_calls where profile = $1 and run = $2 and call = $3 for update",
    )
    .bind(&profile)
    .bind(&run_id)
    .bind(&call)
    .fetch_optional(&mut **tx)
    .await?;
    let approval = lock_approval(tx, nonce).await?;
    let job = sqlx::query(
        "select profile, state, attempts, lease_until_ms, sending_ms, activation,
                team, dm, ts, origin_binding, origin_team
         from bot_card_jobs where nonce = $1 for update",
    )
    .bind(nonce)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    if key.attempt == 0 || i64::from(job.get::<i32, _>("attempts")) != i64::from(key.attempt) {
        return Err(Error::Stale.into());
    }
    Ok(Locks {
        active,
        run,
        call_state,
        approval,
        job,
    })
}

fn key_of(row: &PgRow) -> CardKey {
    CardKey {
        nonce: ApprovalId(row.get("nonce")),
        attempt: row.get::<i32, _>("attempts") as u32,
    }
}

fn lease_expired(job: &PgRow, now: i64) -> bool {
    job.get::<String, _>("state") == "leased" && job.get::<i64, _>("lease_until_ms") < now
}

/// True when the approval is pending/unexpired and the run is still waiting on the same generation without cancellation.
fn can_open(locks: &Locks, now: i64) -> bool {
    locks.approval.get::<String, _>("state") == "pending"
        && locks.approval.get::<i64, _>("expires_ms") > now
        && locks.run.state == "waiting"
        && !locks.run.row.cancel_requested
        && locks.run.generation == locks.approval.get::<i64, _>("generation")
}

async fn now_ms(tx: &mut Tx<'_>) -> R<i64> {
    Ok(sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?)
}

/// Takes `FOR SHARE` on `bot_active` first to preserve lock order; returns `None` before initial publish.
async fn lock_active_set(tx: &mut Tx<'_>) -> R<Option<String>> {
    Ok(
        sqlx::query_scalar::<_, String>(
            "select revision from bot_active where singleton for share",
        )
        .fetch_optional(&mut **tx)
        .await?
        .filter(|revision| !revision.is_empty()),
    )
}

fn job_card(card: &str) -> R<JsonPayload> {
    let value: serde_json::Value = serde_json::from_str(card).map_err(|_| Error::Invalid)?;
    let inner = value.get("approval").ok_or(Error::Invalid)?;
    Ok(JsonPayload(
        serde_json::to_string(inner).map_err(|_| Error::Invalid)?,
    ))
}

/// Extracts Slack channel/thread origin metadata from a joined `bot_runs` + `bot_requests` row (`None` for HTTP-origin cards).
fn card_source(row: &PgRow) -> Option<CardSource> {
    if row.get::<Option<String>, _>("origin_binding").is_some() {
        return None;
    }
    if row.get::<String, _>("conv_kind") != "channel" {
        return None;
    }
    if row.get::<Option<String>, _>("reply_transport").as_deref()
        != Some(PushTransportId::SLACK.as_str())
    {
        return None;
    }
    let channel: String = row.get("conv_id");
    let thread_ts: Option<String> = row.get("thread");
    let message_ts = thread_ts.as_ref().and_then(|_| {
        row.get::<Option<String>, _>("root_slack_key")
            .as_deref()
            .and_then(bot_core::slack::message_ts_from_key)
    });
    Some(CardSource {
        channel,
        thread_ts,
        message_ts,
    })
}

impl Store {
    /// Sweeps stale/stalled jobs, then claims up to `limit` pending, unexpired card jobs with a lease.
    pub(crate) async fn cards_claim(&self, limit: u32) -> R<Vec<CardJob>> {
        self.card_sweep().await?;
        let mut tx = self.begin_authorized().await?;
        let rows = sqlx::query(
            "with picked as (
                 select j.nonce from bot_card_jobs j join bot_approvals a on a.nonce = j.nonce
                 where ((j.state = 'staged' and j.not_before_ms <= bot_now_ms())
                        or (j.state = 'leased' and j.lease_until_ms < bot_now_ms()))
                   and a.state = 'pending' and a.expires_ms > bot_now_ms()
                 order by j.created_ms, j.nonce
                 limit $1 for update of j skip locked)
             update bot_card_jobs j
                set state = 'leased', lease_until_ms = bot_now_ms() + $2, attempts = j.attempts + 1, updated_ms = bot_now_ms()
               from picked, bot_approvals a, bot_runs r
              where j.nonce = picked.nonce and a.nonce = j.nonce
                and r.profile = j.profile and r.id = j.run
              returning j.nonce, j.attempts, j.profile, j.binding, j.owner, j.generation, j.created_ms, a.card,
                        j.origin_binding, r.conv_kind, r.conv_id, r.thread, r.reply_transport,
                        (select q.slack_key from bot_requests q
                          where q.profile = r.profile and q.run = r.root
                          order by q.created_ms asc limit 1) as root_slack_key",
        )
        .bind(i64::from(limit.min(64)))
        .bind(LEASE_MS)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut rows = rows;
        rows.sort_by_key(|row| {
            (
                row.get::<i64, _>("created_ms"),
                row.get::<String, _>("nonce"),
            )
        });
        rows.iter()
            .map(|row| {
                Ok(CardJob {
                    key: key_of(row),
                    profile: ProfileId(row.get("profile")),
                    binding: BindingId(row.get("binding")),
                    owner: PrincipalId(row.get("owner")),
                    card: job_card(&row.get::<String, _>("card"))?,
                    generation: Generation(row.get::<i64, _>("generation") as u64),
                    source: card_source(row),
                })
            })
            .collect()
    }

    /// Marks a leased job `sending` after checking active set, profile revision, lease, and approval validity; only the first caller succeeds.
    pub(crate) async fn cards_begin(&self, key: &CardKey, checked: &Revision) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        if locks.job.get::<String, _>("state") != "leased" {
            return Err(Error::Conflict.into());
        }
        let now = now_ms(&mut tx).await?;
        if lease_expired(&locks.job, now) || !can_open(&locks, now) {
            return Err(Error::Stale.into());
        }
        let profile: String = locks.job.get("profile");
        let current: Option<String> =
            sqlx::query_scalar("select revision from bot_active_profiles where profile = $1")
                .bind(&profile)
                .fetch_optional(&mut *tx)
                .await?;
        if current.as_deref() != Some(checked.0.as_str()) {
            return Err(Error::Stale.into());
        }
        let set = locks.active.as_deref().ok_or(Error::Stale)?;
        sqlx::query(
            "update bot_card_jobs
                set state = 'sending', sending_ms = bot_now_ms(), lease_until_ms = null, activation = $2, checked_revision = $3,
                    updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(set)
        .bind(&checked.0)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Idempotently settles a card job outcome; conflicting outcomes return `Conflict`, stale cards return `Stale`.
    pub(crate) async fn cards_settle(&self, key: &CardKey, outcome: CardOutcome) -> R<()> {
        match outcome {
            CardOutcome::Posted { team, dm, ts } => self.card_post(key, (&team, &dm, &ts)).await,
            CardOutcome::NotSent { retry_after, .. } => {
                let wait = retry_after.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
                self.card_retry(key, wait).await
            }
            CardOutcome::Unavailable => self.card_close(key, Closing::Unavailable).await,
            CardOutcome::Held => self.card_close(key, Closing::Held).await,
        }
    }

    /// Confirms DM delivery, persists the receipt to open the card, and enqueues `DM_SENT_NOTICE` in one commit.
    async fn card_post(&self, key: &CardKey, receipt: (&str, &str, &str)) -> R<()> {
        let (team, dm, ts) = receipt;
        if [team, dm, ts].iter().any(|v| !crate::codec::id_ok(v)) {
            return crate::codec::invalid();
        }
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        match locks.job.get::<String, _>("state").as_str() {
            "sending" => {}
            "posted" => {
                // Re-delivering the exact same receipt is idempotent; a different receipt conflicts.
                let same = locks.job.get::<Option<String>, _>("team").as_deref() == Some(team)
                    && locks.job.get::<Option<String>, _>("dm").as_deref() == Some(dm)
                    && locks.job.get::<Option<String>, _>("ts").as_deref() == Some(ts);
                return if same {
                    Ok(())
                } else {
                    Err(Error::Conflict.into())
                };
            }
            _ => return Err(Error::Conflict.into()),
        }
        // HTTP-origin cards must land in the exact Slack team pinned at admission; team drift closes as `route_changed`.
        let now = now_ms(&mut tx).await?;
        if locks
            .job
            .get::<Option<String>, _>("origin_binding")
            .is_some()
            && locks.job.get::<Option<String>, _>("origin_team").as_deref() != Some(team)
        {
            self.close_card(
                tx,
                locks,
                &key.nonce,
                Closing::Blocked(ApprovalBlock::RouteChanged),
                now,
            )
            .await?;
            return Err(Error::Stale.into());
        }
        let set = locks.active.as_deref().ok_or(Error::Stale)?;
        let same_set = locks.job.get::<Option<String>, _>("activation").as_deref() == Some(set);
        if !(can_open(&locks, now) && same_set) {
            // Card went stale after posting; leave it closed without a receipt so clicks are rejected.
            sqlx::query(
                "update bot_card_jobs set state = 'closed', closed_reason = 'stale', updated_ms = bot_now_ms() where nonce = $1",
            )
            .bind(&key.nonce.0)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(Error::Stale.into());
        }
        sqlx::query(
            "update bot_card_jobs
                set state = 'posted', team = $2, dm = $3, ts = $4, posted_ms = bot_now_ms(), updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(team)
        .bind(dm)
        .bind(ts)
        .execute(&mut *tx)
        .await?;
        let notice = replies::answer(&JsonPayload(
            serde_json::json!({ "text": DM_SENT_NOTICE }).to_string(),
        ))?;
        push_reply(&mut tx, &locks.run.row.profile, &locks.run.row.id, &notice).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Returns an unsent job to `staged` with backoff, or closes it as unavailable once `MAX_ATTEMPTS` is reached.
    async fn card_retry(&self, key: &CardKey, retry_after_ms: Option<i64>) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let locks = lock_attempt(&mut tx, key).await?;
        match locks.job.get::<String, _>("state").as_str() {
            "staged" => return Ok(()),
            "sending" | "leased" => {}
            _ => return Err(Error::Conflict.into()),
        }
        let now = now_ms(&mut tx).await?;
        if lease_expired(&locks.job, now) {
            return Err(Error::Stale.into());
        }
        if locks.job.get::<i32, _>("attempts") >= MAX_ATTEMPTS {
            return self
                .close_card(tx, locks, &key.nonce, Closing::Unavailable, now)
                .await;
        }
        let wait = retry_after_ms.unwrap_or(BACKOFF_MS).clamp(0, 3_600_000);
        sqlx::query(
            "update bot_card_jobs
                set state = 'staged', not_before_ms = bot_now_ms() + $2, lease_until_ms = null, sending_ms = null,
                    activation = null, checked_revision = null, updated_ms = bot_now_ms()
              where nonce = $1",
        )
        .bind(&key.nonce.0)
        .bind(wait)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Admission-time origin metadata for HTTP-origin cards (`None` for channel-origin or missing jobs).
    pub(crate) async fn cards_origin(&self, nonce: &ApprovalId) -> R<Option<CardOrigin>> {
        let row = sqlx::query(
            "select origin_binding, requester, revision, origin_team from bot_card_jobs where nonce = $1 and origin_binding is not null",
        )
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| CardOrigin {
            binding: BindingId(row.get("origin_binding")),
            requester: PrincipalId(row.get("requester")),
            revision: Revision(row.get("revision")),
            team: row.get("origin_team"),
        }))
    }

    /// Confirmed DM delivery receipt, present only once the job reaches `posted`.
    pub(crate) async fn cards_receipt(&self, nonce: &ApprovalId) -> R<Option<CardReceipt>> {
        let row = sqlx::query(
            "select j.team, j.dm, j.ts, j.owner, j.profile, j.binding,
                    j.origin_binding, r.conv_kind, r.conv_id, r.thread, r.reply_transport,
                    (select q.slack_key from bot_requests q
                      where q.profile = r.profile and q.run = r.root
                      order by q.created_ms asc limit 1) as root_slack_key
               from bot_card_jobs j
               join bot_runs r on r.profile = j.profile and r.id = j.run
              where j.nonce = $1 and j.state = 'posted'",
        )
        .bind(&nonce.0)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| CardReceipt {
            team: row.get("team"),
            dm: row.get("dm"),
            ts: row.get("ts"),
            owner: PrincipalId(row.get("owner")),
            profile: ProfileId(row.get("profile")),
            binding: BindingId(row.get("binding")),
            source: card_source(&row),
        }))
    }
}

impl CardOutbox for Store {
    fn claim(&self, limit: u32) -> Reply<'_, Vec<CardJob>> {
        Box::pin(async move { self.cards_claim(limit).await.map_err(Fail::into_core) })
    }

    fn begin(&self, key: CardKey, checked_revision: Revision) -> Reply<'_, ()> {
        Box::pin(async move {
            self.cards_begin(&key, &checked_revision)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn settle(&self, key: CardKey, outcome: CardOutcome) -> Reply<'_, ()> {
        Box::pin(async move {
            self.cards_settle(&key, outcome)
                .await
                .map_err(Fail::into_core)
        })
    }

    fn origin(&self, nonce: ApprovalId) -> Reply<'_, Option<CardOrigin>> {
        Box::pin(async move { self.cards_origin(&nonce).await.map_err(Fail::into_core) })
    }

    fn block(&self, key: CardKey, why: ApprovalBlock) -> Reply<'_, ()> {
        Box::pin(async move {
            self.card_close(&key, Closing::Blocked(why))
                .await
                .map_err(Fail::into_core)
        })
    }
}
