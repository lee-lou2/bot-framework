//! Host-owned automatic private memory persistence (`auto_save`/`auto_forget`) and `MemoryStore` port wiring.
use crate::{
    audit::{self, Entry},
    codec::*,
    db::{Fail, R, Store},
    finish::lock_leased,
    memory::{lineage, text_ok},
    memory_ops::{NEAR_DUP_COS, lock_owner_quota},
};
use bot_core::{
    builtin::{MEMORY_FORGET_TOOL, MEMORY_SAVE_TOOL},
    command::*,
    id::*,
    ledger::StartCall,
    memory::{
        MAX_ACTIVE_MEMORIES_PER_OWNER, MemoryEmbedding, MemoryQuery, MemorySave, MemorySaved,
        UnembeddedMemory, dot_f16,
    },
    port::{MemoryStore, Reply},
};
use sqlx::Row;

macro_rules! reply {
    ($fut:expr) => {
        Box::pin(async move { $fut.await.map_err(Fail::into_core) })
    };
}

async fn explicit_memory_call_ran(tx: &mut crate::db::Tx<'_>, profile: &str, run: &str) -> R<bool> {
    let ran: bool = sqlx::query_scalar(
        "select exists(
            select 1 from bot_calls
            where profile = $1 and run = $2 and tool in ($3, $4) and state = 'done'
         )",
    )
    .bind(profile)
    .bind(run)
    .bind(MEMORY_SAVE_TOOL)
    .bind(MEMORY_FORGET_TOOL)
    .fetch_one(&mut **tx)
    .await?;
    Ok(ran)
}

async fn eligible_owner_run(
    tx: &mut crate::db::Tx<'_>,
    profile: &str,
    run: &str,
) -> R<(String, String)> {
    let row = sqlx::query(
        "select parent, principal, agent, conv_kind, run_origin from bot_runs where profile = $1 and id = $2",
    )
    .bind(profile)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    let parent: Option<String> = row.get("parent");
    let principal: String = row.get("principal");
    let agent: String = row.get("agent");
    let conv_kind: String = row.get("conv_kind");
    let origin: String = row.get("run_origin");
    if parent.is_some()
        || origin != "message"
        || !PrincipalId(principal.clone()).is_human()
        || !matches!(conv_kind.as_str(), "direct" | "cli")
    {
        return Err(bot_core::port::Error::Denied.into());
    }
    Ok((principal, agent))
}

impl Store {
    pub(crate) async fn memory_auto_save(
        &self,
        at: &Attempt,
        checked_revision: &Revision,
        spec: MemorySave,
    ) -> R<Option<MemorySaved>> {
        ids_ok(&[&checked_revision.0])?;
        text_ok(&spec.text)?;
        if let Some(old) = &spec.supersedes {
            ids_ok(&[&old.0])?;
        }
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &at.profile.0, checked_revision).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(bot_core::port::Error::Denied.into());
        }
        let (principal, agent) = eligible_owner_run(&mut tx, &run.profile, &run.id).await?;
        if explicit_memory_call_ran(&mut tx, &run.profile, &run.id).await? {
            tx.rollback().await?;
            return Ok(None);
        }

        lock_owner_quota(&mut tx, &run.profile, &principal).await?;

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
            if let Some(old_id) = &spec.supersedes {
                let old = sqlx::query(
                    "select agent, owner from bot_memories
                     where profile = $1 and id = $2 and visibility = 'private'
                       and deleted_ms is null and superseded_by is null for update",
                )
                .bind(&run.profile)
                .bind(&old_id.0)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(bot_core::port::Error::Missing)?;
                if old.get::<String, _>("owner") != principal
                    || !lineage(&agent).contains(&old.get::<String, _>("agent"))
                {
                    return Err(bot_core::port::Error::Denied.into());
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
                    return Err(bot_core::port::Error::Denied.into());
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

        let mut entry = Entry::new(&principal, "memory.auto_save", &saved.id.0, verdict);
        entry.args = Some(sha256_text(&spec.text));
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(Some(saved))
    }

    pub(crate) async fn memory_auto_forget(
        &self,
        at: &Attempt,
        checked_revision: &Revision,
        id: &MemoryId,
    ) -> R<bool> {
        ids_ok(&[&checked_revision.0, &id.0])?;
        let mut tx = self.begin_authorized().await?;
        crate::active::guard_active(&mut tx, &at.profile.0, checked_revision).await?;
        let run = lock_leased(&mut tx, at).await?;
        if run.private || run.cancel_requested {
            return Err(bot_core::port::Error::Denied.into());
        }
        let (principal, agent) = eligible_owner_run(&mut tx, &run.profile, &run.id).await?;
        if explicit_memory_call_ran(&mut tx, &run.profile, &run.id).await? {
            tx.rollback().await?;
            return Ok(false);
        }
        let Some(row) = sqlx::query(
            "select agent, owner from bot_memories
             where profile = $1 and id = $2 and visibility = 'private' and deleted_ms is null for update",
        )
        .bind(&run.profile)
        .bind(&id.0)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.rollback().await?;
            return Ok(false);
        };
        if row.get::<String, _>("owner") != principal
            || !lineage(&agent).contains(&row.get::<String, _>("agent"))
        {
            return Err(bot_core::port::Error::Denied.into());
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
        let entry = Entry::new(&principal, "memory.auto_forget", &id.0, "ok");
        audit::append(&mut tx, &run.profile, &entry).await?;
        tx.commit().await?;
        Ok(true)
    }
}

