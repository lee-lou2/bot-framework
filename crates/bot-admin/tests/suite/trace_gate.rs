//! Collection withdrawal gating (`Exporter::set_gate`), live secret redaction, and trace fingerprinting.

use crate::support::collector::{self, Collector, Mode};
use bot_admin::{
    Fence,
    trace::{
        Backend, Exporter, HeaderValue, Ids, Options, ProfileTrace, Redactor, SpanIds, SpanRecord,
        Tracer, fingerprint, key,
    },
};
use bot_core::{
    id::{Attempt, Generation, ProfileId, RunId, SecretName},
    port::{ProfileSecrets, Reply, SecretStore, SecretValue},
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

fn fast() -> Options {
    Options {
        queue: 1024,
        batch: 64,
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(300),
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        trip_after: 2,
    }
}

fn pid(s: &str) -> ProfileId {
    ProfileId(s.to_owned())
}

fn trace_for(c: &Collector) -> ProfileTrace {
    ProfileTrace {
        backend: Some(Backend {
            remote: false,
            name: "main".into(),
            endpoint: c.url(),
            headers: Vec::new(),
            link: None,
        }),
        ids: Ids::Hash,
        content: false,
        sample: 1.0,
    }
}

fn span(run: &str) -> SpanRecord {
    let a = Attempt {
        profile: pid("alpha"),
        run: RunId(run.to_owned()),
        claim: None,
        generation: Generation(1),
    };
    SpanRecord::start(&a.profile, "attempt main", SpanIds::attempt(&a)).attr(key::BOT_RUN_ID, run)
}

async fn exporter(c: &Collector) -> Arc<Exporter> {
    let config = BTreeMap::from([(pid("alpha"), trace_for(c))]);
    Exporter::start(config, None, Redactor::default(), fast()).expect("start")
}

async fn drain(e: &Exporter) {
    assert!(
        e.flush(Duration::from_secs(10)).await,
        "queue did not drain"
    );
}

fn switch(on: &Arc<AtomicBool>) -> Fence {
    let on = on.clone();
    Arc::new(move || {
        let on = on.clone();
        Box::pin(async move { on.load(Ordering::SeqCst) })
    })
}

#[tokio::test]
async fn a_closed_gate_drops_the_queue_instead_of_sending_it_and_an_open_gate_resumes() {
    let c = collector::start(Mode::Ok).await;
    let e = exporter(&c).await;
    let current = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&current), Duration::from_secs(1)));

    e.record(span("r-open"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1);

    current.store(false, Ordering::SeqCst);
    for i in 0..5 {
        e.record(span(&format!("r-stale-{i}")));
    }
    drain(&e).await;
    assert_eq!(c.spans().len(), 1, "nothing exports while gate is closed");
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed, stats.circuit),
        (1, 5, 0, "closed"),
        "intentional drops do not count as failures or trip circuit"
    );
    assert_eq!(stats.last_error, Some("stale"));

    current.store(true, Ordering::SeqCst);
    e.record(span("r-back"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 2);
}

#[tokio::test]
async fn a_gate_that_cannot_answer_in_time_is_treated_as_closed() {
    let c = collector::start(Mode::Ok).await;
    let e = exporter(&c).await;
    let asked = Arc::new(AtomicUsize::new(0));
    let hang: Fence = {
        let asked = asked.clone();
        Arc::new(move || {
            asked.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        })
    };
    assert!(e.set_gate(hang, Duration::from_millis(100)));
    e.record(span("r-1"));
    drain(&e).await;
    assert!(asked.load(Ordering::SeqCst) >= 1, "gate was consulted");
    assert!(c.spans().is_empty(), "timed-out gate must not export");
    assert_eq!(e.stats(&pid("alpha")).unwrap().dropped, 1);
}

#[tokio::test]
async fn the_gate_is_installed_once_and_a_gateless_exporter_stays_open() {
    let c = collector::start(Mode::Ok).await;
    let e = exporter(&c).await;
    e.record(span("r-1"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1);
    let closed = Arc::new(AtomicBool::new(false));
    let open = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&closed), Duration::from_secs(1)));
    assert!(
        !e.set_gate(switch(&open), Duration::from_secs(1)),
        "installed gate cannot be replaced"
    );
    e.record(span("r-2"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1, "original closed gate stays in effect");
}

