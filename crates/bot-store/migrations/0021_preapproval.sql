-- v2.2 사전 승인(F1)과 지정한 사람의 승인 증거(F4).
-- 자격 값은 어떤 열에도 두지 않는다: 지문 재료(spec)는 이름·판본·해시만 담는다.

-- 사전 승인. 트리거마다 활성 승인은 하나이고, 철회된 행은 남겨 같은 ID가 되살아나지 않게 한다(바뀐 선언은 새 ID).
create table bot_grants (
    id text primary key,
    profile text not null,
    agent text not null,
    trigger text not null,
    -- 트리거별 발급 순번(1부터). 지워지지 않는다.
    seq bigint not null,
    fingerprint bytea not null check (octet_length(fingerprint) = 32),
    spec jsonb not null,
    tool text not null,
    tool_revision text not null,
    args_sha256 bytea not null check (octet_length(args_sha256) = 32),
    -- 정렬된 허용 쌍 [[operation, target], ...]
    pairs jsonb not null,
    max_per_day integer not null check (max_per_day between 1 and 65535),
    state text not null default 'active' check (state in ('active', 'revoked')),
    issued_ms bigint not null default bot_now_ms(),
    revoked_ms bigint,
    revoked_reason text,
    unique (profile, agent, trigger, seq)
);
create unique index bot_grants_active on bot_grants (profile, agent, trigger) where state = 'active';

-- 트리거 단위 정지 상태. 승인이 교체돼도 유지된다. epoch는 정지·재개마다 1씩 늘고 줄지 않는다.
create table bot_trigger_pause (
    profile text not null,
    agent text not null,
    trigger text not null,
    epoch bigint not null default 0,
    paused boolean not null default false,
    -- 마지막 전환의 직전 epoch·종류·승인 ID: 응답이 유실된 같은 명령의 재전달을 알아본다.
    last_from bigint,
    last_kind text check (last_kind in ('pause', 'resume')),
    last_grant text,
    -- 마지막 정지·재개 전환의 DB 시각(전환이 없으면 비어 있다). 이보다 앞서 예정된 발생은 접수하지 않는다.
    changed_ms bigint,
    updated_ms bigint not null default bot_now_ms(),
    primary key (profile, agent, trigger)
);

-- 일별 사용량. 키는 (트리거, UTC 일 번호). 재활성화·재개로 초기화되지 않는다.
create table bot_grant_usage (
    profile text not null,
    agent text not null,
    trigger text not null,
    day bigint not null,
    used integer not null default 0 check (used >= 0),
    primary key (profile, agent, trigger, day)
);

-- 접수 때 루트 run에 묶은 승인과 그때의 정지 세대.
create table bot_run_grants (
    profile text not null,
    run text not null,
    grant_id text not null references bot_grants (id),
    epoch bigint not null,
    primary key (profile, run),
    foreign key (profile, run) references bot_runs (profile, id)
);

-- 호출을 시작할 때 확인한 승인(없으면 비어 있다)과 새 쓰기를 허가한 승인.
alter table bot_calls add column grant_id text;
alter table bot_calls add column grant_epoch bigint;
alter table bot_effects add column grant_id text;

-- F4: 접수 때 루트 run에 묶은 인증된 이벤트 증거(본문·반응 대상 전문은 없다).
create table bot_run_evidence (
    profile text not null,
    run text not null,
    kind text not null check (kind in ('message', 'thread_reply', 'reaction')),
    team text not null,
    event_id text not null,
    binding text not null,
    channel text not null,
    item_ts text not null,
    thread_ts text,
    actor text not null,
    trigger text not null,
    primary key (profile, run),
    foreign key (profile, run) references bot_runs (profile, id)
);

-- 소비한 증거. (프로필, 팀, 이벤트)당 한 번만 승인을 대신한다.
create table bot_evidence_uses (
    profile text not null,
    team text not null,
    event_id text not null,
    run text not null,
    call text not null,
    tool text not null,
    tool_revision text not null,
    args_sha256 bytea not null,
    actor text not null,
    used_ms bigint not null default bot_now_ms(),
    primary key (profile, team, event_id)
);
