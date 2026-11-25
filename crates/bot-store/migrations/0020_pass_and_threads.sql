-- F3: 직접 도구가 «처리하지 않음»(`bot/pass`)으로 넘길 수 있는 후보 사슬과 커서. 접수 한 행(루트 run)에 하나만 있다.
-- `pass_chain`은 접수가 order·매치 순으로 정한 후보(트리거 ID와 실행 대상)와 기본 대화이고, `pass_pos`는 지금 차례인 후보 위치다(후보 수와 같으면 기본 대화 차례).
-- 커서는 호출 결론·감사와 같은 커밋에서만 앞으로 간다(되돌아가지 않는다).
alter table bot_runs
    add column pass_chain jsonb,
    add column pass_pos integer,
    add constraint bot_runs_pass_both check ((pass_chain is null) = (pass_pos is null)),
    add constraint bot_runs_pass_pos_range check (pass_pos is null or pass_pos >= 0);

-- F2: 봇이 참여했다고 확인된 Slack 스레드(`thread_reply`의 입장 증거). 확인된 게시 영수증이나 인증된 자기 봇 메시지만 기록한다.
-- 키에 팀과 봇이 들어 있어 설치(워크스페이스·봇)가 바뀌면 다른 소유로 본다.
create table bot_slack_threads (
    profile text not null,
    binding text not null,
    team text not null,
    bot text not null,
    channel text not null,
    root_ts text not null,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, binding, team, bot, channel, root_ts)
);
