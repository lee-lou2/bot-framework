-- 기억 공유 승인의 항목·본문 묶음 열을 없앤다. 승인 인자 해시가 곧 {"id", 본문 SHA-256}의 지문이고(core `memory_share_hash`),
-- 공유 커밋(`MemoryStore::share`)이 현재 본문으로 지문을 다시 계산해 호출·승인의 해시와 대조한다.
alter table bot_approvals drop column memory_id, drop column memory_sha256;

-- 이전 판이 연 기억 공유 승인(항목 ID만 묶은 해시)은 지문 규약이 달라 공유로 이어질 수 없다. 소비하지 않은 채 무효로 닫고,
-- 승인을 기다리던 실행은 깨워 새 카드(새 승인)로 이어 가게 한다(같은 세대로 재개한다).
with closed as (
    update bot_approvals set state = 'superseded'
    where tool = 'memory.share' and state in ('pending', 'approved')
    returning profile, run
)
update bot_runs r set state = 'ready', wait_kind = null, wait_child = null, keep_generation = true,
                      not_before_ms = 0, updated_ms = bot_now_ms()
from closed
where r.profile = closed.profile and r.id = closed.run and r.state = 'waiting' and r.wait_kind = 'approval';
