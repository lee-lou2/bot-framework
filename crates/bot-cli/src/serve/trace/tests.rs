//! Tests verifying rotated secrets enter the shared leak redactor for both OTLP exports and Admin API responses.

use super::*;
use bot_admin::{
    AdminConfig,
    trace::{SpanIds, SpanRecord, key},
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretValue},
};
use bot_profile::Registry;
use std::sync::Mutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const OLD: &str = "service-key-OLD-71c3d9ab";
const NEW: &str = "service-key-NEW-5be8029f";

struct Rotating(Arc<Mutex<String>>);

struct Bound(Arc<Mutex<String>>);

impl SecretStore for Rotating {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let value = self.0.clone();
        Box::pin(async move { Ok(Box::new(Bound(value)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Bound {
    fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
        let value = self.0.lock().unwrap().clone().into_bytes();
        Box::pin(async move {
            if value.is_empty() {
                Err(Error::Missing)
            } else {
                Ok(SecretValue(value))
            }
        })
    }
}

async fn collector() -> (String, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/traces", listener.local_addr().unwrap());
    let bodies: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
    let sink = bodies.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let sink = sink.clone();
            tokio::spawn(async move {
                let (mut raw, mut chunk) = (Vec::new(), [0u8; 4096]);
                loop {
                    let Ok(n) = stream.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        sink.lock()
                            .unwrap()
                            .push(raw[end + 4..end + 4 + length].to_vec());
                        break;
                    }
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (url, bodies)
}

fn host(endpoint: &str) -> HostFile {
    crate::host::parse(&format!(
        r#"{{"version": 1, "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": 501}}}}}}}},
            "models": {{"personal": ["assistant"]}},
            "admin": {{"trace": {{"personal": {{"endpoint": "{endpoint}", "content": true}}}}}}}}"#
    ))
    .unwrap()
}

fn span(run: &str, text: &str) -> SpanRecord {
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.into()),
        claim: None,
        generation: Generation(1),
    };
    SpanRecord::start(&attempt.profile, "attempt main", SpanIds::attempt(&attempt))
        .attr(key::GEN_AI_INPUT_MESSAGES, text.to_owned())
}

fn holds(bodies: &Arc<Mutex<Vec<Vec<u8>>>>, needle: &str) -> bool {
    bodies
        .lock()
        .unwrap()
        .iter()
        .any(|body| body.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

#[tokio::test]
async fn a_rotated_key_that_is_read_afterwards_is_masked_in_exports_and_flagged_in_admin_responses()
{
    let (endpoint, bodies) = collector().await;
    let shared = Arc::new(Mutex::new(OLD.to_owned()));
    let observe = start(
        &host(&endpoint),
        &Registry::new(),
        Arc::new(Rotating(shared.clone())),
    )
    .await
    .unwrap();
    let read = |observe: &Observe| {
        let secrets = observe.secrets.clone();
        async move {
            let handle = secrets
                .for_profile(ProfileId("personal".into()))
                .await
                .unwrap();
            handle
                .resolve(SecretName::parse("model-key").unwrap())
                .await
                .map(|value| value.0)
        }
    };
    assert_eq!(read(&observe).await.unwrap(), OLD.as_bytes());
    *shared.lock().unwrap() = NEW.to_owned();
    assert!(!observe.redactor.has_value(NEW), "unknown before read");
    assert_eq!(read(&observe).await.unwrap(), NEW.as_bytes());

    observe
        .exporter
        .record(span("r1", &format!("call arg {NEW} end, old key {OLD}")));
    assert!(observe.exporter.flush(Duration::from_secs(10)).await);
    assert!(!holds(&bodies, NEW), "new key leaked to collector");
    assert!(!holds(&bodies, OLD), "old key leaked to collector");
    assert!(holds(&bodies, "[redacted]"), "redaction marker missing");

    let config =
        AdminConfig::new("127.0.0.1:0".parse().unwrap()).leak_redactor(observe.redactor.clone());
    let passes = config.leak_check.expect("leak check");
    assert!(!passes(
        &serde_json::json!({"memo": format!("before {NEW} after")})
    ));
    assert!(!passes(&serde_json::json!({"memo": OLD})));
    assert!(passes(&serde_json::json!({"memo": "ordinary text"})));
}

#[tokio::test]
async fn a_host_without_collection_still_watches_the_secrets_it_reads() {
    let shared = Arc::new(Mutex::new(NEW.to_owned()));
    let host = crate::host::parse(r#"{"version": 1}"#).unwrap();
    let observe = start(&host, &Registry::new(), Arc::new(Rotating(shared)))
        .await
        .unwrap();
    let handle = observe
        .secrets
        .for_profile(ProfileId("personal".into()))
        .await
        .unwrap();
    handle
        .resolve(SecretName::parse("model-key").unwrap())
        .await
        .unwrap();
    assert!(
        observe.redactor.has_value(NEW),
        "admin leak check must know read secrets even when tracing is disabled"
    );
}

#[tokio::test]
async fn reporting_tracer_forwards_non_benign_span_failures_to_sentry_and_skips_benign_and_private_tags()
 {
    let (sentry_url, bodies) = collector().await;
    let host = crate::host::parse(&format!(
        r#"{{"version": 1, "admin": {{"issues": {{"kind": "sentry", "endpoint": "{sentry_url}"}}}}}}"#
    ))
    .unwrap();
    let observe = start(
        &host,
        &Registry::new(),
        Arc::new(Rotating(Arc::new(Mutex::new(OLD.to_owned())))),
    )
    .await
    .unwrap();
    let tracer = observe.tracer();

    tracer.record(span("r-cancel", "ok").fail("cancelled"));
    tracer.record(span("r-deny", "ok").fail("denied"));

    tracer.record(
        span("r-fail", "ok")
            .attr(key::BOT_RUN_ID, "r-fail")
            .attr(key::GEN_AI_TOOL_NAME, "agenda.delete")
            .fail("execution_failed"),
    );

    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r-priv".into()),
        claim: None,
        generation: Generation(1),
    };
    tracer.record(
        SpanRecord::start(&attempt.profile, "private.tool", SpanIds::attempt(&attempt))
            .attr(key::BOT_PRIVATE, true)
            .attr(key::BOT_RUN_ID, "r-priv-hidden")
            .attr(key::GEN_AI_TOOL_NAME, "inbox.secret")
            .fail("outcome_unknown"),
    );

    observe.flush().await;
    let payloads: Vec<String> = bodies
        .lock()
        .unwrap()
        .iter()
        .map(|b| String::from_utf8(b.clone()).unwrap())
        .collect();
    assert_eq!(
        payloads.len(),
        2,
        "expected only 2 non-benign failures: {payloads:?}"
    );
    let normal = payloads
        .iter()
        .find(|p| p.contains("execution_failed"))
        .expect("normal failure");
    assert!(normal.contains("agenda.delete") && normal.contains("r-fail"));
    assert!(normal.contains("\"trace_id\"") && normal.contains("\"span_id\""));
    let priv_ev = payloads
        .iter()
        .find(|p| p.contains("outcome_unknown"))
        .expect("private failure");
    assert!(
        !priv_ev.contains("r-priv-hidden") && !priv_ev.contains("inbox.secret"),
        "private DM spans must omit extra tags: {priv_ev}"
    );
}
