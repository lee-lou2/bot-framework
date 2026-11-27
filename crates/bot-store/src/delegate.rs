//! Agent delegation: creates a direct-child run and parks the parent atomically, deduplicated by `(parent, step)`.

use crate::{
    audit::{self, Entry},
    codec::*,
    db::{R, Store},
    finish::*,
};
use bot_core::{
    command::{Delegate, JsonPayload, Target},
    id::*,
    port::Error,
};
use serde_json::json;
use sqlx::Row;
use std::collections::BTreeSet;

/// Maximum delegation tree depth (root is 0).
const MAX_DEPTH: i32 = 8;

/// Returns true if `child` is a direct child path of `parent` (e.g. `main` -> `main/research`).
fn is_direct_child(parent: &str, child: &str) -> bool {
    child
        .strip_prefix(parent)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|name| !name.is_empty() && !name.contains('/'))
}

impl Store {
    pub(crate) async fn delegate_run(&self, d: Delegate) -> R<RunId> {
        ids_ok(&[&d.step, &d.child.0, &d.checked_revision.0])?;
        if d.instruction.trim().is_empty() || d.scopes.iter().any(|s| !id_ok(s)) {
            return invalid();
        }
        let target = target_json(&Target::Agent {
            agent: d.child.clone(),
            instruction: d.instruction.clone(),
        })?;
        let (_, input) = canonical(&JsonPayload(json!({"text": d.instruction}).to_string()))?;

        // Lock order: active set -> active revision -> parent run.
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &d.parent.profile.0, &d.checked_revision).await?;
        let parent = lock_leased(&mut tx, &d.parent).await?;
        if parent.private || parent.cancel_requested {
            return Err(Error::Denied.into());
        }
        let p = sqlx::query(
            "select root, depth, agent, principal, approver, scopes, revision, binding, conv_kind, conv_id, thread,
                    reply_transport
             from bot_runs where profile = $1 and id = $2",
        )
        .bind(&parent.profile)
        .bind(&parent.id)
        .fetch_one(&mut *tx)
        .await?;
        let agent: String = p.get("agent");
        let depth: i32 = p.get("depth");
        let parent_scopes: Vec<String> = p.get("scopes");
        let parent_scopes: BTreeSet<String> = parent_scopes.into_iter().collect();
        if !is_direct_child(&agent, &d.child.0)
            || depth + 1 > MAX_DEPTH
            || !d.scopes.is_subset(&parent_scopes)
        {
            return Err(Error::Denied.into());
        }
        // Deduplicate retries of the same step when agent, input, and scopes match.
        let existing = sqlx::query(
            "select id, agent, input, scopes from bot_runs where profile = $1 and parent = $2 and parent_step = $3",
        )
        .bind(&parent.profile)
        .bind(&parent.id)
        .bind(&d.step)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(row) = existing {
            let stored: Vec<String> = row.get("scopes");
            let same = row.get::<String, _>("agent") == d.child.0
                && row.get::<String, _>("input") == input
                && stored.into_iter().collect::<BTreeSet<_>>() == d.scopes;
            tx.rollback().await?;
            return if same {
                Ok(RunId(row.get("id")))
            } else {
                Err(Error::Conflict.into())
            };
        }
        let child: String = sqlx::query_scalar("select gen_random_uuid()::text")
            .fetch_one(&mut *tx)
            .await?;
        let scopes: Vec<String> = d.scopes.iter().cloned().collect();
        sqlx::query(
            "insert into bot_runs (profile, id, root, parent, parent_step, depth, agent, principal, approver, scopes, revision,
                                   binding, conv_kind, conv_id, thread, target, input, reply_transport)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
        )
        .bind(&parent.profile)
        .bind(&child)
        .bind(p.get::<String, _>("root"))
        .bind(&parent.id)
        .bind(&d.step)
        .bind(depth + 1)
        .bind(&d.child.0)
        .bind(p.get::<String, _>("principal"))
        .bind(p.get::<Option<String>, _>("approver"))
        .bind(&scopes)
        .bind(p.get::<String, _>("revision"))
        .bind(p.get::<String, _>("binding"))
        .bind(p.get::<String, _>("conv_kind"))
        .bind(p.get::<String, _>("conv_id"))
        .bind(p.get::<Option<String>, _>("thread"))
        .bind(&target)
        .bind(&input)
        .bind(p.get::<Option<String>, _>("reply_transport"))
        .execute(&mut *tx)
        .await?;
        park(&mut tx, &parent, &Wait::Child(child.clone())).await?;
        let principal: String = p.get("principal");
        audit::append(
            &mut tx,
            &parent.profile,
            &Entry::new(&principal, "run.delegate", &child, &d.step),
        )
        .await?;
        tx.commit().await?;
        Ok(RunId(child))
    }
}

#[cfg(test)]
mod tests {
    use super::is_direct_child;

    #[test]
    fn only_direct_children_qualify() {
        assert!(is_direct_child("main", "main/research"));
        assert!(!is_direct_child("main", "main"));
        assert!(!is_direct_child("main", "main/research/deep"));
        assert!(!is_direct_child("main", "mainx/research"));
        assert!(!is_direct_child("main/research", "main/other"));
        assert!(!is_direct_child("main", "main/"));
    }
}
