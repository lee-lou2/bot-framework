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
        "admin bind setting does not affect fingerprint when telemetry is off"
    );
    assert!(
        parse(r#"{"version": 1, "admin": {"trace": {"personal": {"content": true}}}}"#).is_err()
    );
}

#[test]
fn turning_collection_on_off_or_changing_what_leaves_changes_the_fingerprint() {
    let bare = fingerprint(&host(""));
    let on = fingerprint(&host(&traced("personal", ENDPOINT)));
    assert_ne!(on, bare, "enabling trace export must force re-activation");
    assert_eq!(
        fingerprint(&host(&traced("personal", ENDPOINT))),
        on,
        "identical config must produce the same fingerprint"
    );
    let changed = [
        ("content", format!(r#"{ENDPOINT}, "content": true"#)),
        ("plain ids", format!(r#"{ENDPOINT}, "ids": "plain""#)),
        ("sample", format!(r#"{ENDPOINT}, "sample": 0.25"#)),
        (
            "endpoint",
            r#""endpoint": "http://127.0.0.1:4319/v1/traces""#.to_owned(),
        ),
        (
            "headers",
            format!(r#"{ENDPOINT}, "headers": {{"authorization": {{"secret": "otlp-key"}}}}"#),
        ),
    ];
    for (what, body) in changed {
        assert_ne!(
            fingerprint(&host(&traced("personal", &body))),
            on,
            "changing {what} did not change fingerprint"
        );
    }
    assert_eq!(fingerprint(&host("")), bare);
    // Display name and UI link template do not affect exported data.
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

#[test]
fn logs_and_issues_change_the_fingerprint_and_leaving_them_off_preserves_existing_bytes() {
    let bare = fingerprint(&host(""));
    let trace_only = fingerprint(&host(&traced("personal", ENDPOINT)));
    // Golden fingerprints from origin/main 34c860b to catch accidental hash drift.
    assert_eq!(
        bot_core::random::hex(&bare),
        "b5912597998a504ee0018a14e3b4910c39cd6c1f223d1b4e04e0e49629b0611d",
        "baseline host activation fingerprint (logs/issues off) drifted"
    );
    assert_eq!(
        bot_core::random::hex(&trace_only),
        "8b06e3e8736e594cce01431284dd3711c7f9adbe02c8e0b65e2db612f1034a4f",
        "trace-only host activation fingerprint drifted"
    );
    assert_eq!(
        fingerprint(&host(r#", "admin": {"logs": null, "issues": null}"#)),
        bare
    );
    assert_eq!(
        fingerprint(&host(&format!(
            r#", "admin": {{"trace": {{"personal": {{{ENDPOINT}}}}}, "logs": null, "issues": null}}"#
        ))),
        trace_only
    );

    let with_logs = fingerprint(&host(
        r#", "admin": {"logs": {"endpoint": "https://otlp.example.com/v1/logs", "level": "warn"}}"#,
    ));
    assert_ne!(
        with_logs, bare,
        "enabling log export must force re-activation"
    );
    assert_ne!(
        fingerprint(&host(
            r#", "admin": {"logs": {"endpoint": "https://otlp.example.com/v1/logs", "level": "error"}}"#,
        )),
        with_logs,
        "changing log level must force re-activation"
    );
    assert_eq!(
        fingerprint(&host(
            r#", "admin": {"logs": {"name": "renamed", "endpoint": "https://otlp.example.com/v1/logs", "level": "warn"}}"#,
        )),
        with_logs
    );

    let with_issues = fingerprint(&host(
        r#", "admin": {"issues": {"kind": "sentry", "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/", "key": {"secret": "sentry-key"}}}"#,
    ));
    assert_ne!(
        with_issues, bare,
        "enabling issue export must force re-activation"
    );
    assert_ne!(
        fingerprint(&host(
            r#", "admin": {"issues": {"kind": "sentry", "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/", "key": {"secret": "sentry-key"}, "environment": "prod"}}"#,
        )),
        with_issues,
        "changing issue environment must force re-activation"
    );
    assert_eq!(
        fingerprint(&host(
            r#", "admin": {"issues": {"kind": "sentry", "name": "renamed", "endpoint": "https://o0.ingest.sentry.io/api/1/envelope/", "key": {"secret": "sentry-key"}}}"#,
        )),
        with_issues
    );
}
