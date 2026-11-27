//! Trace exporter config: per-profile header secrets, endpoint validation, redaction, and clean shutdown.

use crate::support::collector::{self, Collector, Mode};
use bot_admin::trace::{
    Backend, Exporter, HeaderValue, Ids, Options, ProfileTrace, Redactor, SpanIds, SpanRecord,
    Tracer,
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

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
    assert!(
        e.flush(Duration::from_secs(10)).await,
        "queue did not drain"
    );
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
                    HeaderValue::Secret(SecretName::parse("collector-token").expect("secret name")),
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
    let e =
        Exporter::start(map, Some(Arc::new(store)), Redactor::default(), fast()).expect("start");
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
        "same secret name must resolve per profile"
    );
    assert!(
        c.requests().is_empty(),
        "must not send unauthenticated requests when secret is missing"
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
            remote: true, // host-policy rules are tested separately below
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
            "userinfo",
            backend("http://user:pw@collector:4318", "X-A", None),
        ),
        (
            "header name",
            backend("http://collector:4318", "bad name", None),
        ),
        (
            "link template",
            backend(
                "http://collector:4318",
                "X-A",
                Some("https://traces.example/t/fixed"),
            ),
        ),
        (
            "link scheme",
            backend(
                "http://collector:4318",
                "X-A",
                Some("javascript:{trace_id}"),
            ),
        ),
        (
            "sample",
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
        err.to_ascii_lowercase().contains("runtime"),
        "must refuse start outside tokio runtime: {err}"
    );
    assert!(
        try_start(ProfileTrace::default()).is_ok(),
        "no-op exporter needs no runtime"
    );
}

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
        // Public addresses and boundaries just outside private ranges.
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
        // Hostnames can resolve anywhere (only RFC 6761 localhost is pinned to loopback).
        "collector.example.com",
        "collector",
        "otel-collector",
        "host.docker.internal",
        "localhost.example.com",
        "notlocalhost",
        "localhost.",
        // Non-canonical numeric/lookalike forms that are not IP literals.
        "127.0.0.1.example.com",
        "10.0.0.5.example.com",
        "2130706433",
        "0x7f.0.0.1",
        "127.1",
        "010.0.0.1",
    ] {
        let endpoint = format!("http://{host}:4318/v1/traces");
        let e = at(&endpoint, false).check(&pid("alpha")).expect_err(host);
        assert!(e.contains("alpha"), "error should name profile: {e}");
        assert!(
            e.contains(host.trim_matches(['[', ']'])),
            "error should name host: {host} -> {e}"
        );
        assert!(
            e.contains("admin.trace.alpha.remote"),
            "error should name remediation option: {e}"
        );
        assert!(
            at(&endpoint, true).check(&pid("alpha")).is_ok(),
            "explicit remote=true must allow {host}"
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
        assert!(!e.contains(hidden), "{hidden} leaked in error: {e}");
    }
}

/// `localhost` and `*.localhost` dial loopback directly while preserving the original `Host` header.
/// A 64-char label cannot be resolved by OS DNS, proving the dial bypasses DNS.
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
        .expect("start");
        e.record(span("alpha", &format!("r-{n}"), "attempt main"));
        drain(&e).await;
        let requests = c.requests();
        assert_eq!(
            requests.len(),
            n + 1,
            "{name}: failed to reach loopback collector"
        );
        let host = requests[n]
            .headers
            .iter()
            .find(|(k, _)| k == "host")
            .map(|(_, v)| v.as_str());
        assert_eq!(host, Some(format!("{name}:{port}").as_str()));
    }
}

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
    let e = start(false).err().expect("must refuse without remote=true");
    assert!(
        e.contains("alpha") && e.contains("collector.example.com"),
        "{e}"
    );
    assert!(start(true).is_ok());
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
    assert!(shown.contains('2'), "only count should appear: {shown}");
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
                    HeaderValue::Secret(SecretName::parse("collector-token").expect("secret name")),
                ),
            ],
            link: None,
        }),
        ..ProfileTrace::default()
    };
    let shown = format!("{t:?}");
    assert!(!shown.contains("plain-token-xyz"), "{shown}");
    assert!(shown.contains("collector-token"), "secret name may appear");
}

async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
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
            HeaderValue::Secret(SecretName::parse("collector-token").expect("secret name")),
        )];
    }
    let shared: Arc<dyn SecretStore> = store.clone();
    let e = Exporter::start(
        [(pid("alpha"), t)].into(),
        Some(shared),
        Redactor::default(),
        fast(),
    )
    .expect("start");
    assert!(Arc::strong_count(&store) > 1, "worker holds SecretStore");
    for i in 0..3 {
        e.record(span("alpha", &format!("r-{i}"), "attempt main"));
    }
    drop(e);
    wait_until("queued spans flushed", || c.spans().len() == 3).await;
    wait_until("worker exit and store release", || {
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
    .expect("start");
    e.record(span("alpha", "r-1", "attempt main"));
    tokio::time::sleep(Duration::from_millis(60)).await;
    drop(e);
    wait_until("worker exit after timeout", || {
        Arc::strong_count(&store) == 1
    })
    .await;
}
