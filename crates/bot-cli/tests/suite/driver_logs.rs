//! 데이터베이스 드라이버의 기록은 어떤 명령에서도 stderr에 나오지 않는다. 접속 주소를 구문 분석하다 모르는 매개변수의 값(합성 `sslpassword`)을
//! 경고로 남기기 때문이다. 받자마자 끊는 서버를 두고 실제 `bot serve`와 `bot profile-bundle put`을 `BOT_LOG=trace`로 실행한다(DB가 필요 없다).
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
    // 접속은 바로 끊긴다(접속 거부와 달리 연결 라이브러리가 다시 시도하지 않는다)
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
        // 저장소에 접속하려다 실패한 것이어야 한다(그 길을 지나지 않았다면 이 시험은 아무것도 보지 못한 것이다)
        assert_eq!(out.status.code(), Some(1), "{args:?}: {shown}");
        assert!(
            shown.contains("저장소에 접속하지 못했어요"),
            "{args:?}: {shown}"
        );
        assert!(!shown.contains(SECRET), "{args:?}: {shown}");
        assert!(!shown.contains("sslpassword"), "{args:?}: {shown}");
    }
}
