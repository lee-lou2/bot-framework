-- 회신 발송 예약(outbox). 결과와 같은 커밋으로 쌓이고, 게이트웨이가 별도 발송 세대로 점유해 한 번 보낸다.
-- pending → sending(begin 커밋 뒤에만 전송) → confirmed | rejected | held. sending에서 응답을 잃으면 held로 둔다.
-- 같은 발송 키(profile, run, seq)는 워커가 끝난 뒤에도 유지된다.

create table bot_outbox (
    profile text not null,
    run text not null,
    seq int not null check (seq >= 0),
    -- 재시도에도 고정되는 호출 ID
    call text not null,
    binding text not null,
    principal text not null,
    conv_kind text not null,
    conv_id text not null,
    thread text,
    body text not null,
    state text not null default 'pending'
        check (state in ('pending', 'sending', 'confirmed', 'rejected', 'held')),
    -- 점유(claim)마다 오르는 발송 세대. run의 세대와 무관하다
    generation bigint not null default 0,
    lease_until_ms bigint,
    -- 미실행이 확정된 실패(NotSent)로 쓴 재시도 횟수와 다음 점유 가능 시각
    attempts int not null default 0,
    not_before_ms bigint not null default 0,
    receipt text,
    -- 보류 사유 코드(고정 어휘)
    reason text,
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    primary key (profile, run, seq),
    foreign key (profile, run) references bot_runs (profile, id) on delete cascade
);

create index bot_outbox_pending on bot_outbox (created_ms, profile, run, seq) where state = 'pending';
create index bot_outbox_sending on bot_outbox (lease_until_ms) where state = 'sending';
