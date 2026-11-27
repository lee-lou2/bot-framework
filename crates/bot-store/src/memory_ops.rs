//! Atomic ledger-fenced `memory.save` (quota, deduplication, supersede) and `memory.forget` store operations.
use crate::{
    audit::{self, OwnedEntry},
    calls::{fence, lock_call, lock_root_shared, phase_of, tree_flagged},
    codec::*,
    db::{R, Store, Tx},
    memory::{lineage, text_ok},
    user_schedule::{done_entry, finish_call},
};
use bot_core::{
    builtin::{MEMORY_FORGET_TOOL, MEMORY_SAVE_TOOL},
    id::*,
    ledger::{CallPhase, StartCall},
    memory::{
        MAX_ACTIVE_MEMORIES_PER_OWNER, MemorySave, MemorySaved, dot_f16, forget_seal, save_seal,
    },
    port::Error,
};
use serde_json::{Value, json};
use sqlx::Row;

/// Near-duplicate cosine similarity cutoff for skipping redundant active saves without an LLM call.
const NEAR_DUP_COS: f32 = 0.94;

fn parse_saved(result: &str) -> Option<MemorySaved> {
    let Value::Object(map) = serde_json::from_str::<Value>(result).ok()? else {
        return None;
    };
    let id = map.get("id")?.as_str()?.to_owned();
    let deduplicated = map
        .get("deduplicated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(MemorySaved {
        id: MemoryId(id),
        deduplicated,
    })
}

fn parse_forgotten(result: &str) -> Option<MemoryId> {
    serde_json::from_str::<Value>(result)
        .ok()?
        .get("id")?
        .as_str()
        .map(|s| MemoryId(s.to_owned()))
}

