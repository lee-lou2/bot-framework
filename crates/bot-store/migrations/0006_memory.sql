-- 기억: 프로필·에이전트 범위, owner(인증된 요청자)와 visibility(private 기본, profile은 소유자 승인 뒤).
-- 삭제는 본문을 비운 표지(deleted_ms)로 남긴다. 시드를 다시 등록해도 지운 기억이 되살아나지 않게 하려는 것이다.

create table bot_memories (
    profile text not null,
    id text not null default gen_random_uuid()::text,
    -- 이 기억을 만든(또는 시드가 속한) 에이전트. 자기와 그 아래 에이전트가 읽는다
    agent text not null,
    owner text not null,
    visibility text not null default 'private' check (visibility in ('private', 'profile')),
    body text not null,
    -- 시드: 경로와 내용 해시로 한 번만 등록한다
    seed_path text,
    seed_sha256 bytea check (seed_sha256 is null or octet_length(seed_sha256) = 32),
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    deleted_ms bigint,
    primary key (profile, id),
    check ((seed_path is null) = (seed_sha256 is null))
);
create unique index bot_memories_seed on bot_memories (profile, agent, seed_path, seed_sha256) where seed_path is not null;
create index bot_memories_scope on bot_memories (profile, agent) where deleted_ms is null;
