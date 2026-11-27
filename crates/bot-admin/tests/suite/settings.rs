//! Host config `admin` section parsing: documented examples parse cleanly and invalid configs are rejected.

use bot_admin::{
    AdminSettings,
    settings::DEFAULT_BIND,
    trace::{HeaderValue, Ids},
};
use bot_core::id::{ProfileId, SecretName};
use serde_json::{Value, json};

/// First JSON example in `docs/admin.md`, kept in lockstep with `AdminSettings::from_json`.
fn documented() -> Value {
    let doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/admin.md"));
    let start = doc.find("```json\n").expect("JSON example in docs") + "```json\n".len();
    let end = start + doc[start..].find("```").expect("end of example");
    serde_json::from_str(&doc[start..end]).expect("valid JSON example")
}

fn err(v: Value) -> String {
    AdminSettings::from_json(&v).expect_err("expected error")
}

#[test]
fn an_empty_section_gets_local_defaults() {
    let s = AdminSettings::from_json(&json!({})).expect("defaults");
    assert_eq!(s.bind, DEFAULT_BIND.parse().expect("addr"));
    assert!(s.bind.ip().is_loopback() && !s.remote && s.hosts.is_empty() && s.trace.is_empty());
    assert_eq!(s.max_failures, 5);
}

#[test]
fn the_documented_example_parses_into_typed_settings() {
    let s = AdminSettings::from_json(&documented()).expect("documented example");
    assert_eq!(
        (s.bind.to_string().as_str(), s.remote, s.max_failures),
        ("127.0.0.1:8787", false, 5)
    );
    let t = &s.trace[&ProfileId("personal".into())];
    assert_eq!((t.ids, t.content, t.sample), (Ids::Hash, false, 1.0));
    let b = t.backend.as_ref().expect("collector");
    assert_eq!(
        (b.name.as_str(), b.endpoint.as_str()),
        ("local-collector", "http://127.0.0.1:4318/v1/traces")
    );
    assert_eq!(
        b.link.as_deref(),
        Some("https://traces.example/t/{trace_id}")
    );
    let secret = HeaderValue::Secret(SecretName::parse("collector-token").expect("name"));
    assert!(b.headers.contains(&("Authorization".to_owned(), secret)));
    assert!(
        b.headers
            .contains(&("X-Env".to_owned(), HeaderValue::Plain("dev".into())))
    );
}

#[test]
fn config_builds_hosts_from_the_actual_address_and_the_extras() {
    let s = AdminSettings::from_json(&json!({"hosts": ["Admin.Internal:8443"], "max_failures": 3}))
        .expect("settings");
    let actual = "127.0.0.1:41234".parse().expect("addr");
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
        (json!({"bind": "0.0.0.0:8787"}), "admin.bind"),
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
            json!({"trace": {"p": {"endpoint": "ftp://collector:4318"}}}),
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
            "{trace_id}",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": 1}}}}),
            "headers.A",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": {"secret": "Bad Name"}}}}}),
            "admin.trace.p.headers.A",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"A": {"secret": "x", "more": 1}}}}}),
            "more",
        ),
        (
            json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318", "headers": {"bad name": "v"}}}}),
            "p:",
        ),
    ] {
        let e = err(v.clone());
        assert!(e.contains(needle), "{v} → {e}");
    }
}

#[test]
fn remote_binding_and_disabled_profiles_are_explicit() {
    let s = AdminSettings::from_json(&json!({"bind": "0.0.0.0:8787", "remote": true}))
        .expect("explicit remote");
    assert!(s.remote && !s.bind.ip().is_loopback());
    let s = AdminSettings::from_json(
        &json!({"trace": {"off": null, "on": {"endpoint": "http://127.0.0.1:4318"}}}),
    )
    .expect("settings");
    assert_eq!(
        s.trace.keys().map(|k| k.0.as_str()).collect::<Vec<_>>(),
        ["on"],
        "null disables profile tracing"
    );
    let t = &s.trace[&ProfileId("on".into())];
    assert_eq!(
        (t.ids, t.content),
        (Ids::Hash, false),
        "defaults to hashed IDs and content disabled"
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
    let s = AdminSettings::from_json(&remote(Some(true))).expect("explicit remote collector");
    assert!(
        s.trace[&ProfileId("p".into())]
            .backend
            .as_ref()
            .unwrap()
            .remote
    );
    let s =
        AdminSettings::from_json(&json!({"trace": {"p": {"endpoint": "http://127.0.0.1:4318"}}}))
            .expect("loopback");
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
    .expect("settings");
    let e = Exporter::start(s.trace, None, Redactor::default(), Options::default()).expect("start");
    assert!(e.stats(&ProfileId("p".into())).is_some_and(|x| x.enabled));
    assert!(e.stats(&ProfileId("other".into())).is_none());
}

#[test]
fn langfuse_langsmith_otlp_logs_and_sentry_examples_parse_and_validate() {
    let doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/admin.md"));
    let first = doc.find("```json\n").expect("first example") + "```json\n".len();
    let second_start =
        first + doc[first..].find("```json\n").expect("second example") + "```json\n".len();
    let second_end = second_start + doc[second_start..].find("```").expect("end of example");
    let parsed: Value = serde_json::from_str(&doc[second_start..second_end]).expect("JSON");
    let s = AdminSettings::from_json(&parsed).expect("Langfuse/LangSmith/logs/issues example");
    assert_eq!(
        s.trace[&ProfileId("personal".into())]
            .backend
            .as_ref()
            .unwrap()
            .name,
        "langfuse"
    );
    assert_eq!(
        s.trace[&ProfileId("work".into())]
            .backend
            .as_ref()
            .unwrap()
            .name,
        "langsmith"
    );
    let logs = s.logs.expect("logs");
    assert_eq!(
        (logs.name.as_str(), logs.level),
        ("otel-logs", bot_admin::logs::LogLevel::Warn)
    );
    let issues = s.issues.expect("issues");
    assert_eq!(
        (
            issues.kind,
            issues.environment.as_deref(),
            issues.release.as_deref()
        ),
        (
            bot_admin::issues::IssueKind::Sentry,
            Some("production"),
            Some("bot@0.1.0")
        )
    );
    assert_eq!(
        issues
            .key
            .as_ref()
            .map(|k| (k.profile.0.as_str(), k.name.as_str())),
        Some(("host", "sentry-key"))
    );

    for (v, needle) in [
        (json!({"logs": {}}), "admin.logs.endpoint"),
        (
            json!({"logs": {"endpoint": "http://otlp.example.com/v1/logs"}}),
            "admin.logs.remote",
        ),
        (
            json!({"logs": {"endpoint": "https://otlp.example.com/v1/logs", "level": "trace"}}),
            "admin.logs.level",
        ),
        (json!({"issues": {}}), "admin.issues.endpoint"),
        (
            json!({"issues": {"endpoint": "https://o0.ingest.sentry.io/api/1/envelope/"}}),
            "admin.issues.key",
        ),
        (
            json!({"issues": {"kind": "jira", "endpoint": "http://127.0.0.1:9000/api/1/envelope/"}}),
            "admin.issues.kind",
        ),
        (
            json!({"issues": {"endpoint": "http://sentry.example.com/api/1/envelope/", "key": {"secret": "k"}}}),
            "admin.issues.remote",
        ),
    ] {
        let e = err(v.clone());
        assert!(e.contains(needle), "{v} → {e}");
    }
}
