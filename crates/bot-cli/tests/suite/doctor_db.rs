//! `bot doctor --check-db`가 실제 PostgreSQL에 짧게 접속해 «연결됐어요»를 알리되 아무것도 만들지 않는다(마이그레이션·쓰기 없음).
//! 팀 시험 DB(`BOT_TEST_DATABASE_URL`)가 있어야 하고, 없으면 건너뛴다(`BOT_TEST_REQUIRE_DB=1`이면 실패).
use crate::process_support::*;

#[test]
fn check_db_connects_and_creates_nothing() {
    let Some(schema) = Schema::create() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let made = bot(
        &[],
        &["init", "demo", "--dir", dir.path().to_str().unwrap()],
    );
    assert!(made.status.success(), "{}", text(&made));
    let profiles = dir.path().join("demo/profiles");
    let host = dir.path().join("demo/host.json");
    let url = schema.url();
    let out = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_DATABASE_URL", url.as_str()),
        ],
        &["doctor", "--check-db"],
    );
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    assert!(
        shown.contains("[통과] 데이터베이스 연결 — 연결됐어요"),
        "{shown}"
    );
    assert!(
        !shown.contains(&schema.base),
        "접속 주소는 출력하지 않는다: {shown}"
    );
    assert_eq!(schema.tables(), "0", "마이그레이션하지 않았다");
}
