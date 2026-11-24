//! 호스트 설정의 `admin` 절: 문서의 예시가 그대로 읽히고, 잘못된 설정은 시작 전에 거절된다.

use bot_admin::{
    AdminSettings,
    settings::DEFAULT_BIND,
    trace::{HeaderValue, Ids},
};
use bot_core::id::{ProfileId, SecretName};
use serde_json::{Value, json};

/// 문서(docs/admin.md)의 첫 JSON 예시. 문서와 구현이 어긋나면 이 시험이 알려 준다.
fn documented() -> Value {
    let doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/admin.md"));
    let start = doc.find("```json\n").expect("문서의 JSON 예시") + "```json\n".len();
    let end = start + doc[start..].find("```").expect("예시 끝");
    serde_json::from_str(&doc[start..end]).expect("예시는 올바른 JSON")
}

fn err(v: Value) -> String {
    AdminSettings::from_json(&v).expect_err("거절돼야 한다")
}

#[test]
fn an_empty_section_gets_local_defaults() {
    let s = AdminSettings::from_json(&json!({})).expect("기본값");
    assert_eq!(s.bind, DEFAULT_BIND.parse().expect("주소"));
    assert!(s.bind.ip().is_loopback() && !s.remote && s.hosts.is_empty() && s.trace.is_empty());
    assert_eq!(s.max_failures, 5);
}

#[test]
fn the_documented_example_parses_into_typed_settings() {
    let s = AdminSettings::from_json(&documented()).expect("문서의 예시");
    assert_eq!(
        (s.bind.to_string().as_str(), s.remote, s.max_failures),
        ("127.0.0.1:8787", false, 5)
    );
    let t = &s.trace[&ProfileId("personal".into())];
    assert_eq!((t.ids, t.content, t.sample), (Ids::Hash, false, 1.0));
    let b = t.backend.as_ref().expect("수집기");
    assert_eq!(
        (b.name.as_str(), b.endpoint.as_str()),
        ("local-collector", "http://127.0.0.1:4318/v1/traces")
    );
    assert_eq!(
        b.link.as_deref(),
        Some("https://traces.example/t/{trace_id}")
    );
    let secret = HeaderValue::Secret(SecretName::parse("collector-token").expect("이름"));
    assert!(b.headers.contains(&("Authorization".to_owned(), secret)));
    assert!(
        b.headers
            .contains(&("X-Env".to_owned(), HeaderValue::Plain("dev".into())))
    );
}

#[test]
fn config_builds_hosts_from_the_actual_address_and_the_extras() {
    let s = AdminSettings::from_json(&json!({"hosts": ["Admin.Internal:8443"], "max_failures": 3}))
        .expect("설정");
    let actual = "127.0.0.1:41234".parse().expect("주소");
    let cfg = s.config(actual);
    for h in [
        "127.0.0.1:41234",
        "localhost:41234",
        "[::1]:41234",
        "admin.internal:8443",
    ] {
        assert!(cfg.hosts.iter().any(|x| x == h), "{h} in {:?}", cfg.hosts);
    }
    assert_eq!(cfg.max_failures, 3);
}

