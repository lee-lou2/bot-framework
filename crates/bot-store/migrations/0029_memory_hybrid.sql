-- Hybrid memory retrieval (normalized IEEE-754 f16 embeddings + lexical/trigram scoring),
-- memory kinds (fact/core), and non-destructive superseding (superseded_by / valid_until_ms).
-- Uses stock PostgreSQL core features only (no extensions or superuser privileges required).

alter table bot_memories
    add column kind text not null default 'fact' check (kind in ('fact', 'core')),
    add column superseded_by text,
    add column valid_until_ms bigint,
    add column embedding_model text,
    add column embedding_dim integer check (embedding_dim is null or (embedding_dim >= 1 and embedding_dim <= 4096)),
    add column embedding_f16 bytea,
    add constraint bot_memories_embedding_consistent check (
        (embedding_f16 is null) = (embedding_model is null)
        and (embedding_f16 is null) = (embedding_dim is null)
        and (embedding_f16 is null or octet_length(embedding_f16) = embedding_dim * 2)
    );

create index bot_memories_active on bot_memories (profile, agent, kind)
    where deleted_ms is null and superseded_by is null;
create index bot_memories_owner_active on bot_memories (profile, owner)
    where deleted_ms is null and superseded_by is null;
