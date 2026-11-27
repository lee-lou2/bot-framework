//! Atomic accept-or-replay and scoped lookup for OpenAI-compatible HTTP runs (`bot_core::api`).
//!
//! Uses the shared intake tables (`bot_requests`, `bot_runs`, audit) in one transaction:
//! active-set fence -> replay check -> endpoint advisory lock -> active revision check -> active cap -> insert + audit.
//! Ambiguous commit failures return `ApiAdmission::Unknown`; certain pre-commit rollbacks return `Err(Unavailable)`.
//!
//! Lock order: active set -> endpoint advisory lock (held only by intake) -> active profile revision -> new run -> audit head.

use crate::{
    accept::{check, insert_run},
    audit::{self, Entry},
    codec::*,
    db::{Fail, R, Store, Tx},
    finish::final_text,
};
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiKey, ApiRun, ApiRuns, ApiState, ApprovalBlock},
    command::{Target, TokenUsage},
    id::*,
    identity::{IdentityKind, identity_kind},
    port::{Error, Reply},
    state::{ConversationKind, RunOrigin, RunState},
};
use sqlx::Row;

const UNFINISHED: &str = "('ready', 'leased', 'waiting', 'retry')";

/// Reads one API request and its root run for `(profile, api_endpoint, principal, request)`.
/// Block verdicts come solely from stored denial reasons on root/descendant calls and closed DM card jobs.
const VIEW: &str = "select r.id, r.created_ms, r.state, r.wait_kind, r.result, r.error, r.binding, r.agent, r.revision,
        r.truncated, r.usage_prompt, r.usage_completion, q.api_model, q.input_sha256,
        (select c.reason from bot_calls c join bot_runs t on t.profile = c.profile and t.id = c.run
          where t.profile = r.profile and t.root = r.id and c.state = 'denied'
            and c.reason in ('no_approver', 'approver_revoked', 'route_changed', 'approval_dm_unavailable')
          order by c.finished_ms nulls last, c.call limit 1) as call_block,
        (select j.closed_reason from bot_card_jobs j join bot_runs t on t.profile = j.profile and t.id = j.run
          where t.profile = r.profile and t.root = r.id and j.state = 'closed'
            and j.closed_reason in ('unavailable', 'held', 'no_approver', 'approver_revoked', 'route_changed')
          order by j.updated_ms, j.nonce limit 1) as job_block,
        exists (select 1 from bot_approvals a join bot_runs t on t.profile = a.profile and t.id = a.run
          where t.profile = r.profile and t.root = r.id and t.state = 'waiting' and t.wait_kind = 'approval'
            and a.state = 'pending' and a.generation = t.generation and a.expires_ms > bot_now_ms()
            and not exists (select 1 from bot_card_jobs j where j.nonce = a.nonce and j.state <> 'posted')) as needs_human
     from bot_requests q join bot_runs r on r.profile = q.profile and r.id = q.run
     where q.profile = $1 and q.api_endpoint = $2 and q.principal = $3 and q.request = $4";

fn blocked(row: &sqlx::postgres::PgRow) -> Option<ApprovalBlock> {
    row.get::<Option<String>, _>("call_block")
        .and_then(|code| ApprovalBlock::parse(&code))
        .or_else(|| {
            // DM job closures `unavailable`/`held` map to ApprovalDmUnavailable; other codes match ApprovalBlock directly.
            row.get::<Option<String>, _>("job_block")
                .and_then(|reason| match reason.as_str() {
                    "unavailable" | "held" => Some(ApprovalBlock::ApprovalDmUnavailable),
                    other => ApprovalBlock::parse(other),
                })
        })
}

