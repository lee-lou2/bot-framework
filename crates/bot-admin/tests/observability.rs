//! 범용 OTLP(Langfuse·LangSmith·HTTPS TLS), OTLP 로그(`/v1/logs`), Sentry 호환 이슈 트래커(`/api/*/envelope/`) 통합 시험.

mod support;
use bot_admin::{
    fake::Fake,
    issues::{IssueConfig, IssueExporter, IssueKind, IssueOptions},
    logs::{LogConfig, LogEntry, LogExporter, LogLevel},
    obs::{HostHeaderValue, Options, SecretRef},
    trace::{
        Backend, Exporter, HeaderValue, Ids, ProfileTrace, Redactor, SpanIds, SpanKind, SpanRecord,
        Tracer, key,
    },
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    issue::{IssueEvent, IssueLevel, IssueReporter},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
    trace::now_nano,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use support::collector::{self, Attr, Mode};

struct MapSecrets(BTreeMap<(String, String), String>);
struct MapHandle(BTreeMap<String, String>);

impl SecretStore for MapSecrets {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let mine: BTreeMap<String, String> = self
            .0
            .iter()
            .filter(|((p, _), _)| *p == profile.0)
            .map(|((_, n), v)| (n.clone(), v.clone()))
            .collect();
        Box::pin(async move { Ok(Box::new(MapHandle(mine)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for MapHandle {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let v = self.0.get(name.as_str()).cloned();
        Box::pin(async move { v.map(|s| SecretValue(s.into_bytes())).ok_or(Error::Missing) })
    }
}

fn fast() -> Options {
    Options {
        queue: 256,
        batch: 32,
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(1000),
        backoff_min: Duration::from_millis(40),
        backoff_max: Duration::from_millis(120),
        trip_after: 2,
    }
}

#[tokio::test]
async fn langfuse_and_langsmith_share_universal_otlp_with_genai_and_identity_pseudonymization() {
    let (lf, ls) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let secrets = Arc::new(MapSecrets(BTreeMap::from([
        (
            ("personal".into(), "langfuse-auth".into()),
            "Basic cGstbGY6c2stbGY=".into(),
        ),
        (
            ("work".into(), "langsmith-key".into()),
            "lsv2_test_key_12345678".into(),
        ),
    ])));
    let cfg = BTreeMap::from([
        (
            ProfileId("personal".into()),
            ProfileTrace {
                backend: Some(Backend {
                    name: "langfuse".into(),
                    endpoint: lf.url_at("/api/public/otel/v1/traces"),
                    headers: vec![
                        (
                            "Authorization".into(),
                            HeaderValue::Secret(SecretName::parse("langfuse-auth").unwrap()),
                        ),
                        (
                            "x-langfuse-ingestion-version".into(),
                            HeaderValue::Plain("4".into()),
                        ),
                    ],
                    link: Some("https://cloud.langfuse.com/trace/{trace_id}".into()),
                    remote: false,
                }),
                ids: Ids::Hash,
                content: false,
                sample: 1.0,
            },
        ),
        (
            ProfileId("work".into()),
            ProfileTrace {
                backend: Some(Backend {
                    name: "langsmith".into(),
                    endpoint: ls.url_at("/otel/v1/traces"),
                    headers: vec![
                        (
                            "x-api-key".into(),
                            HeaderValue::Secret(SecretName::parse("langsmith-key").unwrap()),
                        ),
                        (
                            "Langsmith-Project".into(),
                            HeaderValue::Plain("bot-work".into()),
                        ),
                    ],
                    link: None,
                    remote: false,
                }),
                ids: Ids::Plain,
                content: false,
                sample: 1.0,
            },
        ),
    ]);
    let exporter = Exporter::start(cfg, Some(secrets), Redactor::default(), fast()).unwrap();
    for (p, provider, model) in [
        ("personal", "anthropic", "claude-sonnet-4-5"),
        ("work", "openai", "gpt-4o"),
    ] {
        let attempt = Attempt {
            profile: ProfileId(p.into()),
            run: RunId("r-1".into()),
            claim: None,
            generation: Generation(1),
        };
        let root_ids = SpanIds::attempt(&attempt);
        exporter.record(
            SpanRecord::start(&attempt.profile, "worker.run", root_ids)
                .attr(key::GEN_AI_OPERATION_NAME, "invoke_agent")
                .attr(key::USER_ID, "local:alice")
                .attr(key::SESSION_ID, "conv-42")
                .attr(key::GEN_AI_CONVERSATION_ID, "conv-42"),
        );
        exporter.record(
            SpanRecord::start(
                &attempt.profile,
                "worker.model",
                root_ids.child("model", "0"),
            )
            .kind(SpanKind::Client)
            .attr(key::GEN_AI_OPERATION_NAME, "chat")
            .attr(key::GEN_AI_PROVIDER_NAME, provider)
            .attr(key::GEN_AI_REQUEST_MODEL, "assistant")
            .attr(key::GEN_AI_RESPONSE_MODEL, model)
            .attr(key::GEN_AI_USAGE_INPUT_TOKENS, 120u64)
            .attr(key::GEN_AI_USAGE_OUTPUT_TOKENS, 45u64),
        );
    }
    assert!(exporter.flush(Duration::from_secs(5)).await);

    let lf_req = &lf.requests()[0];
    assert_eq!(lf_req.path, "/api/public/otel/v1/traces");
    assert!(
        lf_req
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Basic cGstbGY6c2stbGY=")
    );
    assert!(
        lf_req
            .headers
            .iter()
            .any(|(k, v)| k == "x-langfuse-ingestion-version" && v == "4")
    );
    let lf_spans = lf.spans();
    let lf_root = lf_spans.iter().find(|s| s.name == "worker.run").unwrap();
    assert_eq!(
        lf_root.str(key::USER_ID).map(str::len),
        Some(12),
        "기본 hash 모드는 user.id를 가명 처리한다"
    );
    assert_eq!(
        lf_root.str(key::SESSION_ID).map(str::len),
        Some(12),
        "기본 hash 모드는 session.id를 가명 처리한다"
    );
    assert_ne!(lf_root.str(key::USER_ID), Some("local:alice"));
    let lf_model = lf_spans.iter().find(|s| s.name == "worker.model").unwrap();
    assert_eq!(lf_model.kind, 3, "SpanKind::Client");
    assert_eq!(lf_model.str(key::GEN_AI_PROVIDER_NAME), Some("anthropic"));
    assert_eq!(
        lf_model.str(key::GEN_AI_RESPONSE_MODEL),
        Some("claude-sonnet-4-5")
    );
    assert_eq!(lf_model.int(key::GEN_AI_USAGE_INPUT_TOKENS), Some(120));

    let ls_req = &ls.requests()[0];
    assert_eq!(ls_req.path, "/otel/v1/traces");
    assert!(
        ls_req
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "lsv2_test_key_12345678")
    );
    assert!(
        ls_req
            .headers
            .iter()
            .any(|(k, v)| k == "langsmith-project" && v == "bot-work")
    );
    let ls_root = ls
        .spans()
        .into_iter()
        .find(|s| s.name == "worker.run")
        .unwrap();
    assert_eq!(ls_root.str(key::USER_ID), Some("local:alice"));
    assert_eq!(ls_root.str(key::SESSION_ID), Some("conv-42"));
}

#[tokio::test]
async fn otlp_logs_respect_level_redactor_secret_store_and_activation_gate() {
    let c = collector::start(Mode::Ok).await;
    let red = Redactor::new(["host-logs-secret-999".to_owned()]);
    let secrets = Arc::new(MapSecrets(BTreeMap::from([(
        ("host".into(), "logs-token".into()),
        "Bearer host-logs-secret-999".into(),
    )])));
    let cfg = LogConfig {
        name: "loki-otlp".into(),
        endpoint: c.url_at("/v1/logs"),
        headers: vec![(
            "Authorization".into(),
            HostHeaderValue::Secret(SecretRef::host(SecretName::parse("logs-token").unwrap())),
        )],
        level: LogLevel::Warn,
        remote: false,
    };
    let exporter = LogExporter::start(Some(cfg), Some(secrets), red, fast()).unwrap();
    let gate_open = Arc::new(AtomicBool::new(true));
    let gate_flag = gate_open.clone();
    exporter.set_gate(
        Arc::new(move || {
            let ok = gate_flag.load(Ordering::Relaxed);
            Box::pin(async move { ok })
        }),
        Duration::from_secs(1),
    );

    // Info는 min_level(Warn) 미만이라 전송하지 않는다
    exporter.record(LogEntry {
        timestamp_nano: now_nano(),
        level: LogLevel::Info,
        target: "bot_cli::serve".into(),
        message: "무시되는 info".into(),
        fields: Vec::new(),
        trace_id: None,
        span_id: None,
    });
    exporter.record(LogEntry {
        timestamp_nano: now_nano(),
        level: LogLevel::Warn,
        target: "bot_gateway::slack".into(),
        message: "경고 host-logs-secret-999 발생".into(),
        fields: vec![("run".into(), "r-9".into())],
        trace_id: Some([0x11; 16]),
        span_id: Some([0x22; 8]),
    });
    assert!(exporter.flush(Duration::from_secs(5)).await);

    let reqs = c.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/logs");
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Bearer host-logs-secret-999")
    );
    let logs = c.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].severity_number, 13);
    assert_eq!(logs[0].severity_text, "WARN");
    assert_eq!(logs[0].body, "경고 [redacted] 발생");
    assert_eq!(
        logs[0].attrs.get("code.namespace"),
        Some(&Attr::Str("bot_gateway::slack".into()))
    );
    assert_eq!(logs[0].attrs.get("run"), Some(&Attr::Str("r-9".into())));
    assert_eq!(logs[0].trace, vec![0x11; 16]);
    assert_eq!(logs[0].span, vec![0x22; 8]);

    // 활성화 문이 닫히면 큐의 로그를 보내지 않고 stale로 버린다
    gate_open.store(false, Ordering::Relaxed);
    exporter.record(LogEntry {
        timestamp_nano: now_nano(),
        level: LogLevel::Error,
        target: "bot_gateway::slack".into(),
        message: "철회 뒤 로그".into(),
        fields: Vec::new(),
        trace_id: None,
        span_id: None,
    });
    assert!(exporter.flush(Duration::from_secs(5)).await);
    assert_eq!(c.requests().len(), 1);
    let stats = exporter.stats();
    assert_eq!(
        (stats.sent, stats.dropped, stats.last_error),
        (1, 1, Some("stale"))
    );
}

#[tokio::test]
async fn sentry_issue_exporter_sends_envelopes_throttles_bursts_and_surfaces_in_admin_status() {
    let sentry = collector::start(Mode::Ok).await;
    let logs_col = collector::start(Mode::Ok).await;
    let red = Redactor::new(["rotated-sentry-secret".to_owned()]);
    let secrets = Arc::new(MapSecrets(BTreeMap::from([(
        ("host".into(), "sentry-key".into()),
        "pub_key_abc123".into(),
    )])));
    let issue_cfg = IssueConfig {
        kind: IssueKind::Sentry,
        name: "sentry-main".into(),
        endpoint: sentry.url_at("/api/42/envelope/"),
        key: Some(SecretRef::host(SecretName::parse("sentry-key").unwrap())),
        environment: Some("local".into()),
        release: Some("bot@0.1.0".into()),
        sample: 1.0,
        remote: false,
    };
    let issues = IssueExporter::start(
        Some(issue_cfg),
        Some(secrets),
        red.clone(),
        IssueOptions {
            runtime: fast(),
            cooldown: Duration::from_secs(30),
        },
    )
    .unwrap();
    let logs = LogExporter::start(
        Some(LogConfig {
            name: "otlp-logs".into(),
            endpoint: logs_col.url_at("/v1/logs"),
            headers: Vec::new(),
            level: LogLevel::Warn,
            remote: false,
        }),
        None,
        red,
        fast(),
    )
    .unwrap();

    // 같은 (kind, target) 연타는 쿨다운으로 한 번만 보낸다
    for _ in 0..3 {
        issues.report(
            IssueEvent::new(
                IssueLevel::Error,
                "tool_failed",
                "오류 rotated-sentry-secret 발생",
                "worker.run",
            )
            .profile(ProfileId("personal".into()))
            .trace_id([0xcd; 16])
            .span_id([0xef; 8])
            .tag("bot.run.id", "r-77"),
        );
    }
    assert!(issues.flush(Duration::from_secs(5)).await);

    let reqs = sentry.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/api/42/envelope/");
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-sentry-envelope")
    );
    assert!(reqs[0].headers.iter().any(|(k, v)| k == "x-sentry-auth"
        && v.contains("sentry_version=7")
        && v.contains("sentry_key=pub_key_abc123")));
    let body = String::from_utf8(reqs[0].body.clone()).unwrap();
    assert!(!body.contains("rotated-sentry-secret"));
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), 3);
    let item_hdr: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(item_hdr["length"], lines[2].len());
    let event: Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(event["level"], "error");
    assert_eq!(event["environment"], "local");
    assert_eq!(event["contexts"]["trace"]["type"], "trace");
    assert_eq!(event["contexts"]["trace"]["trace_id"], "cd".repeat(16));
    assert_eq!(event["contexts"]["trace"]["span_id"], "ef".repeat(8));
    assert_eq!(event["tags"]["bot.profile"], "personal");
    let st = issues.stats();
    assert_eq!((st.sent, st.dropped), (1, 2));

    // /v1/admin/status의 process.logs · process.issues에 origin만 보이고 경로·키는 노출되지 않는다
    let s = support::start_cfg(Fake::demo(), |cfg| cfg.logs(logs).issues(issues)).await;
    let status_json = support::get(&s, support::OPERATOR, "/v1/admin/status")
        .await
        .json();
    assert_eq!(status_json["process"]["logs"]["enabled"], true);
    assert_eq!(
        status_json["process"]["logs"]["backends"][0]["kind"],
        "otlp-logs"
    );
    assert_eq!(status_json["process"]["issues"]["enabled"], true);
    assert_eq!(
        status_json["process"]["issues"]["backends"][0]["kind"],
        "sentry"
    );
    assert_eq!(status_json["process"]["issues"]["sent"], 1);
    let raw_status = status_json.to_string();
    assert!(!raw_status.contains("/api/42/envelope/"));
    assert!(!raw_status.contains("pub_key_abc123"));
}

