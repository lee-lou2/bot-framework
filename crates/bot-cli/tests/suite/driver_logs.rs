//! Ensures database driver warnings (which can log unknown DSN query parameters such as
//! `sslpassword`) never reach stderr even under `BOT_LOG=trace`.
use crate::process_support::*;
use std::net::TcpListener;

const SECRET: &str = "sslvalue-SENTINEL-3b7d1";

#[test]
fn database_driver_warnings_never_reach_stderr_in_serve_or_profile_bundle() {
    let dir = tempfile::tempdir().unwrap();
    let made = bot(
        &[],
        &["init", "demo", "--dir", dir.path().to_str().unwrap()],
    );
    assert!(made.status.success(), "{}", text(&made));
    let profiles = dir.path().join("demo/profiles");
    let host = dir.path().join("demo/host.json");
    let profile = profiles.join("demo");
    // Drop incoming connections immediately so the PG driver fails fast without retrying.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || listener.incoming().for_each(drop));
    let url = format!("postgres://user@127.0.0.1:{port}/db?sslpassword={SECRET}");
    let env = [
        ("BOT_PROFILES", profiles.to_str().unwrap()),
        ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ("BOT_DATABASE_URL", url.as_str()),
        ("BOT_LOG", "trace"),
    ];
    for args in [
        vec!["serve", "--role", "gateway"],
        vec!["profile-bundle", "put", profile.to_str().unwrap()],
    ] {
        let out = bot(&env, &args);
        let shown = text(&out);
        let low = shown.to_ascii_lowercase();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {shown}");
        assert!(
            low.contains("connect") && (low.contains("store") || low.contains("database")),
            "{args:?}: {shown}"
        );
        assert!(!shown.contains(SECRET), "{args:?}: {shown}");
        assert!(!shown.contains("sslpassword"), "{args:?}: {shown}");
    }
}
