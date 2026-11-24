-- 0008이 무효로 닫은 옛 기억 공유 승인(항목 ID만 묶은 해시)에 달려 있던 **시작 전** 호출을 정리한다. 그 호출은 옛 해시를 갖고 있어
-- 같은 호출 ID로 새 지문(`memory_share_hash`)을 `begin`하면 충돌한다. 시작하지 않았으므로 바깥에 남은 효과가 없다: 호출 행을 지워
-- (무효 승인 행도 함께 지워진다. 감사 기록은 남는다) 깨어난 실행이 같은 호출 ID로 새 지문과 새 카드(새 승인)를 만들게 한다.
delete from bot_calls c
where c.tool = 'memory.share' and c.state = 'open'
  and exists (select 1 from bot_approvals a where a.profile = c.profile and a.run = c.run and a.call = c.call and a.state = 'superseded')
  and not exists (select 1 from bot_approvals a where a.profile = c.profile and a.run = c.run and a.call = c.call and a.state not in ('superseded', 'expired', 'denied', 'revoked'));
