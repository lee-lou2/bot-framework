//! Sentry Envelope v7 직렬화와 인증 헤더 생성(외부 Sentry SDK 의존 없음).
//! Sentry·GlitchTip·Bugsink 모두 같은 Envelope 형식(`application/x-sentry-envelope`)을 받는다.

use super::IssueConfig;
use crate::trace::Redactor;
use bot_core::issue::IssueEvent;
use serde_json::{Map, Value, json};

pub(super) const ENVELOPE_CONTENT_TYPE: &str = "application/x-sentry-envelope";
const MAX_MESSAGE_CHARS: usize = 2048;
const MAX_KIND_CHARS: usize = 128;
const MAX_TAG_VALUE_CHARS: usize = 200;
const MAX_TAGS: usize = 32;

fn cap(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        s.to_owned()
    } else {
        s.chars().take(limit).collect()
    }
}

/// UTC 나노초를 RFC 3339 문자열(`YYYY-MM-DDTHH:MM:SSZ`)로 바꾼다.
pub(super) fn rfc3339_utc(nano: u64) -> String {
    use std::time::{Duration, UNIX_EPOCH};
    let at = UNIX_EPOCH
        .checked_add(Duration::from_secs(nano / 1_000_000_000))
        .unwrap_or(UNIX_EPOCH);
    bot_core::schedule::utc::format_utc(at)
}

/// `X-Sentry-Auth` 헤더 값을 만든다(키가 있을 때만 호출한다). 키에 공백·쉼표·제어 문자가 섞이면 거절한다.
pub(super) fn auth_header(key: &str) -> Result<(String, String), &'static str> {
    let valid = !key.is_empty()
        && key.len() <= 256
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if !valid {
        return Err("secret");
    }
    Ok((
        "X-Sentry-Auth".to_owned(),
        format!(
            "Sentry sentry_version=7, sentry_client=bot/{}, sentry_key={key}",
            env!("CARGO_PKG_VERSION")
        ),
    ))
}

