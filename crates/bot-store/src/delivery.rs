//! Explicit-destination scheduled reply drafts (`draft`) and owner-approved outbox release (`enqueue`).
//! Raw answer text is stored un-rewritten so `begin`, `approval_open`, and `enqueue` recompute the exact approval document and SHA-256.
//! Lock order: active revision -> root run (FOR SHARE) -> run -> call -> approval -> draft -> audit head.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::{End, end_run_into, final_text, has_attachments, lock_leased, parent_of},
    replies,
    user_schedule::{Claimed, claim, done_entry, finish_call},
};
use bot_core::{
    command::{Finish, JsonPayload},
    id::*,
    intake::PushTransportId,
    ledger::StartCall,
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL, DeliveryDraft, DeliveryTarget, quiet_completion},
    state::{Conversation, ConversationKind},
};
use sqlx::{Row, postgres::PgRow};

const MAX_ANSWERS: usize = 16;

pub(crate) fn draft_of(row: &PgRow) -> R<DeliveryDraft> {
    Ok(DeliveryDraft {
        activation: Revision(
            row.get::<Option<String>, _>("activation")
                .ok_or(Error::Invalid)?,
        ),
        target: DeliveryTarget {
            binding: BindingId(row.get("dest_binding")),
            conversation: Conversation {
                kind: kind_from(&row.get::<String, _>("dest_kind"))?,
                id: row.get("dest_id"),
                thread: row.get("dest_thread"),
            },
        },
        answers: row
            .get::<Option<Vec<String>>, _>("answers")
            .ok_or(Error::Invalid)?
            .into_iter()
            .map(JsonPayload)
            .collect(),
    })
}

/// Recomputes the approval digest from stored draft bytes; returns `Conflict` on mismatch.
fn recomputed(row: &PgRow) -> R<[u8; 32]> {
    let hash = draft_of(row)?.sha256()?;
    if row.get::<Option<Vec<u8>>, _>("sha256").as_deref() != Some(hash.as_slice()) {
        return Err(Error::Conflict.into());
    }
    Ok(hash)
}