impl MemoryStore for Store {
    fn preview(&self, attempt: Attempt, id: MemoryId) -> Reply<'_, MemoryPreview> {
        reply!(async move { self.memory_preview(&attempt, &id).await })
    }

    fn share(&self, start: StartCall, id: MemoryId) -> Reply<'_, MemoryId> {
        reply!(self.memory_share(start, id))
    }

    fn search(&self, attempt: Attempt, query: String) -> Reply<'_, Vec<MemoryHit>> {
        reply!(async move { self.memory_search(&attempt, &query).await })
    }

    fn search_hybrid(&self, attempt: Attempt, query: MemoryQuery) -> Reply<'_, Vec<MemoryHit>> {
        reply!(async move { self.memory_search_hybrid(&attempt, &query).await })
    }

    fn change(&self, attempt: Attempt, change: MemoryChange) -> Reply<'_, MemoryId> {
        reply!(async move { self.memory_change(&attempt, change).await })
    }

    fn save(&self, start: StartCall, spec: MemorySave) -> Reply<'_, MemorySaved> {
        reply!(self.memory_save(start, spec))
    }

    fn forget(&self, start: StartCall, id: MemoryId) -> Reply<'_, MemoryId> {
        reply!(self.memory_forget(start, id))
    }

    fn unembedded(
        &self,
        profile: ProfileId,
        model: String,
        dim: u32,
        limit: u32,
    ) -> Reply<'_, Vec<UnembeddedMemory>> {
        reply!(async move { self.memory_unembedded(&profile, &model, dim, limit).await })
    }

    fn store_embedding(
        &self,
        profile: ProfileId,
        id: MemoryId,
        body_sha256: [u8; 32],
        embedding: MemoryEmbedding,
    ) -> Reply<'_, ()> {
        reply!(async move {
            self.memory_store_embedding(&profile, &id, body_sha256, &embedding)
                .await
        })
    }

    fn auto_save(
        &self,
        attempt: Attempt,
        checked_revision: Revision,
        spec: MemorySave,
    ) -> Reply<'_, Option<MemorySaved>> {
        reply!(async move {
            self.memory_auto_save(&attempt, &checked_revision, spec)
                .await
        })
    }

    fn auto_forget(
        &self,
        attempt: Attempt,
        checked_revision: Revision,
        id: MemoryId,
    ) -> Reply<'_, bool> {
        reply!(async move {
            self.memory_auto_forget(&attempt, &checked_revision, &id)
                .await
        })
    }

    fn seed(&self, seed: MemorySeed) -> Reply<'_, ()> {
        reply!(self.memory_seed(seed))
    }
}
