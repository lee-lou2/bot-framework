//! Unit tests for `gateway` host registry parsing and validation.
use super::*;

const SAMPLE: &str = r#"{
    "cli": {"accounts": {"owner": {"uid": 501}}},
    "slack": {"workspaces": {"home": {
        "team_id": "T0AAAAAAA", "app_id": "A0BBBBBBB",
        "bot_token": "slack-bot-token", "app_token": "slack-app-token",
        "channels": {"planning": "C0CCCCCCC"}}}},
    "telegram": {"bots": {"ops": {
        "bot_id": 123456789, "bot_token": "telegram-bot-token", "dm": true}}},
    "http": {"listen": "127.0.0.1:0", "endpoints": {"reminders": {
        "issuer": "https://issuer.example", "audience": "bot", "key": "reminders-key"}}},
    "limits": {"per_principal_per_min": 5}
}"#;

#[test]
fn reads_a_full_section_and_layers_limits_over_defaults() {
    let host = HostConfig::from_json(SAMPLE).unwrap();
    assert_eq!(host.workspace_by_team("T0AAAAAAA").unwrap().0, "home");
    assert_eq!(host.telegram.bots["ops"].bot_id, 123456789);
    assert_eq!(host.limits().per_principal_per_min, 5);
    assert_eq!(host.limits().body_bytes, Limits::default().body_bytes);
    assert_eq!(host.listen().port(), 0);
    assert!(HostConfig::from_json("{}").is_ok());
}

fn rejects(json: &str) -> String {
    HostConfig::from_json(json).unwrap_err().to_string()
}

