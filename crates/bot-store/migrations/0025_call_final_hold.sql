-- 미확정 단계가 있으면 각 단계가 판정 대상이다. 단계가 모두 정해졌지만 최종 응답을 잃은 호출은 자기 발생을 갖는다.
create or replace function bot_calls_hold() returns trigger language plpgsql as $$
begin
    -- 구판 관리자가 마지막 단계 판정을 호출 전체 미실행으로 확대하는 쓰기도 같은 트랜잭션에서 거절한다.
    if tg_table_name = 'bot_calls' and new.state = 'held' and new.resolution = 'not_executed'
       and exists (select 1 from bot_effects e where e.profile = new.profile and e.run = new.run and e.call = new.call
                   and (e.state in ('started', 'confirmed') or (e.state = 'held' and e.resolution is null))) then
        raise check_violation using message = '처리됐거나 미결인 단계가 있는 호출은 미실행으로 판정할 수 없다';
    end if;
    if exists (select 1 from bot_calls c where c.profile = new.profile and c.run = new.run and c.call = new.call
               and c.state = 'held' and c.resolution is null)
       and not exists (select 1 from bot_effects e where e.profile = new.profile and e.run = new.run and e.call = new.call
                       and (e.state = 'started' or (e.state = 'held' and e.resolution is null))) then
        perform bot_hold_open(new.profile, new.run, new.call, null, 'tool');
    else
        perform bot_hold_close(new.profile, new.run, new.call, null, 'tool');
    end if;
    return null;
end $$;

drop trigger bot_calls_hold on bot_calls;
create trigger bot_calls_hold after update of state, resolution on bot_calls for each row
    when ((old.state, old.resolution) is distinct from (new.state, new.resolution)
          and (old.state = 'held' or new.state = 'held'))
    execute function bot_calls_hold();

-- 마지막 단계의 늦은 영수증이나 운영자 판정도 호출 보류를 같은 커밋에서 연다.
create trigger bot_effects_call_hold after update of state, resolution on bot_effects for each row
    when ((old.state,old.resolution) is distinct from (new.state,new.resolution)
          and (old.state in ('started', 'held') or new.state in ('started', 'held')))
    execute function bot_calls_hold();

-- 구판은 마지막 단계 판정 때 확정 영수증이 있어도 호출 전체를 미실행으로 표시했다.
-- 아직 보류 중인 호출만 최종 판정 대기로 되돌리고 이미 커밋된 단계·운영자 증거는 보존한다.
with corrected as (
    update bot_calls c set resolution = null, receipt = null
    where c.state = 'held' and c.resolution = 'not_executed'
      and exists (select 1 from bot_effects e where e.profile = c.profile and e.run = c.run and e.call = c.call and e.state = 'confirmed')
    returning c.profile, c.run
)
update bot_runs r set state = 'waiting', wait_kind = 'hold', wait_child = null,
       lease_owner = null, lease_until_ms = null, updated_ms = bot_now_ms()
from corrected c where (r.profile, r.id) = (c.profile, c.run)
  and r.state in ('ready', 'retry', 'leased') and not r.cancel_requested and not r.private;

-- 단계가 모두 끝나 최초부터 발생조차 없었던 구판 호출에는 원래 보류 시각을 사용한다.
insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
select 'hold-' || nextval('bot_hold_seq'), c.profile, c.run, c.call, null, 'tool', c.updated_ms
from bot_calls c
where c.state = 'held' and c.resolution is null
  and not exists (select 1 from bot_runs r where r.profile = c.profile and r.id = c.run and (r.private or r.state = 'cancelled'))
  and not exists (select 1 from bot_effects e where e.profile = c.profile and e.run = c.run and e.call = c.call
                  and (e.state = 'started' or (e.state = 'held' and e.resolution is null)))
  and not exists (select 1 from bot_holds h where h.profile = c.profile and h.run = c.run and h.call = c.call
                  and h.kind = 'tool' and h.effect is null and h.closed_ms is null);

-- 구판에서 취소한 실행은 더 이상 판정할 수 없다. 보류의 발생·증거 기록은 보존한다.
update bot_holds h set closed_ms = coalesce(r.finished_ms, bot_now_ms())
from bot_runs r where (r.profile, r.id) = (h.profile, h.run)
  and r.state = 'cancelled' and h.kind = 'tool' and h.closed_ms is null;
