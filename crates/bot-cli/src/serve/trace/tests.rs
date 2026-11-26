//! 관측 조립: 이 프로세스가 비밀 보관소에서 읽어 쓰는 값은 쓰이기 전에 가림 장치에 들어가, 키를 교체해도 새 값이 내보내는 글과 관리 응답 검사에서 가려진다.
//! (내보내기·관리 응답 검사가 **같은** 살아 있는 목록을 쓰는지가 핵심이다.)

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

/// 값을 바꿀 수 있는 보관소(키 교체를 흉내 낸다).
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

/// 받은 요청 본문을 모으는 수집기. (주소, 본문들)
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
    // 시작할 때 읽은 키는 가려진다
    assert_eq!(read(&observe).await.unwrap(), OLD.as_bytes());
    // 운영자가 키를 교체했다. 새 값을 모델 서비스가 읽어 쓰는 순간(쓰이기 전에) 목록에 들어간다
    *shared.lock().unwrap() = NEW.to_owned();
    assert!(!observe.redactor.has_value(NEW), "읽기 전에는 모른다");
    assert_eq!(read(&observe).await.unwrap(), NEW.as_bytes());

    // 내보내는 글: 이미 도는 내보내기가 새 값도 가린다(내용 허용 프로필)
    observe
        .exporter
        .record(span("r1", &format!("호출 인자 {NEW} 끝, 예전 키 {OLD}")));
    assert!(observe.exporter.flush(Duration::from_secs(10)).await);
    assert!(!holds(&bodies, NEW), "새 키가 수집기에 갔다");
    assert!(!holds(&bodies, OLD), "옛 키가 수집기에 갔다");
    assert!(holds(&bodies, "[redacted]"), "가린 자리가 남는다");

    // 관리 응답 검사: 같은 목록을 쓰는 검사가 새 값이 섞인 응답을 막는다
    let config =
        AdminConfig::new("127.0.0.1:0".parse().unwrap()).leak_redactor(observe.redactor.clone());
    let passes = config.leak_check.expect("검사");
    assert!(!passes(
        &serde_json::json!({"memo": format!("앞 {NEW} 뒤")})
    ));
    assert!(!passes(&serde_json::json!({"memo": OLD})));
    assert!(passes(&serde_json::json!({"memo": "일반 글"})));
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
        "수집이 꺼져 있어도 관리 응답 검사는 값을 안다"
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

    // 정상 거절·취소(benign)는 이슈 트래커에 올리지 않는다
    tracer.record(span("r-cancel", "ok").fail("cancelled"));
    tracer.record(span("r-deny", "ok").fail("denied"));

    // 비정상 실패 스팬은 Sentry로 보낸다(trace_id + 태그 포함)
    tracer.record(
        span("r-fail", "ok")
            .attr(key::BOT_RUN_ID, "r-fail")
            .attr(key::GEN_AI_TOOL_NAME, "agenda.delete")
            .fail("execution_failed"),
    );

    // 민감 DM(bot.private=true) 실패 스팬은 부가 태그(run/agent/tool)를 싣지 않는다
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
        "benign 2건 제외, 비정상 실패 2건만 보고: {payloads:?}"
    );
    let normal = payloads
        .iter()
        .find(|p| p.contains("execution_failed"))
        .expect("일반 실패");
    assert!(normal.contains("agenda.delete") && normal.contains("r-fail"));
    assert!(normal.contains("\"trace_id\"") && normal.contains("\"span_id\""));
    let priv_ev = payloads
        .iter()
        .find(|p| p.contains("outcome_unknown"))
        .expect("민감 실패");
    assert!(
        !priv_ev.contains("r-priv-hidden") && !priv_ev.contains("inbox.secret"),
        "민감 DM 스팬은 부가 태그를 싣지 않는다: {priv_ev}"
    );
}
