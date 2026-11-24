//! 수집 철회: 수집을 거두거나 바꾸면 활성화 지문이 달라지고(`fingerprint`), 그때 구판이 된 프로세스의 내보내기 문(`Exporter::set_gate`)은 닫혀
//! 큐에 있던 스팬까지 보내지 않고 버린다. 확인하지 못하는 동안(거짓·시간 초과)도 닫힌 것으로 본다. 문이 다시 열리면 보내기를 잇는다.
//! 문은 전송 **직전**에도 본다: 헤더 자격을 읽는 동안 철회돼도 보내지 않고, 그 읽기가 새로 알게 된 값(교체된 키)은 그 뒤에 만드는 본문에서 가려진다.
//! 이미 보내기 시작한 요청은 거둘 수 없어 `sent`로 센다.

mod support;
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
use support::collector::{self, Collector, Mode};
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
    Exporter::start(config, None, Redactor::default(), fast()).expect("시작")
}

async fn drain(e: &Exporter) {
    assert!(e.flush(Duration::from_secs(10)).await, "큐가 비지 않았다");
}

/// 켜고 끌 수 있는 문(참이면 «지금도 현재»).
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

    // 대조: 문이 열려 있으면 간다
    e.record(span("r-open"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1);

    // 수집이 거두어졌다(이 프로세스는 구판이 되었다): 새로 기록한 스팬은 가지 않고 버려진다
    current.store(false, Ordering::SeqCst);
    for i in 0..5 {
        e.record(span(&format!("r-stale-{i}")));
    }
    drain(&e).await;
    assert_eq!(c.spans().len(), 1, "닫힌 문 뒤로는 아무것도 가지 않는다");
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed, stats.circuit),
        (1, 5, 0, "closed"),
        "의도한 버림이라 실패·차단기와 무관하다"
    );
    assert_eq!(stats.last_error, Some("stale"));

    // 문이 다시 열리면(같은 판본으로 돌아왔다) 이어서 보낸다
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
    assert!(asked.load(Ordering::SeqCst) >= 1, "문을 확인했다");
    assert!(c.spans().is_empty(), "확인하지 못하면 보내지 않는다");
    assert_eq!(e.stats(&pid("alpha")).unwrap().dropped, 1);
}

#[tokio::test]
async fn the_gate_is_installed_once_and_a_gateless_exporter_stays_open() {
    let c = collector::start(Mode::Ok).await;
    let e = exporter(&c).await;
    // 문이 없으면(시작 중·시험) 열려 있다
    e.record(span("r-1"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1);
    let closed = Arc::new(AtomicBool::new(false));
    let open = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&closed), Duration::from_secs(1)));
    assert!(
        !e.set_gate(switch(&open), Duration::from_secs(1)),
        "이미 달린 문은 바꾸지 못한다"
    );
    e.record(span("r-2"));
    drain(&e).await;
    assert_eq!(c.spans().len(), 1, "처음 단 문(닫힘)이 그대로다");
}

/// 키를 교체했다: 시작 때 몰랐던 값도 가림 장치에 더하면(복제본을 쥔 호스트) 이미 도는 내보내기의 다음 글부터 가려진다.
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
    .expect("시작");
    let with_value =
        |run: &str| span(run).attr(key::GEN_AI_INPUT_MESSAGES, format!("호출 인자 {NEW} 끝"));
    // 대조: 아직 모르는 값(토큰 모양도 아니다)은 그대로 나간다
    e.record(with_value("r-before"));
    drain(&e).await;
    let raw = |c: &Collector, needle: &str| {
        c.requests()
            .iter()
            .any(|r| r.body.windows(needle.len()).any(|w| w == needle.as_bytes()))
    };
    assert!(raw(&c, NEW), "대조: 모르는 값은 가려지지 않는다");
    // 호스트가 새 값을 읽어 쓰기 시작하며 더한다
    live.add(NEW);
    let before = c.requests().len();
    e.record(with_value("r-after"));
    drain(&e).await;
    let later: Vec<_> = c.requests().into_iter().skip(before).collect();
    assert!(!later.is_empty(), "다음 글이 나갔다");
    assert!(
        later
            .iter()
            .all(|r| !r.body.windows(NEW.len()).any(|w| w == NEW.as_bytes())),
        "더한 뒤의 글에는 값이 없다"
    );
    assert!(
        later
            .iter()
            .any(|r| r.body.windows(10).any(|w| w == b"[redacted]")),
        "가린 자리가 남는다"
    );
}

