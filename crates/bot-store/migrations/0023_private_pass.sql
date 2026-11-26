-- NULL은 전용 처리 지문(v1)이다. 구판의 완료에는 넘김을 확정할 증거가 없다.
-- 일반 공개를 허용한 새 접수만 input_sha256에 전체 봉투의 v2 HMAC을 쓴다.
-- 기존 행은 모두 NULL이다. 새 행과 갱신에만 검사해 잠금 중 전체 표 스캔을 피한다.
alter table bot_runs add column private_pass text;
alter table bot_runs add constraint bot_runs_private_pass_state
    check (private_pass is null or
           (private and (private_pass = 'bound_v2' or (private_pass = 'passed_v2' and state = 'succeeded')))) not valid;
