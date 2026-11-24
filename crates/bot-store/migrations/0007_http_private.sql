-- HTTP 입구의 최초 라우팅(경로·메서드·응답 방식)과 민감 DM 접수의 전송 시작 표시.
-- 이전 판이 접수한 HTTP 행은 세 열이 모두 비어 있다: 조회는 경로 없이(None) 돌려주고 응답 방식은 accepted로 본다.

alter table bot_requests
    add column http_method text,
    add column http_path text,
    add column http_response text check (http_response in ('accepted', 'sync')),
    add constraint bot_requests_http_all_or_none
        check ((http_method is null) = (http_path is null) and (http_method is null) = (http_response is null));

-- 전용 원장(PrivateLedger)이 외부 전송 직전에 한 번 커밋한 시각. 이 값이 있으면 보냈는지 모르는 상태이므로
-- 리스가 끝났을 때 다시 실행하지 않고 보류한다. 전용 원장만 바꾼다.
alter table bot_runs add column private_started_ms bigint;

-- 이전 판의 미종결 민감 접수(leased)에는 시작 표시가 없다. 시작 안 함으로 추측하지 않는다: 리스가 끝나면 청소(sweep_private)가
-- 감사 기록과 함께 보류로 옮기고, 보류 뒤에는 늦은 성공도 받지 않으며 운영자 증거 판정만 받는다.
