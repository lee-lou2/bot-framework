-- 프로필 판본의 검증한 바이트 묶음: 재배포 뒤 새 프로세스가 진행 중 작업이 고정한 옛 판본을 복원하는 근거.
-- 키 = (프로필, 판본)이고 판본은 묶음의 SHA-256이므로 DB가 «키와 바이트가 맞다»를 강제한다. 한 번 보관한 묶음은 바꾸거나 지우지 않는다(자동 삭제 없음).

create table bot_profile_bundles (
    profile text not null,
    revision text not null,
    bundle bytea not null,
    created_ms bigint not null default bot_now_ms(),
    primary key (profile, revision),
    check (revision = encode(sha256(bundle), 'hex'))
);

create function bot_profile_bundles_frozen() returns trigger language plpgsql as $$
begin
    raise exception 'bot_profile_bundles rows are immutable' using errcode = '55000';
end $$;

create trigger bot_profile_bundles_frozen before update or delete on bot_profile_bundles
    for each row execute function bot_profile_bundles_frozen();