/// 비밀을 읽는 데 오래 걸리는 보관소(느린 키체인·SOPS를 흉내 낸다). `hold`이면 읽기가 시작될 때 `entered`를 올리고 `release`가 올 때까지 값을 주지 않는다.
/// `teaches`가 있으면 값을 내주는 순간 그 가림 장치에 더한다 — 읽은 값을 쓰기 전에 가림 목록에 넣는 보관소 래퍼(`Watched`)와 같다.
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

    /// `n`번째 읽기가 시작될 때까지 기다린다.
    async fn entered(&self, n: usize) {
        for _ in 0..1000 {
            if self.0.entered.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{n}번째 읽기가 시작되지 않았다");
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

/// 헤더 하나(`authorization`)가 비밀 보관소의 값인 프로필 설정
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
    .expect("시작")
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

/// 헤더 자격을 읽는 사이에 수집이 거두어졌다: 읽기를 마쳐도 옛 목적지로 보내지 않는다. 읽기 전에 이미 구판이면 자격조차 읽지 않는다.
#[tokio::test]
async fn a_withdrawal_while_the_header_credential_is_being_read_sends_nothing() {
    const CREDENTIAL: &str = "collector-credential-value-0001";
    let c = collector::start(Mode::Ok).await;
    let store = Store::new(CREDENTIAL, true, None);
    let e = exporter_reading(with_credential(&c, false), &store, Redactor::default());
    let current = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&current), Duration::from_secs(1)));

    // 대조: 읽기가 느려도 문이 열려 있으면 그 자격을 단 헤더로 간다
    e.record(span("r-open"));
    store.entered(1).await;
    store.release();
    drain(&e).await;
    assert_eq!(c.requests().len(), 1);
    assert_eq!(authorization(&c.requests()[0]), Some(CREDENTIAL));

    // 읽기가 시작된 뒤(처음 문 확인은 지났다) 수집이 거두어졌다. 읽기를 마쳐도 가지 않는다
    e.record(span("r-withdrawn"));
    store.entered(2).await;
    current.store(false, Ordering::SeqCst);
    store.release();
    drain(&e).await;
    assert_eq!(c.requests().len(), 1, "읽는 사이에 거둔 수집으로 보냈다");
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed, stats.circuit),
        (1, 1, 0, "closed"),
        "의도한 버림: 실패도 차단기도 아니다"
    );
    assert_eq!(stats.last_error, Some("stale"));

    // 이미 구판이면 자격을 읽지도 않는다
    e.record(span("r-after"));
    drain(&e).await;
    assert_eq!(store.reads(), 2, "구판이 보관소를 읽었다");
    assert_eq!(e.stats(&pid("alpha")).unwrap().dropped, 2);
    assert_eq!(c.requests().len(), 1);
}

/// 헤더 자격을 읽다가 새로 알게 된 값(교체된 키)은 그 읽기 **뒤에** 만드는 본문에서 가려진다: 본문을 먼저 만들면 그 배치에서 값이 나간다.
#[tokio::test]
async fn a_value_learned_while_the_header_credential_is_read_is_masked_in_that_same_batch() {
    const NEW: &str = "svc-credential-5be8029x"; // 토큰 모양이 아니다: 가림 목록만 알아본다
    let c = collector::start(Mode::Ok).await;
    let live = Redactor::default();
    let store = Store::new(NEW, false, Some(live.clone()));
    let e = exporter_reading(with_credential(&c, true), &store, live.clone());
    e.record(span("r-1").attr(key::GEN_AI_INPUT_MESSAGES, format!("호출 인자 {NEW} 끝")));
    drain(&e).await;
    let requests = c.requests();
    assert_eq!(requests.len(), 1);
    assert!(live.has_value(NEW), "읽기가 값을 알려 줬다");
    assert!(
        !holds(&requests[0], NEW),
        "읽기가 알려 준 값이 본문에 실렸다"
    );
    assert!(holds(&requests[0], "[redacted]"), "가린 자리가 남는다");
    assert_eq!(
        authorization(&requests[0]),
        Some(NEW),
        "헤더에는 자격이 실려 간다"
    );
}

