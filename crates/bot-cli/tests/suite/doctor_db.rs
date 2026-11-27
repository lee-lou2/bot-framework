//! Verifies `bot doctor --check-db` connects to PostgreSQL, reports success without printing the URL, and creates no tables.
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
        shown.lines().any(|line| {
            let low = line.to_ascii_lowercase();
            (low.starts_with("[pass]") || low.starts_with("[ok]"))
                && (low.contains("database") || low.contains("db"))
                && low.contains("connect")
        }),
        "{shown}"
    );
    assert!(
        !shown.contains(&schema.base),
        "connection URL must not be printed: {shown}"
    );
    assert_eq!(schema.tables(), "0", "must not run migrations");
}
