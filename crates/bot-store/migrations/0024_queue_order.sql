-- 최초 접수와 ready 재진입은 그때부터 기다린다. 구판 작성자가 재진입 때 0을 써도 같은 시각을 기록한다.
alter table bot_runs alter column not_before_ms set default bot_now_ms();
create function bot_runs_enter_ready() returns trigger language plpgsql as $$
begin
    new.not_before_ms := bot_now_ms();
    return new;
end
$$;
create trigger bot_runs_enter_ready before update of state on bot_runs
    for each row when (not new.private and new.state = 'ready' and old.state <> 'ready')
    execute function bot_runs_enter_ready();

-- 구판 ready의 진입 시각은 알 수 없어 생성 시각을 쓴다. leased 상태에만 리스 만료 시각이 있다.
create index bot_runs_eligible on bot_runs
    ((case when state = 'ready' and not_before_ms = 0 then created_ms
           else coalesce(lease_until_ms, not_before_ms) end), created_ms, id)
    where not private and state in ('ready', 'retry', 'leased');
