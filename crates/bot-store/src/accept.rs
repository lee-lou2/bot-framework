//! External request intake and status lookup; acceptance is durable only after commit.

use crate::{
    audit::{self, OwnedEntry},
    codec::*,
    db::{R, Store, Tx},
};
use bot_core::{
    command::{Accept, AcceptExtras, HttpRoute},
    id::*,
    intake::{PushTransportId, RequestKey, RequestState},
    port::Error,
    profile::ResponseMode,
    state::{ConversationKind, RunOrigin},
};
use serde_json::{Value, json};
use sqlx::Row;

/// Hashes only external inputs (conversation, attachments, canonical body, and HTTP route).
/// Excludes `Accept.target` so redeliveries after a profile revision change converge on the original run instead of conflicting.
fn input_digest(a: &Accept, body: &Value) -> R<[u8; 32]> {
    let c = &a.event.conversation;
    let mut doc = json!({
        "conversation": {"kind": kind_str(&c.kind), "id": c.id, "thread": c.thread},
        "attachments": a.event.attachments,
        "body": body,
    });
    // HTTP requests include method and path without altering non-HTTP digest bytes.
    if let Some(route) = &a.event.http {
        doc["http"] = json!({"method": route.method, "path": route.path});
    }
    Ok(sha256_text(&doc.to_string()))
}

fn route_ok(method: &str, path: &str) -> bool {
    !method.is_empty()
        && method.len() <= 16
        && method.bytes().all(|b| b.is_ascii_uppercase())
        && path.starts_with('/')
        && path.len() <= 512
        && !path.contains('?')
        && path.chars().all(|c| !c.is_control() && c != ' ')
}

fn response_str(mode: ResponseMode) -> &'static str {
    match mode {
        ResponseMode::Accepted => "accepted",
        ResponseMode::Sync => "sync",
    }
}

pub(crate) struct Checked {
    pub agent: AgentId,
    pub scopes: Vec<String>,
    pub body_text: String,
    pub target: Value,
    pub digest: [u8; 32],
}

pub(crate) fn check(a: &Accept) -> R<Checked> {
    let e = &a.event;
    let agent = agent_of(&a.target).clone();
    ids_ok(&[
        &e.profile.0,
        &e.binding.0,
        &e.principal.0,
        &e.request.0,
        &e.conversation.id,
        &a.revision.0,
        &agent.0,
    ])?;
    if e.conversation.thread.as_deref().is_some_and(|t| !id_ok(t)) {
        return invalid();
    }
    // HTTP conversations must carry a validated route, and vice versa.
    if (e.conversation.kind == ConversationKind::Http) != e.http.is_some()
        || e.http
            .as_ref()
            .is_some_and(|h| !route_ok(&h.method, &h.path))
    {
        return invalid();
    }
    if a.reply_transport.is_some() && !push_conversation(&e.conversation.kind) {
        return invalid();
    }
    attachments_ok(&e.attachments)?;
    let scopes: Vec<String> = a.scopes.iter().cloned().collect();
    if scopes.iter().any(|s| !id_ok(s)) || a.approver.as_ref().is_some_and(|p| !id_ok(&p.0)) {
        return invalid();
    }
    let (body, body_text) = canonical(&e.body)?;
    Ok(Checked {
        digest: input_digest(a, &body)?,
        target: target_json(&a.target)?,
        agent,
        scopes,
        body_text,
    })
}

fn push_conversation(kind: &ConversationKind) -> bool {
    match kind {
        ConversationKind::Direct | ConversationKind::Channel => true,
        ConversationKind::Cli | ConversationKind::Http | ConversationKind::Scheduled => false,
    }
}