/// 가림 처리를 거친 `IssueEvent`를 Sentry Envelope v7 본문(3줄 NDJSON)으로 인코딩한다.
pub(super) fn encode_envelope(cfg: &IssueConfig, event: &IssueEvent, red: &Redactor) -> Vec<u8> {
    let event_id = crate::json::hex(&event.event_id);
    let sent_at = rfc3339_utc(event.timestamp_nano);
    let kind = cap(&red.mask(&event.kind), MAX_KIND_CHARS);
    let message = cap(&red.mask(&event.message), MAX_MESSAGE_CHARS);
    let target = cap(&red.mask(&event.target), MAX_KIND_CHARS);
    let mut tags = Map::new();
    tags.insert("component".into(), Value::String(target.clone()));
    if let Some(profile) = &event.profile {
        tags.insert("bot.profile".into(), Value::String(profile.0.clone()));
    }
    for (k, v) in event.tags.iter().take(MAX_TAGS) {
        let key_ok = !k.is_empty()
            && k.len() <= 32
            && k.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        if key_ok && !red.leaks_meta(k) {
            tags.insert(
                k.clone(),
                Value::String(cap(&red.mask(v), MAX_TAG_VALUE_CHARS)),
            );
        }
    }
    let mut payload = Map::new();
    payload.insert("event_id".into(), Value::String(event_id.clone()));
    payload.insert(
        "timestamp".into(),
        json!((event.timestamp_nano as f64) / 1_000_000_000.0),
    );
    payload.insert("platform".into(), Value::String("other".into()));
    payload.insert(
        "level".into(),
        Value::String(event.level.as_str().to_owned()),
    );
    payload.insert("logger".into(), Value::String(target.clone()));
    if let Some(env) = &cfg.environment {
        payload.insert("environment".into(), Value::String(env.clone()));
    }
    if let Some(release) = &cfg.release {
        payload.insert("release".into(), Value::String(release.clone()));
    }
    payload.insert("message".into(), json!({"formatted": message}));
    payload.insert(
        "exception".into(),
        json!({"values": [{"type": kind, "value": message}]}),
    );
    payload.insert("fingerprint".into(), json!([kind, target]));
    payload.insert("tags".into(), Value::Object(tags));
    if let Some(trace_id) = &event.trace_id {
        let mut trace_ctx = Map::new();
        trace_ctx.insert("type".into(), Value::String("trace".into()));
        trace_ctx.insert("trace_id".into(), Value::String(crate::json::hex(trace_id)));
        if let Some(span_id) = &event.span_id {
            trace_ctx.insert("span_id".into(), Value::String(crate::json::hex(span_id)));
        }
        payload.insert(
            "contexts".into(),
            json!({"trace": Value::Object(trace_ctx)}),
        );
    }
    let header_bytes = serde_json::to_vec(&json!({
        "event_id": event_id,
        "sent_at": sent_at,
        "sdk": {"name": "bot", "version": env!("CARGO_PKG_VERSION")},
    }))
    .unwrap_or_default();
    let payload_bytes = serde_json::to_vec(&Value::Object(payload)).unwrap_or_default();
    let item_bytes = serde_json::to_vec(&json!({
        "type": "event",
        "content_type": "application/json",
        "length": payload_bytes.len(),
    }))
    .unwrap_or_default();
    let mut out =
        Vec::with_capacity(header_bytes.len() + item_bytes.len() + payload_bytes.len() + 3);
    out.extend_from_slice(&header_bytes);
    out.push(b'\n');
    out.extend_from_slice(&item_bytes);
    out.push(b'\n');
    out.extend_from_slice(&payload_bytes);
    out.push(b'\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use bot_core::{id::ProfileId, issue::IssueLevel};

    #[test]
    fn formats_known_unix_timestamps_as_utc_rfc3339() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339_utc(1_700_000_000_000_000_000),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(
            rfc3339_utc(1_709_164_800_000_000_000),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn auth_header_rejects_parameter_injection_and_control_bytes() {
        assert!(auth_header("pub_key-123.abc").is_ok());
        for bad in [
            "",
            "abc, sentry_secret=evil",
            "abc def",
            "abc\r\nX-Inject: 1",
            "키",
        ] {
            assert_eq!(auth_header(bad), Err("secret"), "{bad:?}");
        }
    }

    #[test]
    fn envelope_has_three_json_lines_with_length_span_id_and_redaction() {
        let cfg = IssueConfig {
            kind: super::super::IssueKind::Sentry,
            name: "sentry".into(),
            endpoint: "http://127.0.0.1:9999/api/1/envelope/".into(),
            key: None,
            environment: Some("staging".into()),
            release: Some("bot@0.1.0".into()),
            sample: 1.0,
            remote: false,
        };
        let red = Redactor::new(["super-secret-xyz".to_owned()]);
        let ev = IssueEvent::new(
            IssueLevel::Error,
            "tool_failed",
            "실패 super-secret-xyz 끝",
            "bot_broker::service",
        )
        .profile(ProfileId("personal".into()))
        .trace_id([0xab; 16])
        .span_id([0xef; 8])
        .tag("bot.run.id", "r-1");
        let raw = String::from_utf8(encode_envelope(&cfg, &ev, &red)).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        let h: Value = serde_json::from_str(lines[0]).unwrap();
        let ih: Value = serde_json::from_str(lines[1]).unwrap();
        let body: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(h["event_id"], body["event_id"]);
        assert_eq!(ih["type"], "event");
        assert_eq!(ih["length"], lines[2].len());
        assert_eq!(body["level"], "error");
        assert_eq!(body["environment"], "staging");
        assert_eq!(body["release"], "bot@0.1.0");
        assert_eq!(body["contexts"]["trace"]["type"], "trace");
        assert_eq!(body["contexts"]["trace"]["trace_id"], "ab".repeat(16));
        assert_eq!(body["contexts"]["trace"]["span_id"], "ef".repeat(8));
        assert_eq!(body["tags"]["bot.profile"], "personal");
        assert_eq!(body["tags"]["bot.run.id"], "r-1");
        assert!(!raw.contains("super-secret-xyz"));
        assert!(raw.contains("[redacted]"));
    }
}
