-- 접수·실행(큐)·체크포인트·감사 체인. 모든 키는 profile로 시작한다.
-- 시각은 epoch ms이고 DB 시계(bot_now_ms)로만 잰다. 호스트 시계는 판정에 쓰지 않는다.

create function bot_now_ms() returns bigint
    language sql volatile
    as $$ select floor(extract(epoch from clock_timestamp()) * 1000)::bigint $$;

-- 실행 하나. 루트 run은 root = id, 위임한 자식은 parent·parent_step을 가진다.
-- generation은 claim마다 오르는 펜싱 토큰이다. 리스(lease_until_ms)가 끝난 소유자의 쓰기는 거절된다.
create table bot_runs (
    profile text not null,
    id text not null default gen_random_uuid()::text,
    root text not null,
    parent text,
    parent_step text,
    depth int not null default 0 check (depth between 0 and 8),
    agent text not null,
    principal text not null,
    -- 승인자 지정(없으면 요청자). 예약은 트리거가 지정한 사람
    approver text,
    scopes text[] not null default '{}',
    revision text not null,
    -- 예약·이벤트 트리거 ID(관리 화면 표시용)
    trigger text,
    -- 원래 목적지: 접수 때 고정하고 이후 바꾸지 않는다
    binding text not null,
    conv_kind text not null check (conv_kind in ('direct', 'channel', 'cli', 'http', 'scheduled')),
    conv_id text not null,
    thread text,
    attachments text[] not null default '{}',
    target jsonb not null check (jsonb_typeof(target) = 'object'),
    input text not null,
    state text not null default 'ready'
        check (state in ('ready', 'leased', 'waiting', 'retry', 'succeeded', 'dead', 'cancelled')),
    wait_kind text check (wait_kind in ('child', 'approval', 'hold')),
    wait_child text,
    generation bigint not null default 0,
    -- claim(정상 재개 포함)마다 오르는 실행 토큰. 세대가 같아도 옛 소유자의 접근을 막는다
    claim_epoch bigint not null default 0,
    -- 실패·리스 만료로 소모한 횟수. 승인·보류 대기와 정상 재개는 세지 않는다
    attempts int not null default 0,
    lease_owner text,
    lease_until_ms bigint,
    not_before_ms bigint not null default 0,
    cancel_requested boolean not null default false,
    -- 같은 키의 run은 동시에 하나만 실행한다(beat)
    serial_key text,
    -- 요청 트리 전체의 플래그. 루트 행에서만 읽는다
    private_memory_read boolean not null default false,
    -- 민감 DM 접수 기록(본문 없음). 워커가 집지 않고 목록·이력에도 내용이 없다
    private boolean not null default false,
    -- 자발적 대기(승인·자식·보류)에서 깨어난 run은 같은 세대로 재개한다. 인계·재시도·dead letter 재개는 세대를 올린다
    keep_generation boolean not null default false,
    result text,
    error text,
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    finished_ms bigint,
    primary key (profile, id),
    foreign key (profile, root) references bot_runs (profile, id),
    foreign key (profile, parent) references bot_runs (profile, id),
    check ((state = 'waiting') = (wait_kind is not null)),
    check (wait_child is null or wait_kind = 'child'),
    check (wait_kind is distinct from 'child' or wait_child is not null),
    check ((state = 'leased') = (lease_until_ms is not null))
);

create index bot_runs_ready on bot_runs (not_before_ms, created_ms, id) where state in ('ready', 'retry') and not private;
create index bot_runs_leased on bot_runs (lease_until_ms, created_ms, id) where state = 'leased' and not private;
create index bot_runs_root on bot_runs (profile, root);
create index bot_runs_waiting_child on bot_runs (profile, wait_child) where state = 'waiting' and wait_kind = 'child';
create index bot_runs_conversation on bot_runs (profile, binding, conv_kind, conv_id, thread, created_ms);
create unique index bot_runs_child_step on bot_runs (profile, parent, parent_step) where parent is not null;
create unique index bot_runs_serial_running on bot_runs (profile, serial_key)
    where state = 'leased' and serial_key is not null;

-- 외부 접수의 유일 키. 같은 키·같은 입력 해시는 기존 run, 다른 해시는 충돌이다.
-- run보다 먼저 넣을 수 있도록 외래 키 검사는 커밋 시점으로 미룬다.
create table bot_requests (
    profile text not null,
    binding text not null,
    principal text not null,
    request text not null,
    input_sha256 bytea not null check (octet_length(input_sha256) = 32),
    run text not null,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, binding, principal, request),
    foreign key (profile, run) references bot_runs (profile, id) deferrable initially deferred
);
create index bot_requests_run on bot_requests (profile, run);

-- 재개용 체크포인트: 모델 step·도구 결과·자식 결과. 같은 key는 한 번만 기록한다.
create table bot_steps (
    profile text not null,
    run text not null,
    key text not null,
    seq bigint generated always as identity,
    body text not null,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, run, key),
    foreign key (profile, run) references bot_runs (profile, id) on delete cascade
);
create index bot_steps_order on bot_steps (profile, run, seq);

-- 감사 체인: 호스트 전체에 순번이 1부터 하나로 이어지고, 각 행은 이전 해시를 포함해 SHA-256으로 묶인다.
-- 상태를 바꾸는 트랜잭션이 같은 트랜잭션 안에서 head를 잠그고 마지막에 추가한다. profile은 표시·필터용이고 해시에 들어간다.
create table bot_audit_head (
    singleton boolean primary key default true check (singleton),
    seq bigint not null,
    hash bytea not null check (octet_length(hash) = 32)
);

create table bot_audit (
    seq bigint primary key check (seq >= 1),
    profile text,
    at_ms bigint not null,
    actor text not null,
    action text not null,
    target text not null,
    args_sha256 bytea check (args_sha256 is null or octet_length(args_sha256) = 32),
    verdict text not null,
    prev_hash bytea not null check (octet_length(prev_hash) = 32),
    hash bytea not null check (octet_length(hash) = 32)
);
create index bot_audit_profile on bot_audit (profile, seq);
