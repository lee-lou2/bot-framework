//! 수집(OTLP) 설정은 활성화 지문에 든다: 거두거나 바꾸면 활성화 사건이라 옛 설정으로 남은 프로세스가 구판이 된다(그 프로세스의 내보내기 문이 닫힌다).
//! 수집을 쓰지 않는 호스트의 지문은 그대로다.

use super::*;

fn host(admin: &str) -> HostFile {
    parse(&format!(
        r#"{{"version": 1,
            "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}},
            "models": {{"personal": ["assistant"]}}{admin}}}"#
    ))
    .unwrap()
}

fn fingerprint(host: &HostFile) -> [u8; 32] {
    host.authority_fingerprint(&BTreeMap::new()).unwrap()
}

fn traced(profile: &str, body: &str) -> String {
    format!(r#", "admin": {{"trace": {{"{profile}": {{{body}}}}}}}"#)
}

const ENDPOINT: &str = r#""endpoint": "http://127.0.0.1:4318/v1/traces""#;

#[test]
fn a_host_that_does_not_collect_keeps_its_fingerprint_whatever_else_the_admin_section_says() {
    let bare = fingerprint(&host(""));
    assert_eq!(
        fingerprint(&host(r#", "admin": {"bind": "127.0.0.1:0"}"#)),
        bare,
        "수집이 없으면 관리 화면 설정은 지문과 무관하다"
    );
    // 백엔드(endpoint)가 없는 설정은 아무것도 내보내지 않으니 만들 수도 없다(endpoint는 필수)
    assert!(
        parse(r#"{"version": 1, "admin": {"trace": {"personal": {"content": true}}}}"#).is_err()
    );
}

#[test]
fn turning_collection_on_off_or_changing_what_leaves_changes_the_fingerprint() {
    let bare = fingerprint(&host(""));
    let on = fingerprint(&host(&traced("personal", ENDPOINT)));
    assert_ne!(on, bare, "수집을 켜면 활성화 사건이다");
    assert_eq!(
        fingerprint(&host(&traced("personal", ENDPOINT))),
        on,
        "같은 설정은 같은 지문이다"
    );
    let changed = [
        ("내용 허용", format!(r#"{ENDPOINT}, "content": true"#)),
        ("식별자 평문", format!(r#"{ENDPOINT}, "ids": "plain""#)),
        ("표본", format!(r#"{ENDPOINT}, "sample": 0.25"#)),
        (
            "목적지",
            r#""endpoint": "http://127.0.0.1:4319/v1/traces""#.to_owned(),
        ),
        (
            "헤더",
            format!(r#"{ENDPOINT}, "headers": {{"authorization": {{"secret": "otlp-key"}}}}"#),
        ),
    ];
    for (what, body) in changed {
        assert_ne!(
            fingerprint(&host(&traced("personal", &body))),
            on,
            "{what}을(를) 바꿨는데 지문이 그대로다"
        );
    }
    // 거두면 처음 지문으로 돌아온다 — 그 사이의 «켠 설정»으로 남은 프로세스는 구판이다
    assert_eq!(fingerprint(&host("")), bare);
    // 백엔드 이름·화면 링크 틀은 내보내는 것을 바꾸지 않는다
    assert_eq!(
        fingerprint(&host(&traced(
            "personal",
            &format!(r#"{ENDPOINT}, "name": "main", "link": "https://t.example/{{trace_id}}""#)
        ))),
        on
    );
}

#[test]
fn collection_for_another_profile_is_a_change_too() {
    let one = fingerprint(&host(&traced("personal", ENDPOINT)));
    let two = fingerprint(&host(&format!(
        r#", "admin": {{"trace": {{"personal": {{{ENDPOINT}}}, "work": {{{ENDPOINT}}}}}}}"#
    )));
    assert_ne!(one, two);
}
