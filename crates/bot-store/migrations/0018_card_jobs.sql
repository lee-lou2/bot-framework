-- 소유자 DM 승인 카드 전달 작업(docs/reliability.md «Slack 소유자에게 승인 카드를 전달할 때»).
-- 채널 출처의 «명시 목적지 생성»·«전송 초안» 승인은 같은 커밋에 승인 행(pending)과 이 작업을 함께 만든다. 작업이 `posted`가 되기 전의 카드는
-- 열리지 않은 카드라 결정·목록·클릭 대상이 아니다. 게시를 확인한 영수증(팀·DM 채널·메시지 ts)이 한 커밋에서 카드를 연다.
--   staged → leased(점유) → sending(최초 전송자만) → posted | closed
--   leased는 리스가 끝나면 다시 점유되고, sending은 자동으로 다시 점유되지 않는다(게시 여부 불명이면 닫는다)
create table bot_card_jobs (
    nonce text primary key references bot_approvals (nonce) on delete cascade,
    profile text not null,
    run text not null,
    -- 원래 입력 바인딩: 카드를 올릴 Slack 설치를 정한다
    binding text not null,
    -- 승인자(= 요청자)의 사람 신원
    owner text not null,
    -- 승인을 요청한 시도의 세대(정상 재개에서는 바뀌지 않는다)
    generation bigint not null,
    state text not null default 'staged' check (state in ('staged', 'leased', 'sending', 'posted', 'closed')),
    -- 점유 횟수(다시 시도한 만큼 늘고, 한도를 넘으면 닫는다)
    attempts int not null default 0,
    not_before_ms bigint not null default 0,
    lease_until_ms bigint,
    sending_ms bigint,
    -- begin이 확인한 호스트 활성 집합 판본과, 게이트웨이가 확인한 프로필 판본
    activation text,
    checked_revision text,
    -- 게시를 확인한 영수증
    team text,
    dm text,
    ts text,
    posted_ms bigint,
    closed_reason text check (closed_reason in ('unavailable', 'held', 'stale')),
    created_ms bigint not null default bot_now_ms(),
    updated_ms bigint not null default bot_now_ms(),
    check ((state = 'posted') = (team is not null and dm is not null and ts is not null and posted_ms is not null)),
    check ((team is null) = (dm is null) and (dm is null) = (ts is null)),
    check ((state = 'closed') = (closed_reason is not null)),
    check (state <> 'leased' or lease_until_ms is not null),
    check (state <> 'sending' or (sending_ms is not null and activation is not null and checked_revision is not null))
);
create index bot_card_jobs_ready on bot_card_jobs (created_ms, nonce) where state in ('staged', 'leased');
create index bot_card_jobs_sending on bot_card_jobs (sending_ms) where state = 'sending';

-- 미발송 종결의 사유에 «승인 카드를 소유자 DM으로 전달하지 못함»을 더한다
alter table bot_deliveries drop constraint bot_deliveries_unsent_check;
alter table bot_deliveries
    add constraint bot_deliveries_unsent_check check (unsent in ('rejected', 'unrouted', 'dm_unavailable'));