#[tokio::test]
async fn a_value_added_to_the_redactor_after_the_exporter_started_is_masked_in_later_exports() {
    const NEW: &str = "svc-credential-5be8029x";
    let c = collector::start(Mode::Ok).await;
    let mut trace = trace_for(&c);
    trace.content = true;
    let live = Redactor::default();
    let e = Exporter::start(
        BTreeMap::from([(pid("alpha"), trace)]),
        None,
        live.clone(),
        fast(),
    )
    .expect("start");
    let with_value =
        |run: &str| span(run).attr(key::GEN_AI_INPUT_MESSAGES, format!("call arg {NEW} end"));
    // Control: value does not match token shapes so it passes through until registered.
    e.record(with_value("r-before"));
    drain(&e).await;
    let raw = |c: &Collector, needle: &str| {
        c.requests()
            .iter()
            .any(|r| r.body.windows(needle.len()).any(|w| w == needle.as_bytes()))
    };
    assert!(raw(&c, NEW), "unregistered value passes through");
    live.add(NEW);
    let before = c.requests().len();
    e.record(with_value("r-after"));
    drain(&e).await;
    let later: Vec<_> = c.requests().into_iter().skip(before).collect();
    assert!(!later.is_empty(), "subsequent batch exported");
    assert!(
        later
            .iter()
            .all(|r| !r.body.windows(NEW.len()).any(|w| w == NEW.as_bytes())),
        "registered value must not appear in later exports"
    );
    assert!(
        later
            .iter()
            .any(|r| r.body.windows(10).any(|w| w == b"[redacted]")),
        "redaction placeholder present"
    );
}

/// Simulates a slow SecretStore; when `teaches` is set, adds resolved values to the redactor before returning (matching `Watched`).
struct Slow {
    value: String,
    hold: bool,
    entered: AtomicUsize,
    release: Notify,
    teaches: Option<Redactor>,
}

#[derive(Clone)]
struct Store(Arc<Slow>);

impl Store {
    fn new(value: &str, hold: bool, teaches: Option<Redactor>) -> Self {
        Self(Arc::new(Slow {
            value: value.to_owned(),
            hold,
            entered: AtomicUsize::new(0),
            release: Notify::new(),
            teaches,
        }))
    }

    async fn entered(&self, n: usize) {
        for _ in 0..1000 {
            if self.0.entered.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("read #{n} never started");
    }

    fn reads(&self) -> usize {
        self.0.entered.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.0.release.notify_one();
    }
}

impl SecretStore for Store {
    fn for_profile(&self, _: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let store = self.clone();
        Box::pin(async move { Ok(Box::new(store) as Box<dyn ProfileSecrets>) })
    }
}

impl ProfileSecrets for Store {
    fn resolve(&self, _: SecretName) -> Reply<'_, SecretValue> {
        let slow = self.0.clone();
        Box::pin(async move {
            slow.entered.fetch_add(1, Ordering::SeqCst);
            if slow.hold {
                slow.release.notified().await;
            }
            if let Some(red) = &slow.teaches {
                red.add(&slow.value);
            }
            Ok(SecretValue(slow.value.clone().into_bytes()))
        })
    }
}

fn with_credential(c: &Collector, content: bool) -> ProfileTrace {
    let mut trace = trace_for(c);
    trace.content = content;
    trace.backend.as_mut().unwrap().headers = vec![(
        "authorization".into(),
        HeaderValue::Secret(SecretName::parse("collector-credential").unwrap()),
    )];
    trace
}

fn exporter_reading(trace: ProfileTrace, store: &Store, redactor: Redactor) -> Arc<Exporter> {
    let secrets: Arc<dyn SecretStore> = Arc::new(store.clone());
    Exporter::start(
        BTreeMap::from([(pid("alpha"), trace)]),
        Some(secrets),
        redactor,
        fast(),
    )
    .expect("start")
}

fn authorization(request: &collector::Req) -> Option<&str> {
    request
        .headers
        .iter()
        .find(|(name, _)| name == "authorization")
        .map(|(_, value)| value.as_str())
}

