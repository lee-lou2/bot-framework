-- 기존 대화 종류나 요청자만으로 수동 입력과 자동 발화를 구분할 수 없다.
alter table bot_runs add column run_origin text not null default 'unknown'
    check (run_origin in ('message', 'http', 'schedule', 'wake', 'unknown'));
alter table bot_user_schedules add column fire_origin text not null default 'unknown'
    check (fire_origin in ('schedule', 'wake', 'unknown'));
