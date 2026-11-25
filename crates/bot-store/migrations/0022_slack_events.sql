-- F2: Slack 이벤트 한 건은 바인딩·프로필이 바뀌어도 접수 하나다. `slack_key`는 어댑터가 인증한 이벤트의 식별(`SlackSource::event_key`: 글은 팀·채널·ts, 반응은 팀·event_id)이다.
-- 기존 접수 키(프로필, 바인딩, 주체, 요청)와 별개로 지킨다: 바인딩이 바뀐 뒤의 재전달은 그 키가 달라 두 번째 실행을 만들 수 있었다.
alter table bot_requests add column slack_key text;

-- 이 열이 없던 구판의 Slack 접수 행(대화 종류가 채널·DM이고 요청 ID가 `채널.글ts`)에는 팀을 모르는 임시 키(`l`, 단위 구분자, `채널.글ts`)를 단다. 같은 글의 행이 여럿이면
-- (바인딩이 바뀐 재전달이 이미 만든 것) 가장 먼저 받은 한 행에만 단다. 그 글이 다시 오면 접수가 이 행을 정식 키로 채택해 첫 run으로 수렴한다(`bot-store/src/slack_events.rs`).
with legacy as (
    select q.profile, q.binding, q.principal, q.request,
           row_number() over (partition by q.request order by q.created_ms, q.profile, q.binding, q.principal) as n
      from bot_requests q
      join bot_runs r on r.profile = q.profile and r.id = q.run
     where q.api_endpoint is null
       and r.conv_kind in ('channel', 'direct')
       and q.request ~ '^[A-Za-z0-9]+[.][0-9]+[.][0-9]+$'
)
update bot_requests q
   set slack_key = 'l' || chr(31) || q.request
  from legacy
 where legacy.n = 1
   and q.profile = legacy.profile and q.binding = legacy.binding
   and q.principal = legacy.principal and q.request = legacy.request;

create unique index bot_requests_slack_key on bot_requests (slack_key) where slack_key is not null;