fn holds(request: &collector::Req, needle: &str) -> bool {
    request
        .body
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Gate is re-checked after reading header credentials, and stale exporters skip reading credentials altogether.
#[tokio::test]
async fn a_withdrawal_while_the_header_credential_is_being_read_sends_nothing() {
    const CREDENTIAL: &str = "collector-credential-value-0001";
    let c = collector::start(Mode::Ok).await;
    let store = Store::new(CREDENTIAL, true, None);
    let e = exporter_reading(with_credential(&c, false), &store, Redactor::default());
    let current = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&current), Duration::from_secs(1)));

    e.record(span("r-open"));
    store.entered(1).await;
    store.release();
    drain(&e).await;
    assert_eq!(c.requests().len(), 1);
    assert_eq!(authorization(&c.requests()[0]), Some(CREDENTIAL));

    // Withdraw collection after the pre-read gate check passed, while secret resolution is in flight.
    e.record(span("r-withdrawn"));
    store.entered(2).await;
    current.store(false, Ordering::SeqCst);
    store.release();
    drain(&e).await;
    assert_eq!(c.requests().len(), 1, "exported after mid-read withdrawal");
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed, stats.circuit),
        (1, 1, 0, "closed"),
        "intentional drop is neither a failure nor a circuit trip"
    );
    assert_eq!(stats.last_error, Some("stale"));

    // Already-stale exporter must not consult SecretStore.
    e.record(span("r-after"));
    drain(&e).await;
    assert_eq!(store.reads(), 2, "stale exporter read secret store");
    assert_eq!(e.stats(&pid("alpha")).unwrap().dropped, 2);
    assert_eq!(c.requests().len(), 1);
}

/// Body serialization happens after header credential resolution so newly learned secrets are masked in the same batch.
#[tokio::test]
async fn a_value_learned_while_the_header_credential_is_read_is_masked_in_that_same_batch() {
    const NEW: &str = "svc-credential-5be8029x"; // non-token shape: only explicit redaction matches
    let c = collector::start(Mode::Ok).await;
    let live = Redactor::default();
    let store = Store::new(NEW, false, Some(live.clone()));
    let e = exporter_reading(with_credential(&c, true), &store, live.clone());
    e.record(span("r-1").attr(key::GEN_AI_INPUT_MESSAGES, format!("call arg {NEW} end")));
    drain(&e).await;
    let requests = c.requests();
    assert_eq!(requests.len(), 1);
    assert!(live.has_value(NEW), "secret read registered value");
    assert!(
        !holds(&requests[0], NEW),
        "learned secret leaked into request body"
    );
    assert!(
        holds(&requests[0], "[redacted]"),
        "redacted placeholder present"
    );
    assert_eq!(
        authorization(&requests[0]),
        Some(NEW),
        "credential sent in authorization header"
    );
}

/// In-flight requests already sent cannot be recalled (counted as `sent`); subsequent batches are gated.
#[tokio::test]
async fn a_request_that_already_started_is_counted_as_sent_and_the_next_batch_is_stopped() {
    let c = collector::start(Mode::Late(Duration::from_millis(150))).await;
    let e = exporter(&c).await;
    let current = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&current), Duration::from_secs(1)));
    e.record(span("r-started"));
    for _ in 0..1000 {
        if !c.requests().is_empty() {
            break; // request received by collector before delayed response
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(c.requests().len(), 1, "in-flight request never started");
    current.store(false, Ordering::SeqCst);
    drain(&e).await;
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed),
        (1, 0, 0),
        "in-flight batch completes as sent"
    );

    e.record(span("r-next"));
    drain(&e).await;
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!((stats.sent, stats.dropped, stats.failed), (1, 1, 0));
    assert_eq!(stats.last_error, Some("stale"));
    assert_eq!(c.requests().len(), 1);
}

fn with(mut trace: ProfileTrace, change: impl FnOnce(&mut ProfileTrace)) -> ProfileTrace {
    change(&mut trace);
    trace
}

fn backend(trace: &mut ProfileTrace) -> &mut Backend {
    trace.backend.as_mut().unwrap()
}

fn fp(trace: ProfileTrace) -> Option<[u8; 32]> {
    fingerprint(&BTreeMap::from([(pid("alpha"), trace)]))
}

