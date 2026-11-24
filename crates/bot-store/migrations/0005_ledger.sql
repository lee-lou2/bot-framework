-- 호출 원장·승인 원장·효과(단계) 원장. 모든 전이는 세대·리스를 대조한 같은 트랜잭션에서 하고 감사 체인에 한 줄을 잇는다.
-- 호출 ID는 재시도·재개에도 같다: (profile, run, call)이 유일하다.

-- 호출 하나: open → started → done | held, open → denied, held(판정) → done | open(미실행 입증)
create table bot_calls (
    profile text not null,
    run text not null,
    call text not null,
    tool text not null,
    args_sha256 bytea not null check (octet_length(args_sha256) = 32),
    state text not null default 'open' check (state in ('open', 'started', 'done', 'denied', 'held')),
    -- open일 때 가장 최근 승인(nonce)
    approval text,
    -- 시작한 세대와 소유 표지. held 뒤에도 복구용으로 보존한다
    started_generation bigint,
    owner text,
    result text,
    -- held 사유(고정 어휘 코드)
    reason text,
    -- 운영자 판정: executed(영수증) | not_executed(미실행 증거)
    resolution text check (resolution in ('executed', 'not_executed')),
    receipt text,
    -- 관리 화면 표시용(승인이 있을 때만 알 수 있다)
    risk text check (risk in ('read', 'write', 'irreversible')),
    tool_revision text,
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    started_ms bigint,
    finished_ms bigint,
    primary key (profile, run, call),
    foreign key (profile, run) references bot_runs (profile, id) on delete cascade,
    check (state <> 'started' or (started_generation is not null and owner is not null))
);
create index bot_calls_held on bot_calls (profile, run) where state = 'held';

-- 승인 한 건. nonce는 일회용이며 전역에서 유일하다(조회 키가 nonce 하나다).
create table bot_approvals (
    nonce text primary key,
    profile text not null,
    run text not null,
    -- 승인을 요청한 시도의 세대. 정상 재개(같은 세대)에서는 바뀌지 않고, 인계면 승인이 무효가 된다
    generation bigint not null,
    call text not null,
    requester text not null,
    approver text not null,
    risk text not null check (risk in ('read', 'write', 'irreversible')),
    policy_revision text not null,
    tool text not null,
    tool_revision text not null,
    args_sha256 bytea not null check (octet_length(args_sha256) = 32),
    expires_ms bigint not null,
    reasons text[] not null default '{}',
    -- 기억 공유(memory.share) 승인이 카드에 보인 항목과 본문 해시. 공유 커밋에서 현재 본문과 대조한다
    memory_id text,
    memory_sha256 bytea check (memory_sha256 is null or octet_length(memory_sha256) = 32),
    card text not null,
    state text not null default 'pending'
        check (state in ('pending', 'approved', 'consumed', 'denied', 'expired', 'superseded', 'revoked')),
    decided_by text,
    decided_ms bigint,
    created_ms bigint not null default bot_now_ms(),
    foreign key (profile, run, call) references bot_calls (profile, run, call) on delete cascade
);
create index bot_approvals_run on bot_approvals (profile, run, state);
create index bot_approvals_expiry on bot_approvals (expires_ms) where state in ('pending', 'approved');

-- 여러 번 쓰는 도구의 단계 원장: started → confirmed | rejected | held
create table bot_effects (
    profile text not null,
    id text not null,
    run text not null,
    call text not null,
    step text not null,
    args_sha256 bytea not null check (octet_length(args_sha256) = 32),
    state text not null default 'started' check (state in ('started', 'confirmed', 'rejected', 'held')),
    -- 이 단계를 시작한 시도의 세대. 늦은 세대의 갱신은 거절한다
    generation bigint not null,
    receipt text,
    -- held 사유 코드
    reason text,
    resolution text check (resolution in ('executed', 'not_executed')),
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    primary key (profile, id),
    unique (profile, run, call, step),
    foreign key (profile, run, call) references bot_calls (profile, run, call) on delete cascade
);
create index bot_effects_call on bot_effects (profile, run, call, created_ms);
