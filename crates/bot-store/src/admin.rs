//! Operator run controls: dead-letter resume and cancellation, with idempotent receipt checks via audit rows.

use crate::{
    audit::{self, Entry, OwnedEntry},
    codec::{ids_ok, target_from},
    db::{R, Store, Tx},
    finish::*,
};
use bot_core::{
    admin::{RunAction, RunCommand, RunCommandOutcome},
    id::*,
    port::Error,
    state::RunState,
};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgRow};

fn short(reason: &str) -> String {
    reason.chars().take(200).collect()
}

fn state_name(state: RunState) -> &'static str {
    match state {
        RunState::Ready => "ready",
        RunState::Leased => "leased",
        RunState::Waiting => "waiting",
        RunState::Retry => "retry",
        RunState::Succeeded => "succeeded",
        RunState::Dead => "dead",
        RunState::Cancelled => "cancelled",
    }
}

impl Store {
    /// Locks a non-private root run, checks idempotency and preconditions, applies the command, and commits with audit.
    pub(crate) async fn control_root_run(
        &self,
        actor: &PrincipalId,
        command: &RunCommand,
    ) -> R<RunCommandOutcome> {
        let (profile, run) = (&command.profile.0, &command.run.0);
        ids_ok(&[profile, run, &actor.0, &command.id])?;
        let state = state_name(command.state);
        let action = match command.action {
            RunAction::Resume => "run.resume",
            RunAction::Cancel => "run.cancel",
        };
        let digest = bot_core::canon::sha256(
            &json!([
                "run-command-v1",
                command.id,
                profile,
                run,
                actor.0,
                action,
                state,
                command.generation.0,
                command.claim_epoch.0
            ])
            .to_string(),
        );
        let mut tx = self.begin_authorized().await?;
        let row = sqlx::query(
            "select root, state, generation, claim_epoch, cancel_requested
             from bot_runs where profile = $1 and id = $2 and not private for update",
        )
        .bind(profile)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("root") != *run {
            return Err(Error::Conflict.into());
        }
        // Check for command replay under the run lock first so retries return the original result even after state/generation changes.
        if let Some(previous) = sqlx::query_scalar::<_, Vec<u8>>(
            "select args_sha256 from bot_audit where profile = $1 and target = $2 and run_command_id = $3",
        )
        .bind(profile)
        .bind(run)
        .bind(&command.id)
        .fetch_optional(&mut *tx)
        .await?
        {
            return if previous == digest {
                Ok(RunCommandOutcome::Applied)
            } else {
                Err(Error::Conflict.into())
            };
        }
        if row.get::<String, _>("state") != state
            || row.get::<i64, _>("generation") as u64 != command.generation.0
            || row.get::<i64, _>("claim_epoch") as u64 != command.claim_epoch.0
        {
            return Err(Error::Stale.into());
        }
        match command.action {
            RunAction::Resume if command.state != RunState::Dead => {
                return Err(Error::Conflict.into());
            }
            RunAction::Cancel
                if !matches!(
                    command.state,
                    RunState::Ready | RunState::Retry | RunState::Waiting | RunState::Leased
                ) || row.get::<bool, _>("cancel_requested") =>
            {
                return Err(Error::Conflict.into());
            }
            _ => {}
        }

        let tree = lock_subtree(&mut tx, profile, run).await?;
        if tree_has_open_hold(&mut tx, profile, run).await? {
            return Ok(RunCommandOutcome::Held);
        }
        let mut audits = Vec::new();
        match command.action {
            RunAction::Resume => {
                if !move_dead_run(&mut tx, profile, run).await? {
                    return Err(Error::Stale.into());
                }
            }
            RunAction::Cancel => {
                cancel_locked(&mut tx, &tree, actor, &mut audits).await?;
            }
        }
        let mut entry = OwnedEntry::new(profile, &actor.0, action, run, "operator_requested");
        entry.args = Some(digest);
        entry.run_command_id = Some(command.id.clone());
        audits.push(entry);
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(RunCommandOutcome::Applied)
    }