#[test]
fn a_plain_http_entrance_opens_beyond_loopback_only_when_the_operator_says_so() {
    assert!(HostConfig::from_json("{}").is_ok());
    for local in [
        "127.0.0.1:8788",
        "127.0.0.1:0",
        "127.8.9.10:8788",
        "[::1]:8788",
        "[::ffff:127.0.0.1]:8788",
    ] {
        let json = format!(r#"{{"http":{{"listen":"{local}"}}}}"#);
        assert!(HostConfig::from_json(&json).is_ok(), "{local}");
    }
    for beyond in [
        "0.0.0.0:8788",
        "[::]:8788",
        "192.168.0.10:8788",
        "10.0.0.5:8788",
        "203.0.113.7:8788",
        "[2001:db8::1]:8788",
        "[::ffff:10.0.0.5]:8788",
        "[::ffff:0.0.0.0]:8788",
    ] {
        let json = format!(r#"{{"http":{{"listen":"{beyond}"}}}}"#);
        let message = rejects(&json);
        assert!(
            message.starts_with("gateway.http.listen: ") && message.contains("http.remote"),
            "{beyond} -> {message}"
        );
        let ip = beyond.parse::<SocketAddr>().unwrap().ip().to_string();
        assert!(!message.contains(&ip), "{message}");
        let json = format!(r#"{{"http":{{"listen":"{beyond}","remote":true}}}}"#);
        assert_eq!(
            HostConfig::from_json(&json).unwrap().listen().to_string(),
            beyond,
            "{beyond}"
        );
    }
    assert!(
        rejects(r#"{"http":{"listen":"0.0.0.0:8788","remote":false}}"#).contains("http.remote")
    );
    let host = HostConfig::from_json(r#"{"http":{"remote":true}}"#).unwrap();
    assert_eq!(host.listen().to_string(), DEFAULT_LISTEN);
    assert!(HostConfig::from_json(r#"{"http":{"listen":"0.0.0.0:8788","remote":"yes"}}"#).is_err());
}

#[test]
fn rejects_unknown_fields_and_bad_shapes_without_echoing_values() {
    assert!(rejects(r#"{"slak": {}}"#).contains("slak"));
    let long_secret = "x".repeat(40);
    let message = rejects(&format!(
        r#"{{"slack":{{"workspaces":{{"home":{{"team_id":"T1","bot_token":"Bad Token {long_secret}","app_token":"a"}}}}}}}}"#
    ));
    assert!(
        message.contains("slack.workspaces.home.bot_token"),
        "{message}"
    );
    assert!(!message.contains(&long_secret));
    for (json, needle) in [
        (r#"{"cli":{"accounts":{"owner":{}}}}"#, "exactly one"),
        (
            r#"{"cli":{"accounts":{"owner":{"uid":1,"os_user":"x"}}}}"#,
            "exactly one",
        ),
        (r#"{"cli":{"accounts":{"Owner":{"uid":1}}}}"#, "alias"),
        (r#"{"cli":{"socket":"relative.sock"}}"#, "absolute path"),
        (r#"{"http":{"listen":"nope"}}"#, "host:port"),
        (
            r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b"}}}}"#,
            "app_token is required",
        ),
        (
            r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","receive":"http"}}}}"#,
            "slack.workspaces.home.receive: http receive is not supported yet",
        ),
        (
            r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","app_token":"a","signing_secret":"s","receive":"http"}}}}"#,
            "slack.workspaces.home.receive: http receive is not supported yet",
        ),
        (
            r#"{"slack":{"workspaces":{"home":{"team_id":"T1","bot_token":"b","app_token":"a","channels":{"dm":"D123"}}}}}"#,
            "dm binding",
        ),
        (
            r#"{"telegram":{"bots":{"Ops":{"bot_id":1,"bot_token":"tg-tok"}}}}"#,
            "alias",
        ),
        (
            r#"{"telegram":{"bots":{"ops":{"bot_id":0,"bot_token":"tg-tok"}}}}"#,
            "bot_id",
        ),
        (
            r#"{"telegram":{"bots":{"ops":{"bot_id":1,"bot_token":"Bad Token"}}}}"#,
            "telegram.bots.ops.bot_token",
        ),
        (r#"{"limits":{"body_bytes":0}}"#, "body_bytes"),
        (
            r#"{"http":{"endpoints":{"_requests":{"issuer":"i","audience":"a","key":"k"}}}}"#,
            "alias",
        ),
        (
            r#"{"http":{"endpoints":{"x":{"issuer":"i","audience":"a","key":"k","sync_wait_ms":0}}}}"#,
            "sync_wait_ms",
        ),
    ] {
        let message = rejects(json);
        assert!(message.contains(needle), "{json} -> {message}");
    }
}

#[test]
fn two_aliases_cannot_share_one_team_or_telegram_bot_id() {
    let message = rejects(
        r#"{"slack":{"workspaces":{
            "a":{"team_id":"T1","bot_token":"b","app_token":"t"},
            "b":{"team_id":"T1","bot_token":"b","app_token":"t"}}}}"#,
    );
    assert!(message.contains("duplicate team_id"), "{message}");
    let tg = rejects(
        r#"{"telegram":{"bots":{
            "a":{"bot_id":999,"bot_token":"t1","dm":true},
            "b":{"bot_id":999,"bot_token":"t2","dm":true}}}}"#,
    );
    assert!(tg.contains("duplicate bot_id"), "{tg}");
}

#[test]
fn slack_dm_sent_notice_validates_single_line_text_within_limit() {
    let ok =
        HostConfig::from_json(r#"{"slack":{"dm_sent_notice":"Approval sent via DM."}}"#).unwrap();
    assert_eq!(
        ok.slack.dm_sent_notice.as_deref(),
        Some("Approval sent via DM.")
    );
    let max_len = format!(r#"{{"slack":{{"dm_sent_notice":"{}"}}}}"#, "a".repeat(200));
    assert!(HostConfig::from_json(&max_len).is_ok());
    for bad in [
        r#"{"slack":{"dm_sent_notice":""}}"#,
        r#"{"slack":{"dm_sent_notice":"   "}}"#,
        r#"{"slack":{"dm_sent_notice":"line1\nline2"}}"#,
        &format!(r#"{{"slack":{{"dm_sent_notice":"{}"}}}}"#, "a".repeat(201)),
    ] {
        let message = rejects(bad);
        assert!(
            message.contains("slack.dm_sent_notice"),
            "{bad} -> {message}"
        );
    }
    let np = HostConfig::from_json(r#"{"slack":{"note_prefix":"※"}}"#).unwrap();
    assert_eq!(np.slack.note_prefix.as_deref(), Some("※"));
    for bad in [
        r#"{"slack":{"note_prefix":""}}"#,
        r#"{"slack":{"note_prefix":"- "}}"#,
        r#"{"slack":{"note_prefix":"*"}}"#,
        r#"{"slack":{"note_prefix":">"}}"#,
        r##"{"slack":{"note_prefix":"#"}}"##,
        r#"{"slack":{"note_prefix":"1."}}"#,
        r#"{"slack":{"note_prefix":"a b"}}"#,
        &format!(r#"{{"slack":{{"note_prefix":"{}"}}}}"#, "a".repeat(17)),
    ] {
        assert!(rejects(bad).contains("slack.note_prefix"), "{bad}");
    }
}