pub(crate) async fn peek(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Option<PgRow>> {
    Ok(sqlx::query(&format!(
        "select {} from bot_deliveries where profile = $1 and run = $2",
        crate::cols::DELIVERIES
    ))
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?)
}

async fn lock(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Option<PgRow>> {
    Ok(sqlx::query(&format!(
        "select {} from bot_deliveries where profile = $1 and run = $2 for update",
        crate::cols::DELIVERIES
    ))
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Locks `bot_active FOR SHARE` first (lock order: active set -> run) and returns the current set revision.
async fn lock_activation(tx: &mut Tx<'_>) -> R<Option<String>> {
    let current: Option<String> =
        sqlx::query_scalar("select revision from bot_active where singleton for share")
            .fetch_optional(&mut **tx)
            .await?;
    Ok(current.filter(|r| !r.is_empty()))
}

async fn current_activation(tx: &mut Tx<'_>) -> R<String> {
    lock_activation(tx).await?.ok_or(Error::Stale.into())
}

/// Verifies a frozen draft exists and matches `args` before opening `DELIVER_TOOL`.
pub(crate) async fn check_begin(
    tx: &mut Tx<'_>,
    profile: &str,
    run: &str,
    args: &[u8; 32],
) -> R<()> {
    let Some(row) = peek(tx, profile, run).await? else {
        return Err(Error::Conflict.into());
    };
    if row.get::<String, _>("state") != "drafted" || recomputed(&row)?.as_slice() != args.as_slice()
    {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

/// Verifies the delivery card displays the exact frozen draft, matches the digest, names the owner, and still belongs to the active set.
pub(crate) async fn check_open(
    tx: &mut Tx<'_>,
    approval: &bot_core::state::Approval,
    card: &str,
) -> R<()> {
    let at = &approval.attempt;
    if approval.call.0 != DELIVER_CALL
        || approval.risk != bot_core::state::Risk::Irreversible
        || approval.requester != approval.approver
    {
        return Err(Error::Denied.into());
    }
    let owner: String =
        sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
            .bind(&at.profile.0)
            .bind(&at.run.0)
            .fetch_one(&mut **tx)
            .await?;
    if owner != approval.approver.0 {
        return Err(Error::Denied.into());
    }
    let row = peek(tx, &at.profile.0, &at.run.0)
        .await?
        .ok_or(Error::Conflict)?;
    if row.get::<String, _>("state") != "drafted"
        || recomputed(&row)? != approval.args_sha256
        || shown_draft(card).as_deref() != Some(draft_of(&row)?.approval_json()?.as_str())
    {
        return Err(Error::Conflict.into());
    }
    // Reject opening a card whose draft was frozen under a superseded active set.
    if current_activation(tx).await? != draft_of(&row)?.activation.0 {
        return Err(Error::Conflict.into());
    }
    Ok(())
}

pub(crate) fn shown_draft(card: &str) -> Option<String> {
    shown_row(card, bot_core::schedule::DELIVERY_CARD_LABEL)
}

/// Returns whether a schedule-creation card names an explicit `destination` key in its `LABEL_ARGS` JSON object.
/// Rejects missing, duplicate, or non-object `LABEL_ARGS` lines with `Invalid` rather than defaulting to unrouted.
pub(crate) fn create_names_destination(card: &str) -> R<bool> {
    let args = shown_row(card, bot_core::cards::LABEL_ARGS).ok_or(Error::Invalid)?;
    let args: serde_json::Value = serde_json::from_str(&args).map_err(|_| Error::Invalid)?;
    Ok(args
        .as_object()
        .ok_or(Error::Invalid)?
        .contains_key("destination"))
}

/// Computes the SHA-256 of the card's `LABEL_ARGS` raw text to verify displayed args match the sealed digest.
pub(crate) fn create_shown_args_sha256(card: &str) -> R<[u8; 32]> {
    let shown = shown_row(card, bot_core::cards::LABEL_ARGS).ok_or(Error::Invalid)?;
    Ok(crate::codec::sha256_text(&shown))
}

fn shown_row(card: &str, label: &str) -> Option<String> {
    bot_core::cards::extract_line(card, label)
}

/// Enqueues one reply to the explicit destination; pinned to `PushTransportId::SLACK` while explicit routes are Slack-only.
async fn push_to(tx: &mut Tx<'_>, row: &PgRow, principal: &str, body: &str) -> R<()> {
    let dest_kind: String = row.get("dest_kind");
    match kind_from(&dest_kind)? {
        ConversationKind::Direct | ConversationKind::Channel => {}
        ConversationKind::Cli | ConversationKind::Http | ConversationKind::Scheduled => {
            return invalid();
        }
    }
    sqlx::query(
        "insert into bot_outbox (profile, run, seq, call, binding, principal, conv_kind, conv_id, thread, body, transport)
         select $1, $2, n.seq, 'reply:' || $2 || ':' || n.seq::text, $3, $4, $5, $6, $7, $8, $9
         from lateral (select coalesce(max(o.seq) + 1, 0) as seq from bot_outbox o where o.profile = $1 and o.run = $2) n",
    )
    .bind(row.get::<String, _>("profile"))
    .bind(row.get::<String, _>("run"))
    .bind(row.get::<String, _>("dest_binding"))
    .bind(principal)
    .bind(&dest_kind)
    .bind(row.get::<String, _>("dest_id"))
    .bind(row.get::<Option<String>, _>("dest_thread"))
    .bind(body)
    .bind(PushTransportId::SLACK.as_str())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Freezes the final answers into a draft once, or reloads an existing draft when `completion` is `None`.
    pub(crate) async fn delivery_draft(
        &self,
        at: &Attempt,
        completion: Option<Finish>,
    ) -> R<Option<DeliveryDraft>> {
        let mut tx = self.begin_authorized().await?;
        // Lock order: active set -> run -> draft -> audit head.
        let current_set = lock_activation(&mut tx).await?;
        if parent_of(&mut tx, at).await?.is_some() {
            tx.rollback().await?;
            // Delegated children do not deliver directly.
            return Ok(None);
        }
        let run = lock_leased(&mut tx, at).await?;
        let Some(row) = lock(&mut tx, &at.profile.0, &at.run.0).await? else {
            tx.rollback().await?;
            return Ok(None);
        };
        match (row.get::<String, _>("state").as_str(), completion) {
            ("drafted", _) => {
                recomputed(&row)?;
                let draft = draft_of(&row)?;
                let current = current_set.ok_or(Error::Stale)?;
                if current != draft.activation.0 {
                    return Err(Error::Conflict.into());
                }
                tx.rollback().await?;
                Ok(Some(draft))
            }
            ("pending", Some(done)) => {
                let (_, result) = canonical(&done.result)?;
                if done.answers.len() > MAX_ANSWERS {
                    return invalid();
                }
                let normalized = done
                    .answers
                    .iter()
                    .map(replies::answer)
                    .collect::<R<Vec<_>>>()?;
                let quiet: bool =
                    sqlx::query_scalar("select quiet from bot_runs where profile = $1 and id = $2")
                        .bind(&run.profile)
                        .bind(&run.id)
                        .fetch_one(&mut *tx)
                        .await?;
                // Suppress delivery when a quiet schedule succeeds with `<quiet>` and no attachments.
                if quiet_completion(quiet, &final_text(&result))
                    && !normalized.iter().any(|a| has_attachments(a))
                {
                    sqlx::query(
                        "update bot_deliveries set state = 'dropped', updated_ms = bot_now_ms() where profile = $1 and run = $2",
                    )
                    .bind(&run.profile)
                    .bind(&run.id)
                    .execute(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    return Ok(None);
                }
                let active: Option<String> = sqlx::query_scalar(
                    "select revision from bot_active_profiles where profile = $1",
                )
                .bind(&run.profile)
                .fetch_optional(&mut *tx)
                .await?;
                if active.is_none() {
                    // Inactive profiles cannot freeze new drafts.
                    return Err(Error::Denied.into());
                }
                let activation = current_set.ok_or(Error::Stale)?;
                let draft = DeliveryDraft {
                    activation: Revision(activation.clone()),
                    target: DeliveryTarget {
                        binding: BindingId(row.get("dest_binding")),
                        conversation: Conversation {
                            kind: kind_from(&row.get::<String, _>("dest_kind"))?,
                            id: row.get("dest_id"),
                            thread: row.get("dest_thread"),
                        },
                    },
                    answers: done.answers,
                };
                let hash = draft.sha256()?;
                sqlx::query(
                    "update bot_deliveries set state = 'drafted', activation = $3, result = $4, answers = $5, sha256 = $6,
                            drafted_ms = bot_now_ms(), updated_ms = bot_now_ms()
                     where profile = $1 and run = $2",
                )
                .bind(&run.profile)
                .bind(&run.id)
                .bind(&activation)
                .bind(&result)
                .bind(draft.answers.iter().map(|a| a.0.clone()).collect::<Vec<_>>())
                .bind(hash.as_slice())
                .execute(&mut *tx)
                .await?;
                let mut entry = Entry::new("system", "delivery.draft", &run.id, "frozen");
                entry.args = Some(hash);
                audit::append(&mut tx, &run.profile, &entry).await?;
                tx.commit().await?;
                Ok(Some(draft))
            }
            _ => {
                tx.rollback().await?;
                Ok(None)
            }
        }
    }

    /// Returns the committed queued receipt for the final claim of a succeeded run without reading draft content.
    pub(crate) async fn delivery_receipt(&self, at: &Attempt) -> R<Option<JsonPayload>> {
        let row = sqlx::query(
            "select d.state, d.activation, r.state as run_state, r.generation, r.claim_epoch, c.state as call_state, c.result,
                    (select revision from bot_active where singleton) as current
             from bot_deliveries d
             join bot_runs r on r.profile = d.profile and r.id = d.run
             left join bot_calls c on c.profile = d.profile and c.run = d.run and c.call = $3 and c.tool = $4
             where d.profile = $1 and d.run = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(DELIVER_CALL)
        .bind(DELIVER_TOOL)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.get::<String, _>("state") != "enqueued"
            || row.get::<String, _>("run_state") != "succeeded"
        {
            return Ok(None);
        }
        let current: Option<String> = row.get("current");
        let registered: Option<String> = row.get("activation");
        let same_claim = row.get::<i64, _>("generation") == at.generation.0 as i64
            && at
                .claim
                .is_some_and(|c| row.get::<i64, _>("claim_epoch") == c.0 as i64);
        let pinned = self
            .pinned_revision()
            .is_none_or(|expected| current.as_deref() == Some(expected.0.as_str()));
        if !same_claim || !pinned || current.is_none() || current != registered {
            return Err(Error::Stale.into());
        }
        if row.get::<Option<String>, _>("call_state").as_deref() != Some("done") {
            return Ok(None);
        }
        Ok(row.get::<Option<String>, _>("result").map(JsonPayload))
    }

    /// Atomically consumes owner approval, enqueues destination outbox rows, completes the call, and finishes the run.
    /// On `Stale`, sweeps expired approvals so retries immediately observe DB-clock expiry.
    pub(crate) async fn delivery_enqueue(&self, s: StartCall) -> R<()> {
        match self.enqueue_once(s).await {
            Err(Fail::Core(Error::Stale)) => {
                let _ = self.expire_approvals().await;
                Err(Error::Stale.into())
            }
            other => other,
        }
    }

    async fn enqueue_once(&self, s: StartCall) -> R<()> {
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0])?;
        if s.call.0 != DELIVER_CALL {
            return invalid();
        }
        let at = &s.attempt;
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let seen = peek(&mut tx, &at.profile.0, &at.run.0)
            .await?
            .ok_or(Error::Missing)?;
        let sealed = recomputed(&seen)?;
        let (run, _, approval) = match claim(&mut tx, &s, DELIVER_TOOL, &sealed).await? {
            Claimed::Done(_) => {
                tx.rollback().await?;
                return Ok(());
            }
            Claimed::Fresh {
                run,
                shared_flag,
                approval,
            } => (run, shared_flag, approval),
        };
        // Explicit-destination delivery requires owner approval.
        let approval = approval.ok_or(Error::Denied)?;
        let row = lock(&mut tx, &at.profile.0, &at.run.0)
            .await?
            .ok_or(Error::Missing)?;
        if row.get::<String, _>("state") != "drafted" {
            return Err(Error::Conflict.into());
        }
        if recomputed(&row)? != sealed {
            return Err(Error::Conflict.into());
        }
        let draft = draft_of(&row)?;
        if current_activation(&mut tx).await? != draft.activation.0 {
            // Active set changed since draft freeze; destination mapping may have drifted.
            return Err(Error::Conflict.into());
        }
        let owner: String =
            sqlx::query_scalar("select principal from bot_runs where profile = $1 and id = $2")
                .bind(&at.profile.0)
                .bind(&at.run.0)
                .fetch_one(&mut *tx)
                .await?;
        let stored_approver: String =
            sqlx::query_scalar("select approver from bot_approvals where nonce = $1")
                .bind(&approval)
                .fetch_one(&mut *tx)
                .await?;
        if stored_approver != owner {
            // Approver must be the schedule owner.
            return Err(Error::Denied.into());
        }
        for answer in &draft.answers {
            push_to(&mut tx, &row, &owner, &replies::answer(answer)?).await?;
        }
        // Persist the receipt once as the call result so replays return identical bytes.
        let receipt = draft.queued_receipt().0;
        finish_call(&mut tx, &s, &receipt, Some(&approval)).await?;
        sqlx::query(
            "update bot_deliveries set state = 'enqueued', updated_ms = bot_now_ms() where profile = $1 and run = $2",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .execute(&mut *tx)
        .await?;
        let result: String = row
            .get::<Option<String>, _>("result")
            .ok_or(Error::Invalid)?;
        let mut audits = Vec::new();
        end_run_into(
            &mut tx,
            &run,
            End::Succeeded(&result),
            "system",
            &mut audits,
        )
        .await?;
        let mut entry = OwnedEntry::new(
            &at.profile.0,
            &owner,
            "delivery.enqueue",
            &at.run.0,
            "enqueued",
        );
        entry.args = Some(sealed);
        audits.push(entry);
        audits.push(done_entry(&at.profile.0, &owner, &s.call.0, sealed));
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}