#[tokio::test]
async fn malformed_secrets_fail_closed_without_sending_unauthenticated_requests() {
    let col = collector::start(Mode::Ok).await;
    let secrets: Arc<dyn SecretStore> = Arc::new(MapSecrets(BTreeMap::from([
        (("crlf".into(), "k".into()), "Bearer ok\r\nX-Evil: 1".into()),
        (("utf8".into(), "k".into()), "Bearer 한글키".into()),
        (("trimmed".into(), "k".into()), "Basic Z29vZA==\n".into()),
        (("host".into(), "bad-log".into()), "a\nb".into()),
        (
            ("host".into(), "bad-sentry".into()),
            "key, sentry_secret=evil".into(),
        ),
    ])));

    let trace_cfg = BTreeMap::from(["crlf", "utf8", "trimmed"].map(|p| {
        (
            ProfileId(p.into()),
            ProfileTrace {
                backend: Some(Backend {
                    name: p.into(),
                    endpoint: col.url_at("/v1/traces"),
                    headers: vec![(
                        "Authorization".into(),
                        HeaderValue::Secret(SecretName::parse("k").unwrap()),
                    )],
                    link: None,
                    remote: false,
                }),
                ids: Ids::Plain,
                content: false,
                sample: 1.0,
            },
        )
    }));
    let traces = Exporter::start(
        trace_cfg,
        Some(secrets.clone()),
        Redactor::default(),
        fast(),
    )
    .unwrap();
    for p in ["crlf", "utf8", "trimmed"] {
        let a = Attempt {
            profile: ProfileId(p.into()),
            run: RunId("r-sec".into()),
            claim: None,
            generation: Generation(1),
        };
        traces.record(SpanRecord::start(
            &a.profile,
            "worker.run",
            SpanIds::attempt(&a),
        ));
    }
    assert!(traces.flush(Duration::from_secs(5)).await);
    for p in ["crlf", "utf8"] {
        let st = traces.stats(&ProfileId(p.into())).unwrap();
        assert_eq!((st.sent, st.failed, st.last_error), (0, 1, Some("secret")));
    }
    // 끝 줄바꿈이 있는 정상 비밀은 trim 후 정상 전송된다
    let ok_st = traces.stats(&ProfileId("trimmed".into())).unwrap();
    assert_eq!((ok_st.sent, ok_st.failed), (1, 0));

    let logs = LogExporter::start(
        Some(LogConfig {
            name: "bad-logs".into(),
            endpoint: col.url_at("/v1/logs"),
            headers: vec![(
                "Authorization".into(),
                HostHeaderValue::Secret(SecretRef::host(SecretName::parse("bad-log").unwrap())),
            )],
            level: LogLevel::Info,
            remote: false,
        }),
        Some(secrets.clone()),
        Redactor::default(),
        fast(),
    )
    .unwrap();
    logs.record(LogEntry {
        timestamp_nano: now_nano(),
        level: LogLevel::Error,
        target: "bot_cli::test".into(),
        message: "never sent".into(),
        fields: Vec::new(),
        trace_id: None,
        span_id: None,
    });
    assert!(logs.flush(Duration::from_secs(5)).await);
    let log_st = logs.stats();
    assert_eq!(
        (log_st.sent, log_st.failed, log_st.last_error),
        (0, 1, Some("secret"))
    );

    let issues = IssueExporter::start(
        Some(IssueConfig {
            kind: IssueKind::Sentry,
            name: "bad-sentry".into(),
            endpoint: col.url_at("/api/1/envelope/"),
            key: Some(SecretRef::host(SecretName::parse("bad-sentry").unwrap())),
            environment: None,
            release: None,
            sample: 1.0,
            remote: false,
        }),
        Some(secrets),
        Redactor::default(),
        IssueOptions {
            runtime: fast(),
            cooldown: Duration::ZERO,
        },
    )
    .unwrap();
    issues.report(IssueEvent::new(
        IssueLevel::Error,
        "err",
        "never sent",
        "bot_cli::test",
    ));
    assert!(issues.flush(Duration::from_secs(5)).await);
    let iss_st = issues.stats();
    assert_eq!(
        (iss_st.sent, iss_st.failed, iss_st.last_error),
        (0, 1, Some("secret"))
    );

    // 잘못된 비밀은 단 한 건도 전송되지 않고, 정상 trimmed 추적 1건만 도착했다
    let reqs = col.requests();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].path, "/v1/traces");
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "authorization" && v == "Basic Z29vZA==")
    );
}
