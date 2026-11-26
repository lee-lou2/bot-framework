-- 회신의 발송 책임. 원래 대화의 키를 run에 두고 실제 목적지의 키를 회신 행에 둔다.
alter table bot_runs add column reply_transport text check (
    reply_transport is null or (reply_transport <> '' and not private and conv_kind in ('direct', 'channel'))
);
-- 빈 값은 구판 자료 중 책임을 판정할 수 없는 회신이다. 제품의 발송 키는 비어 있지 않다.
alter table bot_outbox add column transport text not null default '';

-- 공식 구판의 푸시 어댑터는 Slack 하나다. Slack 주체의 원래 푸시 대화에서만 책임을 계승한다.
update bot_runs set reply_transport = 'slack'
where not private and conv_kind in ('direct', 'channel') and principal like 'slack:%';

-- 원래 대화와 같은 회신 또는 기존 승인 경로가 동결한 명시 목적지만 Slack으로 승계한다.
update bot_outbox o set transport = 'slack'
from bot_runs r
where (r.profile, r.id) = (o.profile, o.run) and not r.private and o.conv_kind in ('direct', 'channel')
  and ((r.reply_transport = 'slack'
        and (o.binding, o.principal, o.conv_kind, o.conv_id) = (r.binding, r.principal, r.conv_kind, r.conv_id)
        and o.thread is not distinct from r.thread)
      or exists (select 1 from bot_deliveries d
                 where (d.profile, d.run) = (o.profile, o.run) and d.state = 'enqueued'
                   and (o.binding, o.conv_kind, o.conv_id) = (d.dest_binding, d.dest_kind, d.dest_id)
                   and o.thread is not distinct from d.dest_thread));

alter table bot_outbox alter column transport drop default;
drop index bot_outbox_pending;
create index bot_outbox_pending on bot_outbox (transport, created_ms, profile, run, seq) where state = 'pending';
