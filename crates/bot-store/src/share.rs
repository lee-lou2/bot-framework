//! Atomic memory sharing (`MemoryStore::share`): verifies owner and body digest, consumes approval, updates visibility, completes the call, and audits in one commit.

use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, lock_root_shared, phase_of, risk_str, tree_flagged},
    codec::*,
    db::{R, Store},
    memory::lineage,
};
use bot_core::{
    command::memory_share_hash,
    id::*,
    ledger::{CallPhase, StartCall},
    port::Error,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;

pub(crate) use bot_core::builtin::SHARE_TOOL;

impl Store {
    pub(crate) async fn memory_share(&self, s: StartCall, id: MemoryId) -> R<MemoryId> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0, &id.0, &s.call.0])?;
        let mut tx = self.begin_authorized().await?;
        // Lock order: active revision -> root run (FOR SHARE) -> run -> call -> approval -> memory -> audit.
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        if call.get::<String, _>("tool") != SHARE_TOOL {
            return Err(Error::Denied.into());
        }
        match phase_of(&call)? {
            CallPhase::Done(result) => {
                // Idempotent replay of a completed share call for the same memory ID.
                tx.rollback().await?;
                let done = serde_json::from_str::<Value>(&result.0)
                    .ok()
                    .and_then(|v| v.get("id").and_then(Value::as_str).map(str::to_owned));
                return if done.as_deref() == Some(id.0.as_str()) {
                    Ok(id)
                } else {
                    Err(Error::Conflict.into())
                };
            }
            CallPhase::Open { .. } => {}
            _ => return Err(Error::Conflict.into()),
        }
        if tree_flagged(&mut tx, &run, shared_flag).await? && !s.private_recall {
            // Another run read private memory after policy evaluation.
            return Err(Error::Conflict.into());
        }
        let Some((expected, reasons)) = &s.approval else {
            return Err(Error::Denied.into());
        };
        let me = sqlx::query(
            "select principal, agent, conv_kind from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (principal, agent, kind): (String, String, String) =
            (me.get("principal"), me.get("agent"), me.get("conv_kind"));
        if expected.tool.0 != SHARE_TOOL
            || expected.requester.0 != principal
            || expected.approver.0 != principal
        {
            return Err(Error::Denied.into());
        }
        crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
        let memory = sqlx::query(
            "select agent, owner, visibility, body from bot_memories
             where profile = $1 and id = $2 and deleted_ms is null for update",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        let private = memory.get::<String, _>("visibility") == "private";
        if memory.get::<String, _>("owner") != principal
            || !lineage(&agent).contains(&memory.get::<String, _>("agent"))
            || (private && !matches!(kind.as_str(), "direct" | "cli"))
        {
            return Err(Error::Denied.into());
        }
        // Reject if memory body changed after the approval card was shown (rolls back approval consumption).
        let body: String = memory.get("body");
        let fingerprint = memory_share_hash(&id, &Sha256::digest(body.as_bytes()).into());
        let call_hash: Vec<u8> = call.get("args_sha256");
        if expected.args_sha256 != fingerprint || call_hash.as_slice() != fingerprint.as_slice() {
            return Err(Error::Conflict.into());
        }
        let shared = sqlx::query(
            "update bot_memories set visibility = 'profile', updated_ms = bot_now_ms()
             where profile = $1 and id = $2 and visibility = 'private'",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;
        sqlx::query(
            "update bot_calls set state = 'done', result = $4, started_generation = $5, owner = $6, started_ms = bot_now_ms(),
                    finished_ms = bot_now_ms(), updated_ms = bot_now_ms(), risk = $7, tool_revision = $8, approval = $9
             where profile = $1 and run = $2 and call = $3",
        )
        .bind(&at.profile.0)
        .bind(&at.run.0)
        .bind(&s.call.0)
        .bind(json!({ "id": id.0 }).to_string())
        .bind(at.generation.0 as i64)
        .bind(&s.owner)
        .bind(risk_str(s.risk))
        .bind(&s.tool_revision.0)
        .bind(&expected.nonce.0)
        .execute(&mut *tx)
        .await?;
        let mut entry = OwnedEntry::new(
            &run.profile,
            &principal,
            "memory.share",
            &id.0,
            if shared { "shared" } else { "already_shared" },
        );
        entry.args = Some(fingerprint);
        let mut done = OwnedEntry::new(&run.profile, &principal, "call.done", &s.call.0, "done");
        done.args = Some(to32(call_hash)?);
        audit::append_many(&mut tx, &[entry, done]).await?;
        tx.commit().await?;
        Ok(id)
    }
}
