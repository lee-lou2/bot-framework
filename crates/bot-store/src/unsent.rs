//! Unsent closure for frozen scheduled-delivery drafts (`Unrouted` or owner `Rejected`), clearing draft bodies and settling runs atomically.
//! Lock order: active set -> run -> call -> approval -> draft -> audit head.

use crate::{
    audit::{self, OwnedEntry},
    codec::{kind_from, to32},
    db::{R, Store, Tx},
    delivery::peek,
    finish::{End, RunRow, end_run_into, lock_leased, parent_of, push_reply_into},
    replies,
};
use bot_core::{
    command::JsonPayload,
    id::*,
    port::Error,
    schedule::{
        DELIVER_CALL, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UNROUTED_NOTICE, UnsentReason,
    },
    state::Conversation,
};
use sqlx::{Row, postgres::PgRow};

fn target_of(row: &PgRow) -> R<DeliveryTarget> {
    Ok(DeliveryTarget {
        binding: BindingId(row.get("dest_binding")),
        conversation: Conversation {
            kind: kind_from(&row.get::<String, _>("dest_kind"))?,
            id: row.get("dest_id"),
            thread: row.get("dest_thread"),
        },
    })
}

/// DB column value for `bot_deliveries.unsent` (distinct from receipt `delivery_*` codes).
fn word(reason: UnsentReason) -> &'static str {
    match reason {
        UnsentReason::Rejected => "rejected",
        UnsentReason::Unrouted => "unrouted",
    }
}

