//! 추적 내보내기 설정: 헤더 비밀은 프로필마다 자기 SecretStore에서, 잘못된 설정은 시작 전에 거절.

mod support;
use bot_admin::trace::{
    Backend, Exporter, HeaderValue, Ids, Options, ProfileTrace, Redactor, SpanIds, SpanRecord,
    Tracer,
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use support::collector::{self, Collector, Mode};

fn pid(s: &str) -> ProfileId {
    ProfileId(s.to_owned())
}

fn cfg(c: &Collector, ids: Ids) -> ProfileTrace {
    let backend = Backend {
        remote: false,
        name: "main".into(),
        endpoint: c.url(),
        headers: Vec::new(),
        link: None,
    };
    ProfileTrace {
        backend: Some(backend),
        ids,
        content: false,
        sample: 1.0,
    }
}

fn span(profile: &str, run: &str, name: &str) -> SpanRecord {
    let a = Attempt {
        profile: pid(profile),
        run: RunId(run.to_owned()),
        claim: None,
        generation: Generation(1),
    };
    SpanRecord::start(&a.profile, name, SpanIds::attempt(&a))
}

fn fast() -> Options {
    Options {
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(300),
        ..Options::default()
    }
}

async fn drain(e: &Exporter) {
    assert!(e.flush(Duration::from_secs(10)).await, "큐가 비지 않았다");
}

struct Store(BTreeMap<(String, String), String>);
struct Handle(BTreeMap<String, String>);

impl SecretStore for Store {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let mine: BTreeMap<String, String> = self
            .0
            .iter()
            .filter(|((p, _), _)| *p == profile.0)
            .map(|((_, n), v)| (n.clone(), v.clone()))
            .collect();
        Box::pin(async move { Ok(Box::new(Handle(mine)) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Handle {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let v = self.0.get(name.as_str()).cloned();
        Box::pin(async move { v.map(|s| SecretValue(s.into_bytes())).ok_or(Error::Missing) })
    }
}

#[tokio::test]
async fn header_secrets_come_from_each_profiles_own_secret_store() {
    let (a, b, c) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let with = |c: &Collector| {
        let mut t = cfg(c, Ids::Plain);
        if let Some(b) = &mut t.backend {
            b.headers = vec![
                (
                    "Authorization".into(),
                    HeaderValue::Secret(SecretName::parse("collector-token").expect("이름")),
                ),
                ("X-Static".into(), HeaderValue::Plain("one".into())),
            ];
        }
        t
    };
    let store = Store(BTreeMap::from([
        (
            ("alpha".into(), "collector-token".into()),
            "Bearer alpha-token".into(),
        ),
        (
            ("beta".into(), "collector-token".into()),
            "Bearer beta-token".into(),
        ),
    ]));
    let map: BTreeMap<ProfileId, ProfileTrace> = [
        (pid("alpha"), with(&a)),
        (pid("beta"), with(&b)),
        (pid("gamma"), with(&c)),
    ]
    .into();
    let e = Exporter::start(map, Some(Arc::new(store)), Redactor::default(), fast()).expect("시작");
    for p in ["alpha", "beta", "gamma"] {
        e.record(span(p, "r-1", "attempt main"));
    }
    drain(&e).await;
    let auth = |c: &Collector| {
        c.requests().first().and_then(|r| {
            r.headers
                .iter()
                .find(|(k, _)| k == "authorization")
                .map(|(_, v)| v.clone())
        })
    };
    assert_eq!(auth(&a).as_deref(), Some("Bearer alpha-token"));
    assert_eq!(
        auth(&b).as_deref(),
        Some("Bearer beta-token"),
        "같은 이름이어도 프로필마다 자기 값만"
    );
    assert!(
        c.requests().is_empty(),
        "비밀을 못 받으면 인증 없이 보내지 않는다"
    );
    assert_eq!(
        e.stats(&pid("gamma")).and_then(|s| s.last_error),
        Some("secret")
    );
    assert!(
        a.requests()[0]
            .headers
            .iter()
            .any(|(k, v)| k == "x-static" && v == "one")
    );
}

#[test]
fn bad_configuration_is_refused_before_anything_starts() {
    let backend = |endpoint: &str, header: &str, link: Option<&str>| ProfileTrace {
        backend: Some(Backend {
            remote: true, // 이 시험은 다른 규칙을 본다: `collector` 같은 이름도 받도록 명시한다(주소 규칙은 아래 시험이 따로 본다)
            name: "x".into(),
            endpoint: endpoint.into(),
            headers: vec![(header.into(), HeaderValue::Plain("v".into()))],
            link: link.map(str::to_owned),
        }),
        ..ProfileTrace::default()
    };
    let try_start = |t: ProfileTrace| {
        Exporter::start(
            [(pid("alpha"), t)].into(),
            None,
            Redactor::default(),
            Options::default(),
        )
    };
    for (what, t) in [
        ("ftp", backend("ftp://collector:4318", "X-A", None)),
        (
            "사용자 정보",
            backend("http://user:pw@collector:4318", "X-A", None),
        ),
        (
            "헤더 이름",
            backend("http://collector:4318", "bad name", None),
        ),
        (
            "링크 틀",
            backend(
                "http://collector:4318",
                "X-A",
                Some("https://traces.example/t/fixed"),
            ),
        ),
        (
            "링크 스킴",
            backend(
                "http://collector:4318",
                "X-A",
                Some("javascript:{trace_id}"),
            ),
        ),
        (
            "표본",
            ProfileTrace {
                sample: 1.5,
                ..ProfileTrace::default()
            },
        ),
    ] {
        assert!(try_start(t).is_err(), "{what}");
    }
    let ok = backend(
        "http://collector:4318",
        "X-A",
        Some("https://traces.example/t/{trace_id}"),
    );
    let err = try_start(ok).err().unwrap_or_default();
    assert!(
        err.contains("런타임"),
        "런타임 밖에서는 시작하지 않는다: {err}"
    );
    assert!(
        try_start(ProfileTrace::default()).is_ok(),
        "내보낼 곳이 없으면 런타임이 없어도 된다"
    );
}

/// 헤더 하나(자격 이름)를 단 설정. `remote`는 사설망 밖 평문 전송을 허용하는 운영자의 명시다.
fn at(endpoint: &str, remote: bool) -> ProfileTrace {
    ProfileTrace {
        backend: Some(Backend {
            name: "x".into(),
            endpoint: endpoint.into(),
            headers: vec![(
                "Authorization".into(),
                HeaderValue::Secret(SecretName::parse("collector-token").unwrap()),
            )],
            link: None,
            remote,
        }),
        ..ProfileTrace::default()
    }
}

/// 수집기 주소는 평문 http라 헤더 비밀과 내용이 그대로 나간다. 루프백·사설 주소(와 localhost)만 명시 없이 받는다.
#[test]
fn only_loopback_and_private_collectors_are_accepted_without_the_explicit_remote_option() {
    for host in [
        "127.0.0.1",
        "127.8.9.10",
        "[::1]",
        "localhost",
        "LOCALHOST",
        "collector.localhost",
        "10.0.0.5",
        "10.255.255.255",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.10",
        "169.254.10.10",
        "[fd00::1]",
        "[fc00::1]",
        "[fe80::1]",
        "[::ffff:10.1.2.3]",
        "[::ffff:127.0.0.1]",
    ] {
        let t = at(&format!("http://{host}:4318/v1/traces"), false);
        assert!(
            t.check(&pid("alpha")).is_ok(),
            "{host}: {:?}",
            t.check(&pid("alpha"))
        );
    }
}

#[test]
fn any_other_collector_is_refused_by_name_unless_remote_is_explicit() {
    for host in [
        // 공개 주소와 사설 대역 바로 옆
        "8.8.8.8",
        "1.1.1.1",
        "11.0.0.1",
        "172.15.255.255",
        "172.32.0.1",
        "192.167.0.1",
        "192.169.0.1",
        "100.64.0.1",
        "203.0.113.7",
        "0.0.0.0",
        "255.255.255.255",
        "[::]",
        "[2001:db8::1]",
        "[2606:4700::1111]",
        "[::ffff:8.8.8.8]",
        "[fec0::1]",
        // 이름은 어디로 풀릴지 모른다(`localhost`만 표준상 루프백이다)
        "collector.example.com",
        "collector",
        "otel-collector",
        "host.docker.internal",
        "localhost.example.com",
        "notlocalhost",
        "localhost.",
        // 루프백·사설처럼 보이지만 IP 리터럴이 아니다(시스템 해석기는 다른 곳으로 풀 수 있다)
        "127.0.0.1.example.com",
        "10.0.0.5.example.com",
        "2130706433",
        "0x7f.0.0.1",
        "127.1",
        "010.0.0.1",
    ] {
        let endpoint = format!("http://{host}:4318/v1/traces");
        let e = at(&endpoint, false).check(&pid("alpha")).expect_err(host);
        assert!(e.contains("alpha"), "프로필 이름을 밝힌다: {e}");
        assert!(
            e.contains(host.trim_matches(['[', ']'])),
            "호스트를 밝힌다: {host} → {e}"
        );
        assert!(
            e.contains("admin.trace.alpha.remote"),
            "고치는 법(옵션 이름)을 알린다: {e}"
        );
        assert!(
            at(&endpoint, true).check(&pid("alpha")).is_ok(),
            "명시하면 받는다: {host}"
        );
    }
}

#[test]
fn the_refusal_names_the_profile_and_the_host_but_nothing_else() {
    let e = at("http://collector.example.com:4318/v1/private-path", false)
        .check(&pid("alpha"))
        .unwrap_err();
    for shown in ["alpha", "collector.example.com", "admin.trace.alpha.remote"] {
        assert!(e.contains(shown), "{shown}: {e}");
    }
    for hidden in ["private-path", "4318", "collector-token", "Authorization"] {
        assert!(!e.contains(hidden), "{hidden}이(가) 오류에 실렸다: {e}");
    }
}

/// 허용 검사는 이름만 본다: `localhost`·`*.localhost`는 시스템 해석기가 무엇을 돌려주든 루프백으로 접속한다(`Host` 헤더만 원래 이름). 63자를 넘는 라벨은 시스템
/// 해석기가 풀지 못하는 이름이라, 해석기를 거치면 보내지 못하고 고정해 접속하면 루프백의 수집기에 닿는다.
#[tokio::test]
async fn a_localhost_collector_is_dialed_at_loopback_and_keeps_its_name_in_the_host_header() {
    let c = collector::start(Mode::Ok).await;
    let port = c.addr.port();
    let long = format!("{}.localhost", "a".repeat(64));
    for (n, name) in [
        "localhost".to_owned(),
        "collector.localhost".to_owned(),
        long,
    ]
    .into_iter()
    .enumerate()
    {
        let endpoint = format!("http://{name}:{port}/v1/traces");
        let e = Exporter::start(
            [(
                pid("alpha"),
                ProfileTrace {
                    backend: Some(Backend {
                        remote: false,
                        name: "x".into(),
                        endpoint,
                        headers: Vec::new(),
                        link: None,
                    }),
                    ..ProfileTrace::default()
                },
            )]
            .into(),
            None,
            Redactor::default(),
            fast(),
        )
        .expect("시작");
        e.record(span("alpha", &format!("r-{n}"), "attempt main"));
        drain(&e).await;
        let requests = c.requests();
        assert_eq!(
            requests.len(),
            n + 1,
            "{name}: 루프백의 수집기에 닿지 못했다"
        );
        let host = requests[n]
            .headers
            .iter()
            .find(|(k, _)| k == "host")
            .map(|(_, v)| v.as_str());
        assert_eq!(
            host,
            Some(format!("{name}:{port}").as_str()),
            "Host 헤더는 원래 이름"
        );
    }
}

/// `serve`(내보내기 시작)도 같은 규칙으로 시작 전에 멈춘다.
#[tokio::test]
async fn the_exporter_refuses_to_start_a_remote_collector_without_the_option() {
    let start = |remote| {
        Exporter::start(
            [(
                pid("alpha"),
                at("http://collector.example.com:4318/v1/traces", remote),
            )]
            .into(),
            None,
            Redactor::default(),
            Options::default(),
        )
    };
    let e = start(false).err().expect("명시 없이는 시작하지 않는다");
    assert!(
        e.contains("alpha") && e.contains("collector.example.com"),
        "{e}"
    );
    assert!(start(true).is_ok(), "명시하면 시작한다");
}

#[test]
fn diagnostic_output_never_shows_secret_values_or_plain_header_values() {
    let red = Redactor::new([
        "synthetic-secret-value".to_owned(),
        "other-secret".to_owned(),
    ]);
    let shown = format!("{red:?}");
    assert!(
        !shown.contains("synthetic") && !shown.contains("other-secret"),
        "{shown}"
    );
    assert!(shown.contains('2'), "개수만 보인다: {shown}");
    let t = ProfileTrace {
        backend: Some(Backend {
            remote: false,
            name: "main".into(),
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            headers: vec![
                (
                    "Authorization".into(),
                    HeaderValue::Plain("Bearer plain-token-xyz".into()),
                ),
                (
                    "X-Key".into(),
                    HeaderValue::Secret(SecretName::parse("collector-token").expect("이름")),
                ),
            ],
            link: None,
        }),
        ..ProfileTrace::default()
    };
    let shown = format!("{t:?}");
    assert!(!shown.contains("plain-token-xyz"), "{shown}");
    assert!(shown.contains("collector-token"), "비밀 이름은 보여도 된다");
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("제한 시간 안에 이루어지지 않았다: {what}");
}

#[tokio::test]
async fn dropping_the_exporter_drains_the_queue_then_stops_workers_and_releases_the_secret_store() {
    let c = collector::start(Mode::Ok).await;
    let store = Arc::new(Store(BTreeMap::from([(
        ("alpha".into(), "collector-token".into()),
        "Bearer alpha-token".into(),
    )])));
    let mut t = cfg(&c, Ids::Plain);
    if let Some(b) = &mut t.backend {
        b.headers = vec![(
            "Authorization".into(),
            HeaderValue::Secret(SecretName::parse("collector-token").expect("이름")),
        )];
    }
    let shared: Arc<dyn SecretStore> = store.clone();
    let e = Exporter::start(
        [(pid("alpha"), t)].into(),
        Some(shared),
        Redactor::default(),
        fast(),
    )
    .expect("시작");
    assert!(Arc::strong_count(&store) > 1, "작업이 SecretStore를 쥔다");
    for i in 0..3 {
        e.record(span("alpha", &format!("r-{i}"), "attempt main"));
    }
    drop(e);
    // 놓기 전에 큐에 있던 스팬은 끝까지 보내고, 작업은 끝나 SecretStore 참조를 놓는다.
    wait_until("남은 스팬 전송", || c.spans().len() == 3).await;
    wait_until("작업 종료와 참조 해제", || {
        Arc::strong_count(&store) == 1
    })
    .await;
}

#[tokio::test]
async fn a_hung_collector_cannot_keep_a_dropped_exporters_workers_alive_forever() {
    let c = collector::start(Mode::Hang).await;
    let store = Arc::new(Store(BTreeMap::new()));
    let shared: Arc<dyn SecretStore> = store.clone();
    let e = Exporter::start(
        [(pid("alpha"), cfg(&c, Ids::Plain))].into(),
        Some(shared),
        Redactor::default(),
        fast(),
    )
    .expect("시작");
    e.record(span("alpha", "r-1", "attempt main"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    drop(e);
    // 전송 제한 시간(300ms)이 지나면 작업이 끝난다.
    wait_until("작업 종료", || Arc::strong_count(&store) == 1).await;
}
