-- 예약: 구간(segment)별 정의, 커서, 발생 기록. cron 계산은 runtime이 하고 저장소는 구간·커서·중복 방지를 맡는다.
-- 구간 (from, until]: until이 비면 현재 구간이다. 판본이 바뀌면 적용 시각으로 구간을 나눈다.

create table bot_schedule_segments (
    profile text not null,
    agent text not null,
    trigger text not null,
    from_ms bigint not null,
    until_ms bigint,
    revision text not null,
    cron text not null,
    timezone text not null,
    -- 상태 확인용. 발생 생략·합치기를 허용하는 유일한 종류다
    beat boolean not null default false,
    principal text not null,
    approver text,
    scopes text[] not null default '{}',
    target jsonb not null,
    primary key (profile, agent, trigger, from_ms),
    check (until_ms is null or until_ms > from_ms)
);
create unique index bot_schedule_open on bot_schedule_segments (profile, agent, trigger) where until_ms is null;

-- 이 시각 이하의 발생은 접수를 마쳤다. 발생 접수와 같은 커밋으로만 앞으로 간다.
create table bot_schedule_cursors (
    profile text not null,
    agent text not null,
    trigger text not null,
    cursor_ms bigint not null,
    primary key (profile, agent, trigger)
);

-- 발생 한 건 = (profile, agent, trigger, 예정 UTC instant). 리더가 바뀌거나 scheduler가 여럿이어도 같은 키로 수렴한다.
create table bot_occurrences (
    profile text not null,
    agent text not null,
    trigger text not null,
    at_ms bigint not null,
    run text not null,
    -- beat가 이미 기다리는 run에 합쳐졌다
    coalesced boolean not null default false,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, agent, trigger, at_ms),
    foreign key (profile, run) references bot_runs (profile, id)
);
create index bot_occurrences_run on bot_occurrences (profile, run);