fn same_target(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

pub(crate) struct Locked {
    pub(crate) row: RunRow,
    pub(crate) state: String,
    pub(crate) generation: i64,
    claim_epoch: i64,
    lease_live: bool,
    owner: String,
}

impl Locked {
    fn same_claim(&self, at: &Attempt) -> bool {
        self.generation == at.generation.0 as i64
            && at.claim.is_some_and(|c| self.claim_epoch == c.0 as i64)
    }
}

pub(crate) async fn lock_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Locked> {
    let row = sqlx::query(
        "select profile, id, parent, parent_step, attempts, cancel_requested, private, state, generation, claim_epoch,
                lease_until_ms, principal
         from bot_runs where profile = $1 and id = $2 for update",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::Missing)?;
    // Read DB clock only after acquiring the row lock.
    let now: i64 = sqlx::query_scalar("select bot_now_ms()")
        .fetch_one(&mut **tx)
        .await?;
    let lease: Option<i64> = row.get("lease_until_ms");
    Ok(Locked {
        state: row.get("state"),
        generation: row.get("generation"),
        claim_epoch: row.get("claim_epoch"),
        lease_live: lease.is_some_and(|until| until >= now),
        owner: row.get("principal"),
        row: RunRow {
            profile: row.get("profile"),
            id: row.get("id"),
            parent: row.get("parent"),
            parent_step: row.get("parent_step"),
            attempts: row.get("attempts"),
            cancel_requested: row.get("cancel_requested"),
            private: row.get("private"),
        },
    })
}

/// Drops a locked draft body (`result`/`answers` cleared) and closes an open `DELIVER_CALL` with `receipt`.
pub(crate) async fn drop_draft(
    tx: &mut Tx<'_>,
    run: (&str, &str),
    call_open: bool,
    ended: (&str, &str, &str),
    nonce: Option<&str>,
) -> R<()> {
    let ((profile, run_id), (word, receipt, current)) = (run, ended);
    if call_open {
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, finished_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(profile)
        .bind(run_id)
        .bind(DELIVER_CALL)
        .bind(receipt)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "update bot_deliveries
         set state = 'dropped', unsent = $3, unsent_receipt = $4, unsent_activation = $5, unsent_nonce = $6,
             unsent_ms = bot_now_ms(), result = null, answers = null, updated_ms = bot_now_ms()
         where profile = $1 and run = $2",
    )
    .bind(profile)
    .bind(run_id)
    .bind(word)
    .bind(receipt)
    .bind(current)
    .bind(nonce)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Reads only the pinned destination of a drafted delivery (even across active-set changes) without exposing draft content.
    pub(crate) async fn delivery_target(&self, at: &Attempt) -> R<Option<DeliveryTarget>> {
        let mut tx = self.begin_authorized().await?;
        if parent_of(&mut tx, at).await?.is_some() {
            tx.rollback().await?;
            return Ok(None);
        }
        lock_leased(&mut tx, at).await?;
        let row = peek(&mut tx, &at.profile.0, &at.run.0).await?;
        tx.rollback().await?;
        match row {
            Some(row) if row.get::<String, _>("state") == "drafted" => Ok(Some(target_of(&row)?)),
            _ => Ok(None),
        }
    }

    /// Returns the committed queued receipt, or else the unsent receipt for the final claim of a succeeded run.
    pub(crate) async fn delivery_any_receipt(&self, at: &Attempt) -> R<Option<JsonPayload>> {
        if let Some(receipt) = self.delivery_receipt(at).await? {
            return Ok(Some(receipt));
        }
        let row = sqlx::query(
            "select d.unsent_receipt, d.unsent_activation, r.generation, r.claim_epoch,
                    (select revision from bot_active where singleton) as current
             from bot_deliveries d
             join bot_runs r on r.profile = d.profile and r.id = d.run
             where d.profile = $1 and d.run = $2 and d.state = 'dropped' and d.unsent is not null and r.state = 'succeeded'",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let current: Option<String> = row.get("current");
        let ended_in: Option<String> = row.get("unsent_activation");
        let same_claim = row.get::<i64, _>("generation") == at.generation.0 as i64
            && at
                .claim
                .is_some_and(|c| row.get::<i64, _>("claim_epoch") == c.0 as i64);
        let pinned = self
            .pinned_revision()
            .is_none_or(|expected| current.as_deref() == Some(expected.0.as_str()));
        if !same_claim || !pinned || current.is_none() || current != ended_in {
            return Err(Error::Stale.into());
        }
        Ok(row
            .get::<Option<String>, _>("unsent_receipt")
            .map(JsonPayload))
    }

    pub(crate) async fn delivery_finish_unsent(
        &self,
        fence: DeliveryFence,
        target: DeliveryTarget,
        reason: UnsentReason,
    ) -> R<JsonPayload> {
        let mut tx = self.begin_authorized().await?;
        // Lock order: active set -> run -> call -> approval -> draft -> audit head.
        let current_set: Option<String> = sqlx::query_scalar::<_, String>(
            "select revision from bot_active where singleton for share",
        )
        .fetch_optional(&mut *tx)
        .await?
        .filter(|r| !r.is_empty());
        let (profile, run_id) = match &fence {
            DeliveryFence::Claim(at) => (at.profile.0.clone(), at.run.0.clone()),
            DeliveryFence::Approval { nonce, .. } => {
                let row = sqlx::query("select profile, run from bot_approvals where nonce = $1")
                    .bind(&nonce.0)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(Error::Missing)?;
                (row.get("profile"), row.get("run"))
            }
        };
        let run = lock_run(&mut tx, &profile, &run_id).await?;
        if run.row.parent.is_some() {
            return Err(Error::Conflict.into());
        }
        if run.row.private {
            return Err(Error::Denied.into());
        }
        let call_state: Option<String> = sqlx::query_scalar(
            "select state from bot_calls where profile = $1 and run = $2 and call = $3 and tool = $4 for update",
        )
        .bind(&profile)
        .bind(&run_id)
        .bind(DELIVER_CALL)
        .bind(DELIVER_TOOL)
        .fetch_optional(&mut *tx)
        .await?;
        let card = match &fence {
            DeliveryFence::Approval { nonce, actor } => {
                let card = sqlx::query(
                    "select profile, run, call, approver, state, expires_ms, bot_now_ms() as now
                     from bot_approvals where nonce = $1 for update",
                )
                .bind(&nonce.0)
                .fetch_one(&mut *tx)
                .await?;
                if card.get::<String, _>("call") != DELIVER_CALL
                    || card.get::<String, _>("approver") != actor.0
                    || actor.0 != run.owner
                {
                    return Err(Error::Denied.into());
                }
                Some((nonce.0.clone(), card))
            }
            DeliveryFence::Claim(_) => None,
        };
        let delivery = sqlx::query(&format!(
            "select {} from bot_deliveries where profile = $1 and run = $2 for update",
            crate::cols::DELIVERIES
        ))
        .bind(&profile)
        .bind(&run_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if !same_target(&target_of(&delivery)?, &target) {
            return Err(Error::Conflict.into());
        }
        // Owner denial/revocation takes precedence over route drift; serialized via the run lock against `approval_decide`.
        let decided = sqlx::query_scalar::<_, i32>(
            "select 1 from bot_approvals
             where profile = $1 and run = $2 and call = $3 and state in ('denied', 'revoked') and decided_by = $4 limit 1",
        )
        .bind(&profile)
        .bind(&run_id)
        .bind(DELIVER_CALL)
        .bind(&run.owner)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        let reason = if decided {
            UnsentReason::Rejected
        } else {
            reason
        };
        let state: String = delivery.get("state");
        let ended: Option<String> = delivery.get("unsent");
        if state == "dropped" && ended.as_deref() == Some(word(reason)) {
            // Replay of the same closure: return the stored receipt only to the matching fence.
            let fenced = match &fence {
                DeliveryFence::Claim(at) => run.state == "succeeded" && run.same_claim(at),
                DeliveryFence::Approval { nonce, .. } => {
                    delivery.get::<Option<String>, _>("unsent_nonce").as_deref() == Some(&nonce.0)
                }
            };
            if !fenced {
                return Err(Error::Stale.into());
            }
            let receipt: String = delivery.get("unsent_receipt");
            tx.rollback().await?;
            return Ok(JsonPayload(receipt));
        }
        if state != "drafted" {
            return Err(Error::Conflict.into());
        }
        let nonce = match (&fence, &card) {
            (DeliveryFence::Claim(at), _) => {
                if run.state != "leased" || !run.lease_live || !run.same_claim(at) {
                    return Err(Error::Stale.into());
                }
                None
            }
            (DeliveryFence::Approval { .. }, Some((nonce, card))) => {
                if card.get::<String, _>("state") != "pending"
                    || card.get::<i64, _>("expires_ms") <= card.get::<i64, _>("now")
                    || run.state != "waiting"
                {
                    return Err(Error::Stale.into());
                }
                Some(nonce.clone())
            }
            _ => return Err(Error::Denied.into()),
        };
        if reason == UnsentReason::Rejected && !decided {
            // `Rejected` requires a persisted denial or revocation by the owner.
            return Err(Error::Denied.into());
        }
        let current = current_set.ok_or(Error::Stale)?;
        let sealed = to32(
            delivery
                .get::<Option<Vec<u8>>, _>("sha256")
                .ok_or(Error::Invalid)?,
        )?;
        let receipt = reason.receipt().0;

        let mut audits = Vec::new();
        if reason == UnsentReason::Unrouted {
            // Post one fixed notice to the originating conversation without leaking draft or recipient details.
            let notice = replies::answer(&JsonPayload(
                serde_json::json!({ "text": UNROUTED_NOTICE }).to_string(),
            ))?;
            push_reply_into(&mut tx, &profile, &run_id, &notice, &mut audits).await?;
        }
        drop_draft(
            &mut tx,
            (&profile, &run_id),
            call_state.as_deref() == Some("open"),
            (word(reason), &receipt, &current),
            nonce.as_deref(),
        )
        .await?;
        let actor = if card.is_some() || reason == UnsentReason::Rejected {
            run.owner.clone()
        } else {
            "system".to_owned()
        };
        end_run_into(
            &mut tx,
            &run.row,
            End::Succeeded(&receipt),
            &actor,
            &mut audits,
        )
        .await?;
        let mut entry =
            OwnedEntry::new(&profile, &actor, "delivery.unsent", &run_id, reason.code());
        entry.args = Some(sealed);
        audits.push(entry);
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(JsonPayload(receipt))
    }
}
