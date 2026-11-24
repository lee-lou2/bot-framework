-- 활성 판본 집합: 호스트가 프로필 활성 집합의 판본을 원자적으로 게시한다. 발송 직전 검사(Outbox.begin)가 이 값을 읽는다.

create table bot_active (
    singleton boolean primary key default true check (singleton),
    revision text not null,
    updated_ms bigint not null default bot_now_ms()
);

create table bot_active_profiles (
    profile text primary key,
    revision text not null
);
