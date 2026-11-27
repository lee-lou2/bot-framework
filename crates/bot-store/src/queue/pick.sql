-- Eligibility timestamp and state filters must match the bot_runs_eligible index in migrations/0024_queue_order.sql.
with clock(ms) as (select bot_now_ms())
select r.profile, r.id, r.state, r.attempts, r.generation, r.parent, r.parent_step, r.cancel_requested, r.claim_set
from bot_runs r
cross join lateral (values (coalesce(r.lease_until_ms, r.not_before_ms))) due(ms)
where not r.private and r.state in ('ready', 'retry', 'leased')
  and due.ms <= (select ms from clock)
  and (r.state <> 'leased' or r.lease_until_ms < (select ms from clock))
  and (r.profile, r.id) not in (
        select s.profile, s.id from unnest($1::text[], $2::text[]) as s(profile, id))
  and (r.serial_key is null or not exists (
        select 1 from bot_runs o
        where o.profile = r.profile and o.serial_key = r.serial_key and o.id <> r.id
          and o.state = 'leased'))
order by due.ms, r.created_ms, r.id
limit 1
for update of r skip locked