/// 전송을 시작한 요청은 그 뒤의 철회로 거둘 수 없다: 그 배치는 `dropped`가 아니라 `sent`로 세고, 그다음 배치부터 문이 막는다.
#[tokio::test]
async fn a_request_that_already_started_is_counted_as_sent_and_the_next_batch_is_stopped() {
    let c = collector::start(Mode::Late(Duration::from_millis(150))).await;
    let e = exporter(&c).await;
    let current = Arc::new(AtomicBool::new(true));
    assert!(e.set_gate(switch(&current), Duration::from_secs(1)));
    e.record(span("r-started"));
    for _ in 0..1000 {
        if !c.requests().is_empty() {
            break; // 수집기가 요청을 다 받았다(응답은 아직이다)
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    assert_eq!(c.requests().len(), 1, "전송이 시작되지 않았다");
    current.store(false, Ordering::SeqCst);
    drain(&e).await;
    let stats = e.stats(&pid("alpha")).unwrap();
    assert_eq!(
        (stats.sent, stats.dropped, stats.failed),
        (1, 0, 0),
        "시작한 전송은 되돌릴 수 없다"
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
    // 백엔드가 없는 프로필의 식별자·내용·표본 설정은 아무것도 내보내지 않으니 지문에 들어가지 않는다
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
    let original = fp(base()).expect("내보내는 프로필이 있다");
    assert_eq!(fp(base()), Some(original), "같은 설정은 같은 지문이다");
    let changes: Vec<(&str, ProfileTrace)> = vec![
        (
            "목적지",
            with(base(), |t| {
                backend(t).endpoint = "http://127.0.0.1:4319/v1/traces".into()
            }),
        ),
        (
            "헤더 추가",
            with(base(), |t| {
                backend(t)
                    .headers
                    .push(("x-more".into(), HeaderValue::Plain("1".into())))
            }),
        ),
        (
            "헤더 이름",
            with(base(), |t| backend(t).headers[0].0 = "x-squad".into()),
        ),
        (
            "평문 헤더 값",
            with(base(), |t| {
                backend(t).headers[0].1 = HeaderValue::Plain("red".into())
            }),
        ),
        (
            "비밀 이름",
            with(base(), |t| {
                backend(t).headers[1].1 =
                    HeaderValue::Secret(SecretName::parse("other-key").unwrap())
            }),
        ),
        (
            "평문↔비밀",
            with(base(), |t| {
                backend(t).headers[0].1 = HeaderValue::Secret(SecretName::parse("blue").unwrap())
            }),
        ),
        ("식별자 표기", with(base(), |t| t.ids = Ids::Plain)),
        ("내용 허용", with(base(), |t| t.content = true)),
        ("표본", with(base(), |t| t.sample = 0.5)),
    ];
    for (what, trace) in changes {
        assert_ne!(
            fp(trace),
            Some(original),
            "{what}을(를) 바꿨는데 지문이 그대로다"
        );
    }
    // 수집을 거두면(백엔드를 없앤다) 지문이 «없음»으로 바뀐다
    assert_eq!(fp(with(base(), |t| t.backend = None)), None);
    // 다른 프로필에 수집을 더하는 것도 바뀐다
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
        "표본 1.0은 그대로"
    );
}

#[test]
fn the_fingerprint_never_contains_a_plain_header_value() {
    // 평문 헤더 값이 토큰일 수 있으니 지문은 값의 해시일 뿐이고, 같은 값이어도 이름이 다르면 다르다
    // (값 이름에 token·key 같은 낱말을 쓰면 관문의 비밀 값 검사가 합성 값을 비밀로 읽는다)
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
        "지문(32바이트)은 값을 담을 수 없다"
    );
}
