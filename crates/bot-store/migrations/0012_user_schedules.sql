-- 사용자 예약: 대화에서 만든 at(한 번)·cron(반복) 예약. 정적 트리거(0003)와 따로 둔다 — 정의가 DB에 있고 소유자가 만들고 취소한다.
-- 신원(소유자·에이전트·원래 바인딩·대화·권한 상한·private 표지)은 만든 run에서 저장소가 복원해 적는다(호출자가 정하지 않는다).
-- 시각은 모두 DB 시계다. 발화(발생)는 (profile, 예약, 예정 UTC 시각)에 유일하고, 같은 발생은 같은 run으로 수렴한다.

create table bot_user_schedules (
    profile text not null,
    id text not null,
    owner text not null,
    agent text not null,
    -- 만든 호출: 한 호출은 예약 하나만 만든다(재전달은 같은 예약)
    origin_run text not null,
    origin_call text not null,
    -- 원래 대화·바인딩: 발화한 run이 이 대화로 돌아가 기억 가시성과 승인 경로를 정한다
    origin_binding text not null,
    origin_kind text not null check (origin_kind in ('direct', 'channel', 'cli', 'scheduled')),
    origin_id text not null,
    origin_thread text,
    -- 만든 요청 트리가 private 기억을 읽었다(발화한 run이 같은 표지로 시작한다)
    origin_private_read boolean not null default false,
    approver text,
    -- 만든 시점 요청자 권한의 상한. 발화마다 그때의 권한과 교집합을 쓴다
    ceiling text[] not null default '{}',
    -- 명시 목적지(원래 대화가 아닌 수신 경로). 없으면 원래 대화다
    dest_binding text,
    dest_kind text,
    dest_id text,
    dest_thread text,
    at_ms bigint,
    cron text,
    timezone text,
    instruction text not null check (octet_length(instruction) between 1 and 8192),
    quiet boolean not null default false,
    -- 자동 후속 확인의 연쇄 깊이(사용자가 만든 것은 0)
    wake_depth smallint not null default 0 check (wake_depth between 0 and 8),
    state text not null default 'active' check (state in ('active', 'canceled', 'closed')),
    close_reason text,
    -- 이 시각 이하의 발생은 접수를 마쳤다(처음에는 만든 시각). 발화와 같은 커밋으로만 앞으로 간다
    cursor_ms bigint not null,
    -- 다음에 확인할 시각. 활성 예약만 갖는다
    next_ms bigint,
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    primary key (profile, id),
    foreign key (profile, origin_run) references bot_runs (profile, id),
    check ((at_ms is not null) <> (cron is not null)),
    check ((cron is null) = (timezone is null)),
    check ((dest_binding is null) = (dest_kind is null) and (dest_kind is null) = (dest_id is null)),
    check ((state = 'active') = (next_ms is not null))
);
create unique index bot_user_schedules_call on bot_user_schedules (profile, origin_run, origin_call);
create index bot_user_schedules_due on bot_user_schedules (next_ms, profile, id) where state = 'active';
create index bot_user_schedules_owner on bot_user_schedules (profile, owner, id) where state = 'active';

-- 발화 한 건 = (profile, 예약, 예정 UTC instant). 스케줄러가 여럿이어도 같은 키로 수렴한다.
create table bot_user_occurrences (
    profile text not null,
    schedule text not null,
    at_ms bigint not null,
    run text not null,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, schedule, at_ms),
    foreign key (profile, schedule) references bot_user_schedules (profile, id),
    foreign key (profile, run) references bot_runs (profile, id)
);
create index bot_user_occurrences_run on bot_user_occurrences (profile, run);

-- 예약 발화 run의 표지: 조용한 예약(성공 결과가 정확히 <조용히>이면 답을 보내지 않는다)과 자동 후속 확인의 연쇄 깊이
alter table bot_runs add column quiet boolean not null default false;
alter table bot_runs add column wake_depth smallint not null default 0;
