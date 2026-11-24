-- 명시 목적지(다른 수신자) 예약 발화의 회신 초안: 발화 run마다 하나.
-- 목적지는 발화를 접수한 커밋에서 예약 정의로부터 고정한다(예약을 취소해도 이미 접수한 실행의 목적지는 바뀌지 않는다).
-- 초안(최종 결과·순서 있는 답 원문·호스트 활성 집합 판본)은 한 번 동결하면 바꾸지 않는다. 답은 JSON으로 다시 쓰지 않은 원문(text[])이라
-- 승인 문서와 SHA-256을 언제든 같은 바이트에서 다시 계산한다. 승인한 초안만 목적지의 outbox로 쌓이고 run이 끝난다.

create table bot_deliveries (
    profile text not null,
    run text not null,
    dest_binding text not null,
    dest_kind text not null check (dest_kind in ('direct', 'channel')),
    dest_id text not null,
    dest_thread text,
    -- pending: 발화만 접수 | drafted: 초안 동결(승인 대기) | enqueued: 승인한 초안을 목적지 outbox에 쌓고 run을 마쳤다 | dropped: 조용한 성공이라 보낼 것이 없다
    state text not null default 'pending' check (state in ('pending', 'drafted', 'enqueued', 'dropped')),
    activation text,
    result text,
    answers text[],
    sha256 bytea check (sha256 is null or octet_length(sha256) = 32),
    drafted_ms bigint,
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    primary key (profile, run),
    foreign key (profile, run) references bot_runs (profile, id) on delete cascade,
    check ((state in ('drafted', 'enqueued')) = (activation is not null and result is not null and answers is not null and sha256 is not null))
);
