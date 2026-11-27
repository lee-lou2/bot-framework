//! Admin approval listings, detail lookups, and card rendering with owner-only body redaction.

use crate::db::{R, Store, ms_to_time};
use bot_core::builtin::{CardBody as Body, card_body};
use bot_core::{admin::*, id::*, port::Error};
use sqlx::{Row, postgres::PgRow};

/// `<ms>:<id>` keyset cursor.
fn cursor(after: &Option<String>) -> (Option<i64>, Option<String>) {
    match after.as_deref().and_then(|a| a.split_once(':')) {
        Some((ms, id)) => (ms.parse().ok(), Some(id.to_owned())),
        None => (None, None),
    }
}

/// `<decided_ms_or_-1>:<is_pending_0_or_1>:<expires_ms_or_0>:<nonce>` cursor for decided/all listings.
fn processed_cursor(after: &Option<String>) -> Option<(i64, i32, i64, String)> {
    let mut parts = after.as_deref()?.splitn(4, ':');
    let decided = parts.next()?.parse().ok()?;
    let pending = parts.next()?.parse().ok()?;
    let expires = parts.next()?.parse().ok()?;
    Some((decided, pending, expires, parts.next()?.to_owned()))
}

/// Parses broker-rendered card lines into `(label, value)` rows with the title first.
/// Joins wrapped `Body` / `Delivery draft` continuations into one full row so callers can verify
/// the visible text against `args_sha256`; omits the row if unparseable.
fn render_of(card: &str, body: Body) -> Vec<(String, String)> {
    let label = match body {
        Body::None => None,
        Body::Share => Some(bot_core::cards::LABEL_BODY),
        Body::Draft => Some(bot_core::schedule::DELIVERY_CARD_LABEL),
    };
    let (mut out, joined) = bot_core::cards::split_render(card, label);
    if let Some(joined) = joined {
        match body {
            Body::Share => {
                if let Ok(text) = serde_json::from_str::<String>(&joined) {
                    out.push((bot_core::cards::LABEL_BODY.to_owned(), text));
                }
            }
            Body::Draft => out.push((bot_core::schedule::DELIVERY_CARD_LABEL.to_owned(), joined)),
            Body::None => {}
        }
    }
    out
}

/// Redacts card contents for memory-share, memory-save, and scheduled-delivery approvals unless the viewer is both requester and approver.
fn render_for(
    viewer: &Viewer,
    approval: &bot_core::state::Approval,
    card: &str,
) -> Vec<(String, String)> {
    let owner = approval.requester == viewer.principal && approval.approver == viewer.principal;
    let body = card_body(&approval.tool.0);
    if (body != Body::None || approval.tool.0 == bot_core::builtin::MEMORY_SAVE_TOOL) && !owner {
        return vec![];
    }
    render_of(card, body)
}

pub(crate) fn approval_view(row: &PgRow, viewer: &Viewer) -> R<ApprovalView> {
    let approval = crate::approvals::approval_of(row)?;
    let render = render_for(viewer, &approval, &row.get::<String, _>("card"));
    Ok(ApprovalView {
        nonce: approval.nonce,
        profile: approval.attempt.profile,
        run: approval.attempt.run,
        generation: approval.attempt.generation,
        call: approval.call,
        requester: approval.requester,
        approver: approval.approver,
        risk: approval.risk,
        tool: approval.tool,
        tool_revision: approval.tool_revision,
        policy_revision: approval.policy_revision,
        args_sha256: approval.args_sha256,
        expires_at: approval.expires_at,
        state: crate::approvals::state_of(&row.get::<String, _>("state"))?,
        render,
        decided_by: row.get::<Option<String>, _>("decided_by").map(PrincipalId),
        decided_at: row.get::<Option<i64>, _>("decided_ms").map(ms_to_time),
    })
}

