-- 키로 이어지는 긴 요청(OpenAI 호환 HTTP 입구)의 접수 행·입구별 진행 중 작업 수·실행 계량.
-- 접수 행(`bot_requests`)은 기존 접수 트랜잭션을 그대로 쓴다: 별도 원장이 아니다. 입구 접수 행은 `api_endpoint`·`api_model`을 함께 갖고, 접수 때 고정한 값이다.
alter table bot_requests
    add column api_endpoint text,
    add column api_model text,
    add constraint bot_requests_api_both check ((api_endpoint is null) = (api_model is null));
-- (프로필, 입구, 주체, 키)는 모델·바인딩이 달라도 하나다: 같은 키의 의미가 둘로 나뉘지 않는다. 기존 키(프로필, 바인딩, 주체, 요청)와 별개로 지킨다
create unique index bot_requests_api_key on bot_requests (profile, api_endpoint, principal, request)
    where api_endpoint is not null;

-- 루트 run만 입구를 가진다(위임 자식은 루트와 한 작업이다). 진행 중 작업 수는 이 부분 색인으로 센다: 끝난 작업은 색인에서 빠진다.
-- truncated·usage_*는 성공 완료 커밋이 쓰는 실행 계량이다: 비어 있으면 «기록 없음(잘리지 않음·사용량 모름)»이고 지어내지 않는다.
alter table bot_runs
    add column api_endpoint text,
    add column truncated boolean,
    add column usage_prompt bigint,
    add column usage_completion bigint,
    add constraint bot_runs_usage_both check ((usage_prompt is null) = (usage_completion is null));
create index bot_runs_api_active on bot_runs (profile, api_endpoint)
    where api_endpoint is not null and state in ('ready', 'leased', 'waiting', 'retry');

-- 접수 때 호스트가 확인한 사람 승인 전달 설치의 Slack team_id: 원래 요청에 묶여, 같은 별칭의 실제 팀이 재배포로 바뀌어도 옛 요청의 카드가 새 팀으로 가지 않는다.
alter table bot_requests add column api_team text;

-- HTTP(OpenAI 호환) 출처 실행의 승인 카드 전달 작업이 원래 요청에서 이어받는 고정 증거(채널 출처 작업은 비어 있다). `binding`은 전달 바인딩(Slack 채널 바인딩),
-- `owner`는 지정된 사람이다. 닫는 이유에 승인 경로 판정(`no_approver`·`approver_revoked`·`route_changed`)을 더한다.
alter table bot_card_jobs
    add column origin_binding text,
    add column requester text,
    add column revision text,
    add column origin_team text,
    add constraint bot_card_jobs_origin_all_or_none
        check ((origin_binding is null) = (requester is null) and (origin_binding is null) = (revision is null));
alter table bot_card_jobs drop constraint bot_card_jobs_closed_reason_check;
alter table bot_card_jobs add constraint bot_card_jobs_closed_reason_check
    check (closed_reason in ('unavailable', 'held', 'stale', 'no_approver', 'approver_revoked', 'route_changed'));