    /// Resumes a dead-letter run, resetting attempts while preserving logical run/effect keys; generation advances on next claim.
    pub(crate) async fn resume_run(
        &self,
        profile: &ProfileId,
        run: &RunId,
        actor: &PrincipalId,
        reason: &str,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        if !move_dead_run(&mut tx, &profile.0, &run.0).await? {
            let exists: bool = sqlx::query_scalar("select exists (select 1 from bot_runs where profile = $1 and id = $2 and not private)")
                .bind(&profile.0)
                .bind(&run.0)
                .fetch_one(&mut *tx)
                .await?;
            return Err(if exists {
                Error::Conflict
            } else {
                Error::Missing
            }
            .into());
        }
        audit::append(
            &mut tx,
            &profile.0,
            &Entry::new(&actor.0, "run.resume", &run.0, &short(reason)),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Cancels queued/waiting runs immediately and flags leased runs to stop before their next external effect. Lock order: parent -> child.
    pub(crate) async fn cancel_run(
        &self,
        profile: &ProfileId,
        run: &RunId,
        actor: &PrincipalId,
        reason: &str,
    ) -> R<()> {
        let mut tx = self.begin_authorized().await?;
        let target = sqlx::query(
            "select parent from bot_runs where profile = $1 and id = $2 and not private",
        )
        .bind(&profile.0)
        .bind(&run.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        lock_parent(
            &mut tx,
            &profile.0,
            target.get::<Option<String>, _>("parent").as_deref(),
        )
        .await?;
        let nodes = lock_subtree(&mut tx, &profile.0, &run.0).await?;
        let mut audits = Vec::new();
        let touched = cancel_locked(&mut tx, &nodes, actor, &mut audits).await?;
        let verdict = if touched == 0 {
            "no_effect".to_owned()
        } else {
            short(reason)
        };
        audits.push(OwnedEntry::new(
            &profile.0,
            &actor.0,
            "run.cancel",
            &run.0,
            verdict,
        ));
        audit::append_many(&mut tx, &audits).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn move_dead_run(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<bool> {
    let target: Option<Value> = sqlx::query_scalar(
        "update bot_runs set state = 'ready', attempts = 0, error = null, result = null, finished_ms = null,
                keep_generation = false, cancel_requested = false, updated_ms = bot_now_ms()
         where profile = $1 and id = $2 and state = 'dead' and not private returning target",
    )
    .bind(profile)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(target) = target else {
        return Ok(false);
    };
    target_from(&target)?;
    Ok(true)
}

/// Checks whether any run in the root's tree has an open hold (used under subtree lock for commands and snapshots for reads).
pub(crate) async fn tree_has_open_hold(tx: &mut Tx<'_>, profile: &str, root: &str) -> R<bool> {
    Ok(sqlx::query_scalar(
        "select exists (
             select 1 from bot_runs r join bot_holds h on h.profile = r.profile and h.run = r.id
             where r.profile = $1 and r.root = $2 and h.closed_ms is null
         )",
    )
    .bind(profile)
    .bind(root)
    .fetch_one(&mut **tx)
    .await?)
}

async fn lock_subtree(tx: &mut Tx<'_>, profile: &str, run: &str) -> R<Vec<PgRow>> {
    // A child delegation may commit while waiting on its parent's lock; locked parents accept no new
    // delegations, so re-lock in depth/id order until the subtree size stabilizes.
    let mut locked = 0;
    loop {
        let nodes = sqlx::query(
            "with recursive tree as (
                 select profile, id, depth from bot_runs where profile = $1 and id = $2
                 union all
                 select c.profile, c.id, c.depth from bot_runs c join tree t on c.profile = t.profile and c.parent = t.id
             )
             select r.profile, r.id, r.parent, r.parent_step, r.attempts, r.state
             from bot_runs r join tree t on t.profile = r.profile and t.id = r.id
             order by r.depth, r.id for update of r",
        )
        .bind(profile)
        .bind(run)
        .fetch_all(&mut **tx)
        .await?;
        if nodes.len() == locked {
            return Ok(nodes);
        }
        locked = nodes.len();
    }
}

async fn cancel_locked(
    tx: &mut Tx<'_>,
    nodes: &[PgRow],
    actor: &PrincipalId,
    audits: &mut Vec<OwnedEntry>,
) -> R<usize> {
    let mut touched = 0;
    for node in nodes {
        let state: String = node.get("state");
        let row = RunRow {
            profile: node.get("profile"),
            id: node.get("id"),
            parent: node.get("parent"),
            parent_step: node.get("parent_step"),
            attempts: node.get("attempts"),
            cancel_requested: true,
            private: false,
        };
        match state.as_str() {
            "ready" | "retry" | "waiting" => {
                end_run_into(tx, &row, End::Cancelled("cancelled"), &actor.0, audits).await?;
                touched += 1;
            }
            "leased" => {
                sqlx::query("update bot_runs set cancel_requested = true, updated_ms = bot_now_ms() where profile = $1 and id = $2")
                    .bind(&row.profile)
                    .bind(&row.id)
                    .execute(&mut **tx)
                    .await?;
                touched += 1;
            }
            _ => {}
        }
    }
    Ok(touched)
}
