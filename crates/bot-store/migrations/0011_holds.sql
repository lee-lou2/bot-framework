-- 보류 발생(hold occurrence). 호출·단계·회신 발송·민감 DM이 보류로 "들어갈 때마다" 새 ID를 받는다.
-- 같은 보류를 유지하는 동안(사유만 바뀌거나 다시 읽을 때)에는 ID가 그대로이고, 판정이나 다른 경로로 보류를 벗어난 뒤
-- 다시 들어가면 새 ID다. 판정한 발생은 운영자·판정 종류·영수증/증거 지문(원문 아님)을 남겨, 같은 요청의 재전송을 Repeated로 알아본다.
-- 발급·종료는 각 원장의 state 전이에 걸린 트리거가 한다: 전이를 만드는 코드가 몇 군데든 "들어갈 때마다"가 빠지지 않는다.

create sequence bot_hold_seq;

create table bot_holds (
    id text primary key,
    profile text not null,
    run text not null,
    -- 호출 ID. 회신 발송은 reply:<run>:<seq>, 민감 DM은 private:<run>이다
    call text not null,
    -- 다단계 도구의 단계(effect) ID. 호출 단위 보류(단일 쓰기·회신·민감 DM)는 null이다
    effect text,
    kind text not null check (kind in ('tool', 'reply', 'private')),
    opened_ms bigint not null,
    -- 판정이나 다른 경로(늦게 온 확정 영수증 등)로 보류를 벗어난 시각. null이면 열린 보류다
    closed_ms bigint,
    -- 운영자 판정(있을 때만): 누가, 어떤 판정을, 어떤 영수증/증거로. 원문은 저장하지 않고 SHA-256만 둔다
    operator text,
    verdict text check (verdict in ('confirmed', 'not_executed')),
    evidence_sha256 bytea check (evidence_sha256 is null or octet_length(evidence_sha256) = 32),
    foreign key (profile, run) references bot_runs (profile, id) on delete cascade,
    check ((operator is null) = (verdict is null) and (verdict is null) = (evidence_sha256 is null)),
    check (verdict is null or closed_ms is not null)
);
-- 같은 대상에는 열린 보류가 하나뿐이다. 종류가 대상의 일부다: 구판에는 도구 호출 ID와 회신 발송 호출 ID가 같은 합법적인 행이 있을 수 있다.
create unique index bot_holds_open on bot_holds (profile, run, call, kind, (coalesce(effect, ''))) where closed_ms is null;
create index bot_holds_run on bot_holds (profile, run);

create function bot_hold_open(p_profile text, p_run text, p_call text, p_effect text, p_kind text) returns void
language plpgsql as $$
begin
    insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
    select 'hold-' || nextval('bot_hold_seq'), p_profile, p_run, p_call, p_effect, p_kind, bot_now_ms()
    where not exists (
        select 1 from bot_holds h
        where h.profile = p_profile and h.run = p_run and h.call = p_call and h.kind = p_kind
          and coalesce(h.effect, '') = coalesce(p_effect, '') and h.closed_ms is null
    );
end $$;

-- 운영자 판정은 먼저 자기 발생을 닫으므로(판정 정보와 함께) 여기서는 아직 열린 것만 판정 없이 닫는다
create function bot_hold_close(p_profile text, p_run text, p_call text, p_effect text, p_kind text) returns void
language plpgsql as $$
begin
    update bot_holds set closed_ms = bot_now_ms()
    where profile = p_profile and run = p_run and call = p_call and kind = p_kind
      and coalesce(effect, '') = coalesce(p_effect, '') and closed_ms is null;
end $$;