impl Store {
    /// Lists approvals filtered before `LIMIT`, excluding unposted owner-DM cards from pending results.
    /// Order: `Pending` by `(expires_ms ASC, nonce ASC)`; `Decided` by `(decided_ms DESC, nonce ASC)`;
    /// `All` lists decided rows first, then pending rows by soonest expiry.
    pub(crate) async fn admin_approvals(
        &self,
        viewer: &Viewer,
        q: ApprovalQuery,
    ) -> R<Listing<ApprovalView>> {
        let limit = i64::from(q.page.limit.clamp(1, 200));
        let profile = q.profile.as_ref().map(|p| p.0.as_str());
        let rows = if q.filter == ApprovalFilter::Pending {
            let (after_ms, after_id) = cursor(&q.page.after);
            sqlx::query(&format!(
                "select {} from bot_approvals a
                 where ($1::text is null or a.profile = $1) and a.state = 'pending'
                   and not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted')
                   and ($2 or a.requester = $3 or a.approver = $3)
                   and ($4::bigint is null or a.expires_ms > $4
                        or (a.expires_ms = $4 and a.nonce collate \"C\" > $5 collate \"C\"))
                 order by a.expires_ms asc, a.nonce collate \"C\" asc limit $6",
                crate::cols::APPROVALS
            ))
            .bind(profile)
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .bind(after_ms)
            .bind(after_id)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        } else {
            let after = processed_cursor(&q.page.after);
            sqlx::query(
                &format!("select {} from bot_approvals a
                 where ($1::text is null or a.profile = $1)
                   and ($2::text <> 'decided' or a.state <> 'pending')
                   and (a.state <> 'pending'
                        or not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted'))
                   and ($3 or a.requester = $4 or a.approver = $4)
                   and ($5::bigint is null
                        or coalesce(a.decided_ms, -1) < $5
                        or (coalesce(a.decided_ms, -1) = $5 and ((a.state = 'pending')::int > $6
                            or ((a.state = 'pending')::int = $6 and (case when a.state = 'pending' then a.expires_ms else 0 end > $7
                                or (case when a.state = 'pending' then a.expires_ms else 0 end = $7
                                    and a.nonce collate \"C\" > $8 collate \"C\"))))))
                 order by coalesce(a.decided_ms, -1) desc, (a.state = 'pending')::int asc,
                          case when a.state = 'pending' then a.expires_ms else 0 end asc, a.nonce collate \"C\" asc
                 limit $9", crate::cols::prefixed(crate::cols::APPROVALS, "a")),
            )
            .bind(profile)
            .bind(if q.filter == ApprovalFilter::Decided { "decided" } else { "all" })
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .bind(after.as_ref().map(|c| c.0))
            .bind(after.as_ref().map_or(0, |c| c.1))
            .bind(after.as_ref().map_or(0, |c| c.2))
            .bind(after.as_ref().map(|c| c.3.clone()).unwrap_or_default())
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        };
        let more = rows.len() as i64 > limit;
        let kept: Vec<&PgRow> = rows.iter().take(limit as usize).collect();
        let items = kept
            .iter()
            .map(|r| approval_view(r, viewer))
            .collect::<R<Vec<_>>>()?;
        let next = more
            .then(|| {
                kept.last().map(|r| {
                    if q.filter == ApprovalFilter::Pending {
                        format!(
                            "{}:{}",
                            r.get::<i64, _>("expires_ms"),
                            r.get::<String, _>("nonce")
                        )
                    } else {
                        let decided = r.get::<Option<i64>, _>("decided_ms").unwrap_or(-1);
                        let pending = r.get::<String, _>("state") == "pending";
                        let expires = if pending {
                            r.get::<i64, _>("expires_ms")
                        } else {
                            0
                        };
                        format!(
                            "{decided}:{}:{expires}:{}",
                            i32::from(pending),
                            r.get::<String, _>("nonce")
                        )
                    }
                })
            })
            .flatten();
        Ok(Listing { items, next })
    }

    pub(crate) async fn admin_approval(
        &self,
        viewer: &Viewer,
        nonce: &ApprovalId,
    ) -> R<ApprovalView> {
        let row = sqlx::query(&format!("select {} from bot_approvals a where a.nonce = $1 and ($2 or a.requester = $3 or a.approver = $3)", crate::cols::APPROVALS))
            .bind(&nonce.0)
            .bind(viewer.operator)
            .bind(&viewer.principal.0)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(Error::Missing)?;
        approval_view(&row, viewer)
    }
}