fn view(row: &sqlx::postgres::PgRow) -> R<ApiRun> {
    let run_state = state_from(&row.get::<String, _>("state"))?;
    let verdict = blocked(row);
    let state = match run_state {
        RunState::Succeeded => match verdict {
            Some(why) => ApiState::ApprovalBlocked(why),
            None => ApiState::Final {
                text: final_text(&row.get::<Option<String>, _>("result").unwrap_or_default()),
                truncated: row.get::<Option<bool>, _>("truncated").unwrap_or(false),
                usage: row
                    .get::<Option<i64>, _>("usage_prompt")
                    .zip(row.get::<Option<i64>, _>("usage_completion"))
                    .map(|(prompt, completion)| TokenUsage {
                        prompt: prompt.max(0) as u64,
                        completion: completion.max(0) as u64,
                    }),
            },
        },
        RunState::Dead => ApiState::Failed {
            reason: row
                .get::<Option<String>, _>("error")
                .unwrap_or_else(|| "run_failed".to_owned()),
        },
        RunState::Cancelled => ApiState::Cancelled,
        // `NeedsHuman` only when an actionable approval card exists (and any DM card job has confirmed `posted`).
        RunState::Waiting | RunState::Ready | RunState::Leased | RunState::Retry => {
            if row.get::<bool, _>("needs_human") {
                ApiState::NeedsHuman
            } else if run_state == RunState::Waiting
                && row.get::<Option<String>, _>("wait_kind").as_deref() == Some("hold")
            {
                ApiState::Held
            } else {
                verdict.map_or(ApiState::Active, ApiState::ApprovalBlocked)
            }
        }
    };
    Ok(ApiRun {
        run: RunId(row.get("id")),
        created: row.get::<i64, _>("created_ms").max(0) as u64 / 1000,
        model: row.get("api_model"),
        binding: BindingId(row.get("binding")),
        agent: AgentId(row.get("agent")),
        revision: Revision(row.get("revision")),
        digest: to32(row.get("input_sha256"))?,
        state,
    })
}

async fn find(tx: &mut Tx<'_>, key: &ApiKey) -> R<Option<ApiRun>> {
    let row = sqlx::query(VIEW)
        .bind(&key.profile.0)
        .bind(&key.endpoint)
        .bind(&key.principal.0)
        .bind(&key.key.0)
        .fetch_optional(&mut **tx)
        .await?;
    row.as_ref().map(view).transpose()
}

fn valid(req: &ApiAccept) -> R<()> {
    let e = &req.accept.event;
    let endpoint_of_principal = e
        .principal
        .0
        .strip_prefix("http:")
        .and_then(|rest| rest.split_once('/'))
        .map(|(endpoint, _)| endpoint);
    let ok = (1..=256).contains(&req.max_active)
        && id_ok(&req.endpoint)
        && !req.endpoint.contains('/')
        && identity_kind(&e.principal.0) == Some(IdentityKind::Http)
        && endpoint_of_principal == Some(req.endpoint.as_str())
        && (1..=128).contains(&e.request.0.len())
        && (1..=63).contains(&req.model.len())
        && id_ok(&req.model)
        && e.conversation.kind == ConversationKind::Http
        // API endpoints dispatch only bound agents, never direct tool targets.
        && matches!(req.accept.target, Target::Agent { .. });
    if ok { Ok(()) } else { invalid() }
}

/// Returns true only for SQLSTATEs guaranteeing rollback before commit (class 23 integrity, 40001/40002/40P01).
/// Ambiguous codes (40003, 08007, disconnects, 57P01) may have committed.
fn rolled_back(code: &str) -> bool {
    code.starts_with("23") || matches!(code, "40001" | "40002" | "40P01")
}

fn commit_unknown(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => !db.code().is_some_and(|code| rolled_back(&code)),
        _ => true,
    }
}

