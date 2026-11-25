//! 호스트 `limits.tool_call_max_secs`(도구 호출 시간 한도의 천장): 해석, 프로필 검증용 목록, 활성화 지문.
//! 천장을 정하지 않으면 기본 60초이고 지문은 그대로다. 천장을 바꾸면(올리든 내리든) 활성화 사건이다.

use super::*;

fn host(limits: &str) -> HostFile {
    parse(&format!(
        r#"{{"version": 1,
            "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}},
            "models": {{"personal": ["assistant"]}}{limits}}}"#
    ))
    .unwrap()
}

fn fingerprint(host: &HostFile) -> [u8; 32] {
    host.authority_fingerprint(&BTreeMap::new()).unwrap()
}

fn ceiling(secs: u32) -> String {
    format!(r#", "limits": {{"tool_call_max_secs": {secs}}}"#)
}

#[test]
fn the_ceiling_defaults_to_sixty_and_reaches_the_catalog_that_validates_profiles() {
    let plain = host("");
    assert_eq!(plain.limits.tool_call_max_secs, 60);
    assert_eq!(plain.catalog.tool_call_cap(), 60);
    let raised = host(&ceiling(900));
    assert_eq!(raised.limits.tool_call_max_secs, 900);
    assert_eq!(raised.catalog.tool_call_cap(), 900);
    // 내장 데모 호스트도 기본 천장이다
    assert_eq!(demo(1000).catalog.tool_call_cap(), 60);
    // 빈 `limits` 절은 기본값이다
    assert_eq!(host(r#", "limits": {}"#).limits, plain.limits);
}

#[test]
fn a_bad_limits_section_stops_the_host_file_before_anything_starts() {
    for (case, section, why) in [
        ("0초", ceiling(0), "limits.tool_call_max_secs"),
        ("900 초과", ceiling(901), "limits.tool_call_max_secs"),
        (
            "정수가 아님",
            r#", "limits": {"tool_call_max_secs": "300"}"#.to_owned(),
            "limits.tool_call_max_secs",
        ),
        (
            "모르는 키",
            r#", "limits": {"tool_call_max": 300}"#.to_owned(),
            "limits.tool_call_max",
        ),
        ("객체가 아님", r#", "limits": 300"#.to_owned(), "limits"),
    ] {
        let text = format!(
            r#"{{"version": 1, "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}}{section}}}"#
        );
        let error = match parse(&text) {
            Ok(_) => panic!("{case}: 거절해야 해요"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains(why), "{case}: {error}");
    }
}

#[test]
fn only_a_changed_ceiling_changes_the_activation_fingerprint() {
    let base = fingerprint(&host(""));
    assert_eq!(
        fingerprint(&host(&ceiling(60))),
        base,
        "기본값을 적어도 기존 지문 그대로"
    );
    assert_eq!(fingerprint(&host(r#", "limits": {}"#)), base);
    let raised = fingerprint(&host(&ceiling(300)));
    assert_ne!(raised, base, "천장을 올리면 활성화 사건이다");
    assert_ne!(fingerprint(&host(&ceiling(900))), raised);
    assert_ne!(
        fingerprint(&host(&ceiling(30))),
        base,
        "내려도 활성화 사건이다"
    );
    assert_eq!(
        fingerprint(&host(&ceiling(300))),
        raised,
        "같은 값이면 같다"
    );
}
