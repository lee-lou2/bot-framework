-- 주요 조회·청소·관리 목록 경로의 부분·복합 색인. 기존 색인은 건드리지 않는다.
create index bot_runs_private_active on bot_runs (lease_until_ms, profile, id)
    where private and state in ('leased', 'waiting');
create index bot_runs_updated on bot_runs (updated_ms desc, profile collate "C" asc, id collate "C" asc);
create index bot_calls_held_open on bot_calls (profile, run, updated_ms)
    where state = 'held' and resolution is null;
create index bot_card_jobs_closed on bot_card_jobs (profile, run)
    where state = 'closed';
create index bot_outbox_held on bot_outbox (updated_ms, run, seq)
    where state = 'held';
