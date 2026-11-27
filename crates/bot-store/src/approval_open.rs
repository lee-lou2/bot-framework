//! Opens approval cards (`approval_open`), parking leased runs and staging owner-DM jobs in `bot_card_jobs`.
//! Lock order: active set -> run -> call -> approval.

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, risk_str},
    codec::*,
    db::{R, Store, time_to_ms},
    finish::*,
    replies,
};
use bot_core::{ledger::OpenApproval, port::Error};

/// Returns `(principal, api_team)` if the run's root originated from an authenticated API request.
async fn api_origin(
    pool: &sqlx::PgPool,
    profile: &str,
    run: &str,
) -> R<Option<(String, Option<String>)>> {
    Ok(sqlx::query_as(
        "select q.principal, q.api_team from bot_runs r
         join bot_requests q on q.profile = r.profile and q.run = r.root and q.api_endpoint is not null
         where r.profile = $1 and r.id = $2",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(pool)
    .await?)
}

impl Store {
    pub(crate) async fn approval_open(&self, open: OpenApproval) -> R<()> {
        let a = &open.approval;
        ids_ok(&[
            &a.nonce.0,
            &a.call.0,
            &a.requester.0,
            &a.approver.0,
            &a.policy_revision.0,
            &a.tool.0,
            &a.tool_revision.0,
        ])?;
        if open.reasons.len() > 16 || open.reasons.iter().any(|r| !id_ok(r) || r.len() > 64) {
            return invalid();
        }
        let (_, card) = canonical(&open.card)?;
        // Explicit-destination approvals (delivery drafts and routed schedule creation) must serialize against active-set replacement.
        let delivery = a.tool.0 == bot_core::schedule::DELIVER_TOOL;
        let create_to = a.tool.0 == crate::user_schedule::CREATE_TOOL
            && crate::delivery::create_names_destination(&card)?;
        // Reject before writing if the schedule-creation card's displayed args digest diverges from `args_sha256`.
        if a.tool.0 == crate::user_schedule::CREATE_TOOL
            && crate::delivery::create_shown_args_sha256(&card)? != a.args_sha256
        {
            return Err(Error::Conflict.into());
        }
        // Routed schedule creation requires a pinned active-set handle to verify the route registry in this commit.
        if create_to && self.activation.is_none() {
            return Err(Error::Denied.into());
        }
        // API-origin approvals serialize under the active-set lock so route drift fails `Stale` before any card or job is persisted.
        let origin = api_origin(&self.pool, &a.attempt.profile.0, &a.attempt.run.0).await?;
        if origin.is_some() && self.activation.is_none() {
            return Err(Error::Denied.into());
        }
        // Only API-origin requests with an admission-verified `api_team` may request Slack DM delivery.
        let slack = match (&open.delivery, &origin) {
            (None, _) => None,
            (Some(binding), Some((_, Some(team)))) => Some((binding, team.clone())),
            (Some(_), _) => return Err(Error::Denied.into()),
        };
        let routed = delivery || create_to;
        let mut tx = if routed || origin.is_some() {
            self.begin_authorized().await?
        } else {
            self.pool.begin().await?
        };
        if routed && self.activation.is_none() {
            // Unpinned handles (delivery drafts only) still lock `bot_active FOR SHARE` first to preserve lock order.
            sqlx::query("select 1 from bot_active where singleton for share")
                .fetch_optional(&mut *tx)
                .await?;
        }
        let run = fence(&mut tx, &a.attempt).await?;
        if run.private {
            // Never expose sensitive private-DM inputs on approval cards.
            return Err(Error::Denied.into());
        }
        let (kind, binding, revision, transport): (String, String, String, Option<String>) =
            sqlx::query_as(
                "select conv_kind, binding, revision, reply_transport from bot_runs where profile = $1 and id = $2",
            )
            .bind(&run.profile)
            .bind(&run.id)
            .fetch_one(&mut *tx)
            .await?;
        let is_slack =
            transport.as_deref() == Some(bot_core::intake::PushTransportId::SLACK.as_str());
        let channel_dm = slack.is_none() && kind == "channel" && (routed || is_slack);
        // Channel-origin owner-DM cards require Slack transport and requester == approver.
        if channel_dm && (!is_slack || a.requester != a.approver) {
            return Err(Error::Denied.into());
        }
        // API-origin requester must match the admission-verified root principal.
        if let Some((principal, _)) = &origin
            && *principal != a.requester.0
        {
            return Err(Error::Denied.into());
        }
        let call = lock_call(&mut tx, &a.attempt, &a.call).await?;
        if delivery {
            crate::delivery::check_open(&mut tx, a, &card).await?;
        }
        let recorded = crate::calls::record_of(&call)?;
        // Card, approval, and begun call must agree on the same args digest.
        if a.tool.0 == crate::user_schedule::CREATE_TOOL && recorded.args_sha256 != a.args_sha256 {
            return Err(Error::Conflict.into());
        }
        let previous = match recorded.phase {
            bot_core::ledger::CallPhase::Open { approval } => approval,
            _ => return Err(Error::Conflict.into()),
        };
        let expires = time_to_ms(a.expires_at);
        let now: i64 = sqlx::query_scalar("select bot_now_ms()")
            .fetch_one(&mut *tx)
            .await?;
        if expires <= now {
            return invalid();
        }
        let mut audits = Vec::new();
        if let Some(old) = previous {
            let closed = sqlx::query(
                "update bot_approvals set state = 'superseded' where nonce = $1 and state in ('pending', 'approved')",
            )
            .bind(&old.0)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if closed == 1 {
                audits.push(OwnedEntry::new(
                    &run.profile,
                    &a.requester.0,
                    "approval.supersede",
                    &old.0,
                    "superseded",
                ));
            }
        }
        let inserted = sqlx::query(
            "insert into bot_approvals (nonce, profile, run, generation, call, requester, approver, risk, policy_revision,
                                        tool, tool_revision, args_sha256, expires_ms, reasons, card)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)",
        )
        .bind(&a.nonce.0)
        .bind(&run.profile)
        .bind(&run.id)
        .bind(a.attempt.generation.0 as i64)
        .bind(&a.call.0)
        .bind(&a.requester.0)
        .bind(&a.approver.0)
        .bind(risk_str(a.risk))
        .bind(&a.policy_revision.0)
        .bind(&a.tool.0)
        .bind(&a.tool_revision.0)
        .bind(a.args_sha256.as_slice())
        .bind(expires)
        .bind(&open.reasons)
        .bind(&card)
        .execute(&mut *tx)
        .await;
        if let Err(e) = inserted {
            let fail = crate::db::Fail::from(e);
            return Err(if fail.is_unique_violation() {
                Error::Conflict.into()
            } else {
                fail
            });
        }
        sqlx::query("update bot_calls set approval = $4, risk = $5, tool_revision = $6, updated_ms = bot_now_ms() where profile = $1 and run = $2 and call = $3")
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&a.call.0)
            .bind(&a.nonce.0)
            .bind(risk_str(a.risk))
            .bind(&a.tool_revision.0)
            .execute(&mut *tx)
            .await?;
        if let Some((delivery, team)) = slack {
            // API-origin Slack DM card job: pin origin binding, requester, revision, and verified team; stays closed until receipt is confirmed.
            ids_ok(&[&delivery.0])?;
            sqlx::query(
                "insert into bot_card_jobs (nonce, profile, run, binding, owner, generation, origin_binding, requester, revision, origin_team)
                 values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            )
            .bind(&a.nonce.0)
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&delivery.0)
            .bind(&a.approver.0)
            .bind(a.attempt.generation.0 as i64)
            .bind(&binding)
            .bind(&a.requester.0)
            .bind(&revision)
            .bind(team)
            .execute(&mut *tx)
            .await?;
        } else if channel_dm {
            // Slack channel-origin approval cards go via 1:1 owner DM only and stay closed until DM receipt is confirmed.
            sqlx::query(
                "insert into bot_card_jobs (nonce, profile, run, binding, owner, generation) values ($1, $2, $3, $4, $5, $6)",
            )
            .bind(&a.nonce.0)
            .bind(&run.profile)
            .bind(&run.id)
            .bind(&binding)
            .bind(&a.approver.0)
            .bind(a.attempt.generation.0 as i64)
            .execute(&mut *tx)
            .await?;
        } else if matches!(kind.as_str(), "direct" | "channel") {
            let card = replies::card(&open.card, &a.nonce.0)?;
            push_reply_into(&mut tx, &run.profile, &run.id, &card, &mut audits).await?;
        }
        park(&mut tx, &run, &Wait::Approval).await?;
        let mut entry = OwnedEntry::new(
            &run.profile,
            &a.requester.0,
            "approval.open",
            &a.nonce.0,
            risk_str(a.risk),
        );
        entry.args = Some(a.args_sha256);
        audits.push(entry);
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}