#[test]
fn bad_settings_are_refused_with_the_path_of_the_problem() {
    for (v, needle) in [
        (json!([]), "admin"),
        (json!({"bnd": "x"}), "bnd"),
        (json!({"bind": 8787}), "admin.bind"),
        (json!({"bind": "localhost"}), "admin.bind"),
        (json!({"bind": "0.0.0.0:8787"}), "원격"),
        (json!({"remote": "yes"}), "admin.remote"),
        (json!({"hosts": "a:1"}), "admin.hosts"),
        (json!({"hosts": ["no-port"]}), "admin.hosts"),
        (json!({"hosts": ["bad host:80"]}), "admin.hosts"),
        (json!({"max_failures": 0}), "admin.max_failures"),
        (json!({"max_failures": 101}), "admin.max_failures"),
        (json!({"trace": []}), "admin.trace"),
        (
            json!({"trace": {"Bad_Name": {"endpoint": "http://127.0.0.1:4318"}}}),
            "Bad_Name",
        ),
        (json!({"trace": {"p": []}}), "admin.trace.p"),
        (json!({"trace": {"p": {}}}), "endpoint"),
        (
            json!({"trace": {"p": {"endpoint": "https://collector:4318"}}}),
            "p:",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://collector.example.com:4318"}}}),
            "admin.trace.p.remote",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "remote": "yes"}}}),
            "admin.trace.p.remote",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "extra": 1}}}),
            "extra",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "ids": "raw"}}}),
            "ids",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "sample": 1.5}}}),
            "sample",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "sample": "all"}}}),
            "sample",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "content": "on"}}}),
            "content",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "link": "https://t.example/fixed"}}}),
            "링크",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": 1}}}}),
            "headers.A",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": {"secret": "Bad Name"}}}}}),
            "비밀 이름",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": {"secret": "x", "more": 1}}}}}),
            "more",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"bad name": "v"}}}}),
            "헤더",
        ),
    ] {
        let e = err(v.clone());
        assert!(e.contains(needle), "{v} → {e}");
    }
}

#[test]
fn remote_binding_and_disabled_profiles_are_explicit() {
    let s = AdminSettings::from_json(&json!({"bind": "0.0.0.0:8787", "remote": true}))
        .expect("명시한 원격");
    assert!(s.remote && !s.bind.ip().is_loopback());
    let s = AdminSettings::from_json(
        &json!({"trace": {"off": null, "on": {"endpoint": "http://127.0.0.1:4318"}}}),
    )
    .expect("설정");
    assert_eq!(
        s.trace.keys().map(|k| k.0.as_str()).collect::<Vec<_>>(),
        ["on"],
        "null이면 꺼짐"
    );
    let t = &s.trace[&ProfileId("on".into())];
    assert_eq!(
        (t.ids, t.content),
        (Ids::Hash, false),
        "기본은 가명·내용 꺼짐"
    );
    assert_eq!(t.backend.as_ref().map(|b| b.name.as_str()), Some("default"));
}

#[test]
fn a_collector_outside_the_private_network_is_accepted_only_with_the_explicit_remote_option() {
    let remote = |flag: Option<bool>| {
        let mut p = json!({"endpoint": "http://203.0.113.7:4318/v1/traces"});
        if let Some(flag) = flag {
            p["remote"] = json!(flag);
        }
        json!({"trace": {"p": p}})
    };
    for flag in [None, Some(false)] {
        let e = err(remote(flag));
        assert!(
            e.contains("203.0.113.7") && e.contains("admin.trace.p.remote"),
            "{e}"
        );
    }
    let s = AdminSettings::from_json(&remote(Some(true))).expect("명시한 원격 수집기");
    assert!(
        s.trace[&ProfileId("p".into())]
            .backend
            .as_ref()
            .unwrap()
            .remote
    );
    // 루프백은 명시 없이 받고, 그때 옵션은 꺼져 있다
    let s =
        AdminSettings::from_json(&json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318"}}}))
            .expect("루프백");
    assert!(
        !s.trace[&ProfileId("p".into())]
            .backend
            .as_ref()
            .unwrap()
            .remote
    );
}

#[tokio::test]
async fn parsed_trace_settings_start_an_exporter_and_unknown_profiles_stay_silent() {
    use bot_admin::trace::{Exporter, Options, Redactor};
    let s = AdminSettings::from_json(
        &json!({"trace": {"p": {"endpoint": "http://127.0.0.1:9/v1/traces"}}}),
    )
    .expect("설정");
    let e = Exporter::start(s.trace, None, Redactor::default(), Options::default()).expect("시작");
    assert!(e.stats(&ProfileId("p".into())).is_some_and(|x| x.enabled));
    assert!(e.stats(&ProfileId("other".into())).is_none());
}
