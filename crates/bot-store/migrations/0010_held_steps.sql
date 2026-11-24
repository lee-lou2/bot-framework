-- 이전 판은 호출을 보류해도 결과가 불명인 단계(시작했지만 영수증이 없다)를 started로 두어, run은 보류로 기다리는데 운영자 보류 목록에는
-- 단계가 하나도 나타나지 않았다. 아직 판정하지 않은 보류 호출의 미확정(started) 단계를 판정 가능한 보류(held)로 옮긴다.
-- 사유는 그 호출의 보류 사유 코드를 따른다(없으면 outcome_unknown). 영수증을 받은 단계와 이미 판정한 단계는 건드리지 않는다.
update bot_effects e
set state = 'held', reason = coalesce(e.reason, c.reason, 'outcome_unknown'), updated_ms = bot_now_ms()
from bot_calls c
where c.profile = e.profile and c.run = e.run and c.call = e.call
  and c.state = 'held' and c.resolution is null
  and e.state = 'started';