fn base() -> ProfileTrace {
    ProfileTrace {
        backend: Some(Backend {
            remote: false,
            name: "main".into(),
            endpoint: "http://127.0.0.1:4318/v1/traces".into(),
            headers: vec![
                ("x-team".into(), HeaderValue::Plain("blue".into())),
                (
                    "authorization".into(),
                    HeaderValue::Secret(SecretName::parse("otlp-key").unwrap()),
                ),
            ],
            link: Some("https://trace.example/t/{trace_id}".into()),
        }),
        ids: Ids::Hash,
        content: false,
        sample: 1.0,
    }
}

#[test]
fn no_exporting_profile_means_no_fingerprint_so_hosts_without_collection_keep_their_revision() {
    assert_eq!(fingerprint(&BTreeMap::new()), None);
    assert_eq!(
        fingerprint(&BTreeMap::from([(pid("alpha"), ProfileTrace::default())])),
        None
    );
    let quiet = ProfileTrace {
        backend: None,
        ids: Ids::Plain,
        content: true,
        sample: 0.3,
    };
    assert_eq!(fingerprint(&BTreeMap::from([(pid("quiet"), quiet)])), None);
}

#[test]
fn every_setting_that_changes_what_leaves_the_host_changes_the_fingerprint() {
    let original = fp(base()).expect("exporting profile");
    assert_eq!(
        fp(base()),
        Some(original),
        "same config yields same fingerprint"
    );
    let changes: Vec<(&str, ProfileTrace)> = vec![
        (
            "endpoint",
            with(base(), |t| {
                backend(t).endpoint = "http://127.0.0.1:4319/v1/traces".into()
            }),
        ),
        (
            "added header",
            with(base(), |t| {
                backend(t)
                    .headers
                    .push(("x-more".into(), HeaderValue::Plain("1".into())))
            }),
        ),
        (
            "header name",
            with(base(), |t| backend(t).headers[0].0 = "x-squad".into()),
        ),
        (
            "plain header value",
            with(base(), |t| {
                backend(t).headers[0].1 = HeaderValue::Plain("red".into())
            }),
        ),
        (
            "secret name",
            with(base(), |t| {
                backend(t).headers[1].1 =
                    HeaderValue::Secret(SecretName::parse("other-key").unwrap())
            }),
        ),
        (
            "plain vs secret",
            with(base(), |t| {
                backend(t).headers[0].1 = HeaderValue::Secret(SecretName::parse("blue").unwrap())
            }),
        ),
        ("id mode", with(base(), |t| t.ids = Ids::Plain)),
        ("content opt-in", with(base(), |t| t.content = true)),
        ("sample rate", with(base(), |t| t.sample = 0.5)),
    ];
    for (what, trace) in changes {
        assert_ne!(
            fp(trace),
            Some(original),
            "changing {what} did not change fingerprint"
        );
    }
    assert_eq!(fp(with(base(), |t| t.backend = None)), None);
    let mut two = BTreeMap::from([(pid("alpha"), base())]);
    two.insert(pid("beta"), base());
    assert_ne!(fingerprint(&two), Some(original));
}

#[test]
fn labels_and_the_screen_link_do_not_change_the_fingerprint() {
    let original = fp(base());
    assert_eq!(
        fp(with(base(), |t| backend(t).name = "renamed".into())),
        original
    );
    assert_eq!(fp(with(base(), |t| backend(t).link = None)), original);
    assert_eq!(
        fp(with(base(), |t| t.sample = -0.0 + 1.0)),
        original,
        "sample 1.0 remains unchanged"
    );
}

#[test]
fn the_fingerprint_never_contains_a_plain_header_value() {
    // Plain values are hashed with their header name; avoid words like token/key so secret scanners ignore this synthetic literal.
    let value = "tok-1234567890-abcdef";
    let a = with(base(), |t| {
        backend(t).headers[0].1 = HeaderValue::Plain(value.into())
    });
    let b = with(base(), |t| {
        backend(t).headers[0] = ("x-other".into(), HeaderValue::Plain(value.into()))
    });
    let (fa, fb) = (fp(a).unwrap(), fp(b).unwrap());
    assert_ne!(fa, fb);
    assert!(
        !fa.windows(value.len()).any(|w| w == value.as_bytes()),
        "32-byte fingerprint must not contain raw value"
    );
}