-- 단계 없는 호출만 호출 단위 발생을 갖는다. 단계가 있는 호출은 단계마다 따로 판정하므로 단계가 발생을 갖는다.
create function bot_calls_hold() returns trigger language plpgsql as $$
begin
    if new.state = 'held' then
        if not exists (select 1 from bot_effects e where e.profile = new.profile and e.run = new.run and e.call = new.call) then
            perform bot_hold_open(new.profile, new.run, new.call, null, 'tool');
        end if;
    else
        perform bot_hold_close(new.profile, new.run, new.call, null, 'tool');
    end if;
    return null;
end $$;
create trigger bot_calls_hold after update of state on bot_calls for each row
    when (old.state is distinct from new.state and (old.state = 'held' or new.state = 'held'))
    execute function bot_calls_hold();

create function bot_effects_hold() returns trigger language plpgsql as $$
begin
    if new.state = 'held' then
        perform bot_hold_open(new.profile, new.run, new.call, new.id, 'tool');
    else
        perform bot_hold_close(new.profile, new.run, new.call, new.id, 'tool');
    end if;
    return null;
end $$;
create trigger bot_effects_hold after update of state on bot_effects for each row
    when (old.state is distinct from new.state and (old.state = 'held' or new.state = 'held'))
    execute function bot_effects_hold();

create function bot_outbox_hold() returns trigger language plpgsql as $$
begin
    if new.state = 'held' then
        perform bot_hold_open(new.profile, new.run, new.call, null, 'reply');
    else
        perform bot_hold_close(new.profile, new.run, new.call, null, 'reply');
    end if;
    return null;
end $$;
create trigger bot_outbox_hold after update of state on bot_outbox for each row
    when (old.state is distinct from new.state and (old.state = 'held' or new.state = 'held'))
    execute function bot_outbox_hold();

-- 민감 DM 접수는 run이 보류 대기(waiting/hold)로 들어갈 때가 보류다
create function bot_runs_hold() returns trigger language plpgsql as $$
begin
    if new.state = 'waiting' and new.wait_kind = 'hold' then
        perform bot_hold_open(new.profile, new.id, 'private:' || new.id, null, 'private');
    else
        perform bot_hold_close(new.profile, new.id, 'private:' || new.id, null, 'private');
    end if;
    return null;
end $$;
create trigger bot_runs_hold after update of state, wait_kind on bot_runs for each row
    when (new.private and (old.state, old.wait_kind) is distinct from (new.state, new.wait_kind)
          and ((new.state = 'waiting' and new.wait_kind = 'hold') or (old.state = 'waiting' and old.wait_kind = 'hold')))
    execute function bot_runs_hold();

-- 이전 판의 미결 보류에 발생 ID를 준다. 운영자 목록이 보여 주던 것(판정 전의 호출·단계·회신·민감 DM)과 같은 조건이다.
insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
select 'hold-' || nextval('bot_hold_seq'), c.profile, c.run, c.call, null, 'tool', c.updated_ms
from bot_calls c
where c.state = 'held' and c.resolution is null
  and not exists (select 1 from bot_runs r where r.profile = c.profile and r.id = c.run and r.private)
  and not exists (select 1 from bot_effects e where e.profile = c.profile and e.run = c.run and e.call = c.call);

insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
select 'hold-' || nextval('bot_hold_seq'), e.profile, e.run, e.call, e.id, 'tool', e.updated_ms
from bot_effects e
join bot_calls c on c.profile = e.profile and c.run = e.run and c.call = e.call
where e.state = 'held' and e.resolution is null and c.state = 'held'
  and not exists (select 1 from bot_runs r where r.profile = e.profile and r.id = e.run and r.private);

insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
select 'hold-' || nextval('bot_hold_seq'), o.profile, o.run, o.call, null, 'reply', o.updated_ms
from bot_outbox o
where o.state = 'held';

insert into bot_holds (id, profile, run, call, effect, kind, opened_ms)
select 'hold-' || nextval('bot_hold_seq'), r.profile, r.id, 'private:' || r.id, null, 'private', r.updated_ms
from bot_runs r
where r.private and r.state = 'waiting' and r.wait_kind = 'hold';