async fn lock_owner_quota(tx: &mut Tx<'_>, profile: &str, owner: &str) -> R<()> {
    sqlx::query(
        "select pg_advisory_xact_lock(hashtextextended('bot.memory:' || $1 || ':' || $2, 0))",
    )
    .bind(profile)
    .bind(owner)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    pub(crate) async fn memory_save(&self, s: StartCall, spec: MemorySave) -> R<MemorySaved> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0])?;
        text_ok(&spec.text)?;
        if let Some(old) = &spec.supersedes {
            ids_ok(&[&old.0])?;
        }
        let (_, sealed_sha) =
            save_seal(&spec.text, spec.kind, spec.supersedes.as_ref()).ok_or(Error::Invalid)?;

        let mut tx = self.begin_authorized().await?;
        // Lock order: active revision -> root run (FOR SHARE) -> run -> call -> approval -> owner lock -> memory -> audit.
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        if call.get::<String, _>("tool") != MEMORY_SAVE_TOOL {
            return Err(Error::Denied.into());
        }
        if call.get::<Vec<u8>, _>("args_sha256").as_slice() != sealed_sha.as_slice() {
            return Err(Error::Conflict.into());
        }
        let pointer = match phase_of(&call)? {
            CallPhase::Done(result) => {
                tx.rollback().await?;
                return parse_saved(&result.0).ok_or_else(|| Error::Conflict.into());
            }
            CallPhase::Open { approval } => approval,
            _ => return Err(Error::Conflict.into()),
        };
        if tree_flagged(&mut tx, &run, shared_flag).await? && !s.private_recall {
            return Err(Error::Conflict.into());
        }
        crate::approvals::require_presented(&mut tx, pointer.as_ref(), s.approval.is_some())
            .await?;
        let me = sqlx::query(
            "select principal, agent, conv_kind from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (principal, agent, conv_kind): (String, String, String) =
            (me.get("principal"), me.get("agent"), me.get("conv_kind"));
        if !PrincipalId(principal.clone()).is_human()
            || !matches!(conv_kind.as_str(), "direct" | "cli")
        {
            return Err(Error::Denied.into());
        }
        let approval_nonce = match &s.approval {
            Some((expected, reasons)) => {
                if expected.tool.0 != MEMORY_SAVE_TOOL
                    || expected.requester.0 != principal
                    || expected.approver.0 != principal
                {
                    return Err(Error::Denied.into());
                }
                crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
                Some(expected.nonce.0.clone())
            }
            None => None,
        };

        lock_owner_quota(&mut tx, &run.profile, &principal).await?;

        // Check exact body or near-duplicate active entry when not explicitly superseding another ID.
        let mut dedup_id: Option<String> = None;
        if spec.supersedes.is_none() {
            let existing = sqlx::query(
                "select id, body, embedding_model, embedding_dim, embedding_f16
                 from bot_memories
                 where profile = $1 and agent = $2 and owner = $3 and visibility = 'private'
                   and deleted_ms is null and superseded_by is null",
            )
            .bind(&run.profile)
            .bind(&agent)
            .bind(&principal)
            .fetch_all(&mut *tx)
            .await?;
            let new_trim = spec.text.trim();
            for row in &existing {
                let body: String = row.get("body");
                if body.trim() == new_trim {
                    dedup_id = Some(row.get("id"));
                    break;
                }
                if let Some(emb) = &spec.embedding
                    && row.get::<Option<String>, _>("embedding_model").as_deref()
                        == Some(emb.model.as_str())
                    && row.get::<Option<i32>, _>("embedding_dim") == Some(emb.dim() as i32)
                    && let Some(bytes) = row.get::<Option<Vec<u8>>, _>("embedding_f16")
                    && dot_f16(&bytes, &emb.values).is_some_and(|cos| cos >= NEAR_DUP_COS)
                {
                    dedup_id = Some(row.get("id"));
                    break;
                }
            }
        }

        let (saved, verdict) = if let Some(id) = dedup_id {
            if let Some(emb) = &spec.embedding {
                sqlx::query(
                    "update bot_memories
                     set embedding_model = $3, embedding_dim = $4, embedding_f16 = $5
                     where profile = $1 and id = $2 and embedding_f16 is null",
                )
                .bind(&run.profile)
                .bind(&id)
                .bind(&emb.model)
                .bind(emb.dim() as i32)
                .bind(emb.to_f16_bytes())
                .execute(&mut *tx)
                .await?;
            }
            (
                MemorySaved {
                    id: MemoryId(id),
                    deduplicated: true,
                },
                "deduplicated",
            )
        } else {
            // Verify target of `supersedes` before quota so replacing an entry does not grow active count.
            if let Some(old_id) = &spec.supersedes {
                let old = sqlx::query(
                    "select agent, owner, visibility from bot_memories
                     where profile = $1 and id = $2 and deleted_ms is null and superseded_by is null for update",
                )
                .bind(&run.profile)
                .bind(&old_id.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(Error::Missing)?;
                if old.get::<String, _>("owner") != principal
                    || !lineage(&agent).contains(&old.get::<String, _>("agent"))
                {
                    return Err(Error::Denied.into());
                }
            } else {
                let active_count: i64 = sqlx::query_scalar(
                    "select count(*) from bot_memories
                     where profile = $1 and owner = $2 and seed_path is null
                       and deleted_ms is null and superseded_by is null",
                )
                .bind(&run.profile)
                .bind(&principal)
                .fetch_one(&mut *tx)
                .await?;
                if active_count >= MAX_ACTIVE_MEMORIES_PER_OWNER {
                    return Err(Error::Denied.into());
                }
            }

            let emb_model = spec.embedding.as_ref().map(|e| e.model.as_str());
            let emb_dim = spec.embedding.as_ref().map(|e| e.dim() as i32);
            let emb_bytes = spec.embedding.as_ref().map(|e| e.to_f16_bytes());
            let new_id: String = sqlx::query_scalar(
                "insert into bot_memories (profile, agent, owner, visibility, kind, body, embedding_model, embedding_dim, embedding_f16)
                 values ($1, $2, $3, 'private', $4, $5, $6, $7, $8) returning id",
            )
            .bind(&run.profile)
            .bind(&agent)
            .bind(&principal)
            .bind(spec.kind.as_str())
            .bind(&spec.text)
            .bind(emb_model)
            .bind(emb_dim)
            .bind(emb_bytes)
            .fetch_one(&mut *tx)
            .await?;

            if let Some(old_id) = &spec.supersedes {
                sqlx::query(
                    "update bot_memories
                     set superseded_by = $3, valid_until_ms = bot_now_ms(), updated_ms = bot_now_ms()
                     where profile = $1 and id = $2",
                )
                .bind(&run.profile)
                .bind(&old_id.0)
                .bind(&new_id)
                .execute(&mut *tx)
                .await?;
            }
            (
                MemorySaved {
                    id: MemoryId(new_id),
                    deduplicated: false,
                },
                if spec.supersedes.is_some() {
                    "superseded"
                } else {
                    "saved"
                },
            )
        };

        let result_json =
            json!({ "id": saved.id.0, "deduplicated": saved.deduplicated }).to_string();
        finish_call(&mut tx, &s, &result_json, approval_nonce.as_deref()).await?;

        let mut mem_audit = OwnedEntry::new(
            &run.profile,
            &principal,
            "memory.save",
            &saved.id.0,
            verdict,
        );
        mem_audit.args = Some(sha256_text(&spec.text));
        let done = done_entry(&run.profile, &principal, &s.call.0, sealed_sha);
        audit::append_many(&mut tx, &[mem_audit, done]).await?;
        tx.commit().await?;
        Ok(saved)
    }

    pub(crate) async fn memory_forget(&self, s: StartCall, id: MemoryId) -> R<MemoryId> {
        let at = &s.attempt;
        ids_ok(&[&s.owner, &s.checked_revision.0, &s.call.0, &id.0])?;
        let (_, sealed_sha) = forget_seal(&id).ok_or(Error::Invalid)?;

        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &at.profile.0, &s.checked_revision).await?;
        let shared_flag = lock_root_shared(&mut tx, at).await?;
        let run = fence(&mut tx, at).await?;
        let call = lock_call(&mut tx, at, &s.call).await?;
        if call.get::<String, _>("tool") != MEMORY_FORGET_TOOL {
            return Err(Error::Denied.into());
        }
        if call.get::<Vec<u8>, _>("args_sha256").as_slice() != sealed_sha.as_slice() {
            return Err(Error::Conflict.into());
        }
        let pointer = match phase_of(&call)? {
            CallPhase::Done(result) => {
                tx.rollback().await?;
                return match parse_forgotten(&result.0) {
                    Some(done) if done == id => Ok(id),
                    _ => Err(Error::Conflict.into()),
                };
            }
            CallPhase::Open { approval } => approval,
            _ => return Err(Error::Conflict.into()),
        };
        if tree_flagged(&mut tx, &run, shared_flag).await? && !s.private_recall {
            return Err(Error::Conflict.into());
        }
        crate::approvals::require_presented(&mut tx, pointer.as_ref(), s.approval.is_some())
            .await?;
        let me = sqlx::query(
            "select principal, agent, conv_kind from bot_runs where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&run.id)
        .fetch_one(&mut *tx)
        .await?;
        let (principal, agent, conv_kind): (String, String, String) =
            (me.get("principal"), me.get("agent"), me.get("conv_kind"));
        if !PrincipalId(principal.clone()).is_human()
            || !matches!(conv_kind.as_str(), "direct" | "cli")
        {
            return Err(Error::Denied.into());
        }
        let approval_nonce = match &s.approval {
            Some((expected, reasons)) => {
                if expected.tool.0 != MEMORY_FORGET_TOOL
                    || expected.requester.0 != principal
                    || expected.approver.0 != principal
                {
                    return Err(Error::Denied.into());
                }
                crate::approvals::consume(&mut tx, at, &s.call, expected, reasons).await?;
                Some(expected.nonce.0.clone())
            }
            None => None,
        };

        let row = sqlx::query(
            "select agent, owner from bot_memories where profile = $1 and id = $2 and deleted_ms is null for update",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Missing)?;
        if row.get::<String, _>("owner") != principal
            || !lineage(&agent).contains(&row.get::<String, _>("agent"))
        {
            return Err(Error::Denied.into());
        }

        sqlx::query(
            "update bot_memories
             set body = '', embedding_model = null, embedding_dim = null, embedding_f16 = null,
                 deleted_ms = bot_now_ms(), updated_ms = bot_now_ms()
             where profile = $1 and id = $2",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .execute(&mut *tx)
        .await?;

        let result_json = json!({ "id": id.0 }).to_string();
        finish_call(&mut tx, &s, &result_json, approval_nonce.as_deref()).await?;

        let mem_audit = OwnedEntry::new(&run.profile, &principal, "memory.forget", &id.0, "ok");
        let done = done_entry(&run.profile, &principal, &s.call.0, sealed_sha);
        audit::append_many(&mut tx, &[mem_audit, done]).await?;
        tx.commit().await?;
        Ok(id)
    }
}