impl Store {
    pub(crate) async fn api_accept(&self, req: ApiAccept) -> R<ApiAdmission> {
        valid(&req)?;
        let a = &req.accept;
        let e = &a.event;
        let checked = check(a)?;
        let profile = &e.profile.0;
        let key = ApiKey {
            profile: e.profile.clone(),
            endpoint: req.endpoint.clone(),
            principal: e.principal.clone(),
            key: e.request.clone(),
        };
        let mut tx = match self.begin_authorized().await {
            Ok(tx) => tx,
            Err(Fail::Core(Error::Stale)) => return Ok(ApiAdmission::Stale),
            Err(other) => return Err(other),
        };
        // Fast-path replay without taking the endpoint lock.
        if let Some(found) = find(&mut tx, &key).await? {
            return Ok(replay(found, &req));
        }
        // Serialize new admissions per endpoint so concurrent duplicates see the first commit.
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("api:{profile}:{}", req.endpoint))
            .execute(&mut *tx)
            .await?;
        if let Some(found) = find(&mut tx, &key).await? {
            return Ok(replay(found, &req));
        }
        match crate::active::guard_active(&mut tx, profile, &a.revision).await {
            Ok(()) => {}
            Err(Fail::Core(Error::Stale)) => return Ok(ApiAdmission::Stale),
            Err(other) => return Err(other),
        }
        let active: i64 = sqlx::query_scalar(&format!(
            "select count(*) from bot_runs where profile = $1 and api_endpoint = $2 and state in {UNFINISHED}"
        ))
        .bind(profile)
        .bind(&req.endpoint)
        .fetch_one(&mut *tx)
        .await?;
        if active >= i64::from(req.max_active) {
            return Ok(ApiAdmission::Capacity);
        }
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        insert_run(
            &mut tx,
            a,
            &checked,
            &run,
            RunOrigin::from_ingress(&e.conversation.kind, &e.principal),
            None,
            None,
        )
        .await?;
        sqlx::query("update bot_runs set api_endpoint = $3 where profile = $1 and id = $2")
            .bind(profile)
            .bind(&run)
            .bind(&req.endpoint)
            .execute(&mut *tx)
            .await?;
        // Unique index is the final guard against concurrent inserts.
        let inserted = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run,
                                       http_method, http_path, http_response, api_endpoint, api_model, api_team)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) on conflict do nothing",
        )
        .bind(profile)
        .bind(&e.binding.0)
        .bind(&e.principal.0)
        .bind(&e.request.0)
        .bind(req.digest.as_slice())
        .bind(&run)
        .bind(e.http.as_ref().map(|h| h.method.as_str()))
        .bind(e.http.as_ref().map(|h| h.path.as_str()))
        .bind(e.http.as_ref().map(|h| http_response(h.response)))
        .bind(&req.endpoint)
        .bind(&req.model)
        .bind(&req.team)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !inserted {
            // Same (profile, binding, principal, request) already exists under another endpoint.
            return Ok(ApiAdmission::Conflict);
        }
        let created = find(&mut tx, &key).await?.ok_or(Error::Unavailable)?;
        let mut entry = Entry::new(&e.principal.0, "run.accept", &run, "accepted");
        entry.args = Some(req.digest);
        audit::append(&mut tx, profile, &entry).await?;
        match tx.commit().await {
            Ok(()) => Ok(ApiAdmission::Accepted(created)),
            Err(error) if commit_unknown(&error) => Ok(ApiAdmission::Unknown),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn api_lookup(&self, key: ApiKey) -> R<Option<ApiRun>> {
        let mut tx = self.begin_authorized().await?;
        let found = find(&mut tx, &key).await?;
        tx.rollback().await?;
        Ok(found)
    }
}

fn http_response(mode: bot_core::profile::ResponseMode) -> &'static str {
    match mode {
        bot_core::profile::ResponseMode::Accepted => "accepted",
        bot_core::profile::ResponseMode::Sync => "sync",
    }
}

/// Replays only when both input digest and model match; mismatches conflict.
fn replay(found: ApiRun, req: &ApiAccept) -> ApiAdmission {
    if found.digest == req.digest && found.model == req.model {
        ApiAdmission::Replayed(found)
    } else {
        ApiAdmission::Conflict
    }
}

impl ApiRuns for Store {
    fn accept(&self, request: ApiAccept) -> Reply<'_, ApiAdmission> {
        Box::pin(async move { self.api_accept(request).await.map_err(Fail::into_core) })
    }

    fn lookup(&self, key: ApiKey) -> Reply<'_, Option<ApiRun>> {
        Box::pin(async move { self.api_lookup(key).await.map_err(Fail::into_core) })
    }
}

#[cfg(test)]
mod tests {
    use super::rolled_back;

    #[test]
    fn only_a_certain_rollback_is_before_the_commit() {
        for certain in ["23503", "23505", "23514", "40001", "40002", "40P01"] {
            assert!(rolled_back(certain), "{certain}");
        }
        // Ambiguous completion, connection errors, admin shutdown, and internal errors may have committed.
        for unknown in [
            "40003", "08007", "08006", "08000", "57P01", "57P02", "XX000", "53300", "",
        ] {
            assert!(!rolled_back(unknown), "{unknown}");
        }
    }
}