pub(crate) async fn insert_run(
    tx: &mut Tx<'_>,
    a: &Accept,
    c: &Checked,
    id: &str,
    origin: RunOrigin,
    trigger: Option<&str>,
    serial_key: Option<&str>,
) -> R<()> {
    let e = &a.event;
    sqlx::query(
        "insert into bot_runs (profile, id, root, agent, principal, scopes, revision, binding,
                               conv_kind, conv_id, thread, attachments, target, input, approver, trigger, serial_key, run_origin,
                               reply_transport)
         values ($1, $2, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(&e.profile.0)
    .bind(id)
    .bind(&c.agent.0)
    .bind(&e.principal.0)
    .bind(&c.scopes)
    .bind(&a.revision.0)
    .bind(&e.binding.0)
    .bind(kind_str(&e.conversation.kind))
    .bind(&e.conversation.id)
    .bind(&e.conversation.thread)
    .bind(&e.attachments)
    .bind(&c.target)
    .bind(&c.body_text)
    .bind(a.approver.as_ref().map(|p| p.0.as_str()))
    .bind(trigger)
    .bind(serial_key)
    .bind(origin.as_str())
    .bind(a.reply_transport.as_ref().map(|key| key.as_str()))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Deduplicates by `(profile, binding, principal, request)`: matching digest returns the original run; mismatch returns `Conflict`.
    pub(crate) async fn accept_run_with(&self, a: Accept, extras: AcceptExtras) -> R<RunId> {
        if (push_conversation(&a.event.conversation.kind) && a.reply_transport.is_none())
            || (extras.source.is_some() && a.reply_transport != Some(PushTransportId::SLACK))
        {
            return invalid();
        }
        let chain = extras.chain.as_ref();
        let c = check(&a)?;
        let e = &a.event;
        let profile = &e.profile.0;
        // Deduplicate verified Slack events across binding/profile reassignments.
        let slack_key = extras
            .source
            .as_ref()
            .map(crate::slack_events::key_of)
            .transpose()?;
        let mut tx = self.begin_authorized().await?;
        // Adopt legacy pre-`slack_key` rows so redeliveries converge on the original run.
        if let (Some(source), Some(key)) = (&extras.source, &slack_key)
            && let Some(run) = crate::slack_events::adopt_legacy(&mut tx, source, key).await?
        {
            tx.commit().await?;
            return Ok(run);
        }
        let run: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        // Persist response mode on first accept without including it in the dedup digest.
        let fresh = sqlx::query(
            "insert into bot_requests (profile, binding, principal, request, input_sha256, run,
                                       http_method, http_path, http_response, slack_key)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) on conflict do nothing",
        )
        .bind(profile)
        .bind(&e.binding.0)
        .bind(&e.principal.0)
        .bind(&e.request.0)
        .bind(c.digest.as_slice())
        .bind(&run)
        .bind(e.http.as_ref().map(|h| h.method.as_str()))
        .bind(e.http.as_ref().map(|h| h.path.as_str()))
        .bind(e.http.as_ref().map(|h| response_str(h.response)))
        .bind(&slack_key)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        if !fresh {
            // Converge matching Slack events onto the original run regardless of input hash.
            if let Some(key) = &slack_key
                && let Some(row) = sqlx::query("select run from bot_requests where slack_key = $1")
                    .bind(key)
                    .fetch_optional(&mut *tx)
                    .await?
            {
                tx.rollback().await?;
                return Ok(RunId(row.get("run")));
            }
            let row = sqlx::query(
                "select input_sha256, run, api_endpoint from bot_requests
                 where profile = $1 and binding = $2 and principal = $3 and request = $4",
            )
            .bind(profile)
            .bind(&e.binding.0)
            .bind(&e.principal.0)
            .bind(&e.request.0)
            .fetch_one(&mut *tx)
            .await?;
            let stored: Vec<u8> = row.get("input_sha256");
            let api: Option<String> = row.get("api_endpoint");
            tx.rollback().await?;
            // Never let regular intake read or adopt OpenAI-compatible API request rows.
            return if api.is_none() && stored.as_slice() == c.digest.as_slice() {
                Ok(RunId(row.get("run")))
            } else {
                Err(Error::Conflict.into())
            };
        }
        // Validate active revision only for fresh requests; duplicates already returned above.
        crate::active::guard_active(&mut tx, profile, &a.revision).await?;
        let origin = RunOrigin::from_ingress(&e.conversation.kind, &e.principal);
        insert_run(&mut tx, &a, &c, &run, origin, None, None).await?;
        if let Some(chain) = chain {
            crate::pass::store_chain(&mut tx, &a, &run, chain).await?;
        }
        crate::evidence::bind_in(&mut tx, &a, &run, &extras).await?;
        let mut entry = OwnedEntry::new(profile, &e.principal.0, "run.accept", &run, "accepted");
        entry.args = Some(c.digest);
        let mut audits = vec![entry];
        if let Some(first) = chain.and_then(|chain| chain.candidates.first()) {
            audits.push(OwnedEntry::new(
                profile,
                &e.principal.0,
                "trigger.select",
                &run,
                format!("{}@0", first.trigger),
            ));
        }
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(RunId(run))
    }

    pub(crate) async fn status_of(&self, key: &RequestKey) -> R<Option<RequestState>> {
        let mut tx = self.begin_authorized().await?;
        let row = sqlx::query(
            "select r.id, r.state, r.result, q.http_method, q.http_path, q.http_response from bot_requests q
             join bot_runs r on r.profile = q.profile and r.id = q.run
             where q.profile = $1 and q.binding = $2 and q.principal = $3 and q.request = $4
               and q.api_endpoint is null",
        )
        .bind(&key.profile.0)
        .bind(&key.binding.0)
        .bind(&key.principal.0)
        .bind(&key.request.0)
        .fetch_optional(&mut *tx)
        .await?;
        tx.rollback().await?;
        let Some(row) = row else { return Ok(None) };
        let result: Option<String> = row.get("result");
        let http = match (
            row.get::<Option<String>, _>("http_method"),
            row.get::<Option<String>, _>("http_path"),
            row.get::<Option<String>, _>("http_response"),
        ) {
            (Some(method), Some(path), Some(mode)) => Some(HttpRoute {
                method,
                path,
                response: if mode == "sync" {
                    ResponseMode::Sync
                } else {
                    ResponseMode::Accepted
                },
            }),
            _ => None,
        };
        Ok(Some(RequestState {
            run: RunId(row.get("id")),
            state: state_from(&text(&row, "state"))?,
            result: result.map(bot_core::command::JsonPayload),
            http,
        }))
    }

    /// Summarizes run-tree progress for Slack status cards without reading payloads or args.
    pub(crate) async fn progress_of(
        &self,
        profile: &ProfileId,
        run: &RunId,
    ) -> R<Option<bot_core::intake::RunProgress>> {
        let mut tx = self.begin_authorized().await?;
        let rows = sqlx::query(
            "select id, agent, state, wait_kind, cancel_requested, depth
             from bot_runs
             where profile = $1 and root = $2
             order by depth desc, updated_ms desc",
        )
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_all(&mut *tx)
        .await?;
        let Some(root) = rows.iter().find(|r| text(r, "id") == run.0) else {
            tx.rollback().await?;
            return Ok(None);
        };
        let state = state_from(&text(root, "state"))?;
        let cancel_requested: bool = root.get("cancel_requested");
        let terminal = matches!(
            state,
            bot_core::state::RunState::Succeeded
                | bot_core::state::RunState::Dead
                | bot_core::state::RunState::Cancelled
        );
        let mut active_agent = None;
        let mut active_tool = None;
        let mut waiting_approval =
            root.get::<Option<String>, _>("wait_kind").as_deref() == Some("approval");
        if !terminal {
            for r in &rows {
                if text(r, "id") == run.0 {
                    continue;
                }
                let st = text(r, "state");
                if matches!(st.as_str(), "ready" | "leased" | "waiting" | "retry") {
                    active_agent = Some(text(r, "agent"));
                    if r.get::<Option<String>, _>("wait_kind").as_deref() == Some("approval") {
                        waiting_approval = true;
                    }
                    break;
                }
            }
            let call = sqlx::query(
                "select c.tool, (c.state = 'open' and a.nonce is not null) as pending_approval
                 from bot_calls c
                 join bot_runs r on r.profile = c.profile and r.id = c.run
                 left join bot_approvals a
                   on a.profile = c.profile and a.nonce = c.approval and a.state = 'pending'
                 where c.profile = $1 and r.root = $2
                   and (c.state = 'started' or (c.state = 'open' and a.nonce is not null))
                 order by case when c.state = 'open' and a.nonce is not null then 0 else 1 end, c.updated_ms desc
                 limit 1",
            )
            .bind(&profile.0)
            .bind(&run.0)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(call) = call {
                active_tool = Some(text(&call, "tool"));
                if call.get::<bool, _>("pending_approval") {
                    waiting_approval = true;
                }
            }
        }
        tx.rollback().await?;
        Ok(Some(bot_core::intake::RunProgress {
            state,
            cancel_requested,
            active_agent,
            active_tool,
            waiting_approval,
        }))
    }
}

use crate::db::text;
