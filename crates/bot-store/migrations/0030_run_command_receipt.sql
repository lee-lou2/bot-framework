-- 운영자 명령의 성공 감사 행을 재전송 영수증으로 쓴다.
-- 명령 ID는 조회 키이고, 같은 행의 args_sha256이 ID·인증 주체·화면에서 본 상태를 함께 봉인한다.
alter table bot_audit add column run_command_id text;

create unique index bot_audit_run_command
    on bot_audit (profile, target, run_command_id)
    where run_command_id is not null;
