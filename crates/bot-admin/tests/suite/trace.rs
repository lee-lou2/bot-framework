//! Trace exporter: profile isolation, fault isolation, content/identity redaction, and deterministic trace IDs.

use crate::support::collector::{self, Attr, Collector, Mode};
use bot_admin::trace::{
    Backend, Exporter, Ids, Options, ProfileTrace, Redactor, SpanIds, SpanRecord, Tracer, key,
    sample_hit,
};
use bot_core::id::{Attempt, Generation, ProfileId, RunId};
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

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

fn attempt(profile: &str, run: &str, generation: u64) -> Attempt {
    Attempt {
        profile: pid(profile),
        run: RunId(run.to_owned()),
        claim: None,
        generation: Generation(generation),
    }
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
    let a = attempt(profile, run, 1);
    SpanRecord::start(&a.profile, name, SpanIds::attempt(&a)).attr(key::BOT_RUN_ID, run)
}

async fn start(map: Vec<(&str, ProfileTrace)>, opts: Options) -> Arc<Exporter> {
    let cfg: BTreeMap<ProfileId, ProfileTrace> =
        map.into_iter().map(|(k, v)| (pid(k), v)).collect();
    Exporter::start(cfg, None, Redactor::default(), opts).expect("start")
}

async fn drain(e: &Exporter) {
    assert!(
        e.flush(Duration::from_secs(10)).await,
        "queue did not drain"
    );
}

fn raw_contains(c: &Collector, needle: &str) -> bool {
    c.requests()
        .iter()
        .any(|r| r.body.windows(needle.len()).any(|w| w == needle.as_bytes()))
}

#[tokio::test]
async fn isolates_profile_traces() {
    let (a, b) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let e = start(
        vec![
            ("alpha", cfg(&a, Ids::Plain)),
            ("beta", cfg(&b, Ids::Plain)),
            ("quiet", ProfileTrace::default()),
        ],
        fast(),
    )
    .await;
    for i in 0..5 {
        e.record(
            span("alpha", &format!("r-{i}"), "attempt main").attr(key::BOT_AGENT, "alpha-agent"),
        );
        e.record(
            span("beta", &format!("r-{i}"), "attempt main").attr(key::BOT_AGENT, "beta-agent"),
        );
    }
    e.record(span("quiet", "r-q", "attempt main"));
    e.record(span("gamma", "r-g", "attempt main"));
    drain(&e).await;
    let (sa, sb) = (a.spans(), b.spans());
    assert_eq!((sa.len(), sb.len()), (5, 5));
    for s in &sa {
        assert_eq!(s.str("bot.profile"), Some("alpha"));
        assert_eq!(
            s.resource.get("bot.profile"),
            Some(&Attr::Str("alpha".into()))
        );
        assert_eq!(s.str("bot.agent"), Some("alpha-agent"));
    }
    for s in &sb {
        assert_eq!(
            (s.str("bot.profile"), s.str("bot.agent")),
            (Some("beta"), Some("beta-agent"))
        );
    }
    assert!(
        sa.iter().all(|x| sb.iter().all(|y| x.trace != y.trace)),
        "trace IDs must not collide across profiles"
    );
    assert!(
        !raw_contains(&a, "beta") && !raw_contains(&b, "alpha"),
        "profile payloads must not cross collectors"
    );
    assert!(
        e.stats(&pid("quiet"))
            .is_some_and(|s| !s.enabled && s.sent == 0 && s.queued == 0)
    );
    assert!(
        e.stats(&pid("gamma")).is_none(),
        "unregistered profile must not export"
    );
    assert_eq!(
        e.stats(&pid("alpha"))
            .map(|s| (s.sent, s.dropped, s.failed)),
        Some((5, 0, 0))
    );
}

#[tokio::test]
async fn exporter_failure_never_blocks_execution() {
    let c = collector::start(Mode::Hang).await;
    let opts = Options {
        queue: 64,
        batch: 16,
        ..fast()
    };
    let e = start(vec![("alpha", cfg(&c, Ids::Plain))], opts).await;
    let started = Instant::now();
    for i in 0..5000 {
        e.record(span("alpha", &format!("r-{i}"), "attempt main"));
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // Recording from a non-Tokio thread must also stay non-blocking.
    let e2 = e.clone();
    std::thread::spawn(move || e2.record(span("alpha", "r-thread", "attempt main")))
        .join()
        .expect("thread");
    drain(&e).await;
    let s = e.stats(&pid("alpha")).expect("stats");
    assert_eq!(
        s.sent + s.failed + s.dropped,
        5001,
        "{s:?}: every span must be accounted for"
    );
    assert_eq!((s.sent, s.queued), (0, 0));
    assert!(s.dropped > 4000 && s.failed > 0, "{s:?}");
    assert_eq!((s.circuit, s.last_error), ("open", Some("timeout")));
    // Circuit closes and exports resume once the collector recovers.
    c.set(Mode::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..3 {
        e.record(span("alpha", &format!("again-{i}"), "attempt main"));
    }
    drain(&e).await;
    let s = e.stats(&pid("alpha")).expect("stats");
    assert_eq!((s.sent, s.circuit), (3, "closed"), "{s:?}");
    assert_eq!(c.spans().len(), 3);
}

#[tokio::test]
async fn refused_and_failing_collectors_are_counted_by_class() {
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let dead = (format!("http://127.0.0.1:{port}/v1/traces"), ());
    let broken = collector::start(Mode::Status(500)).await;
    let reset = collector::start(Mode::Reset).await;
    let backend = |url: String| ProfileTrace {
        backend: Some(Backend {
            remote: false,
            name: "x".into(),
            endpoint: url,
            headers: Vec::new(),
            link: None,
        }),
        ids: Ids::Plain,
        content: false,
        sample: 1.0,
    };
    let e = start(
        vec![
            ("refused", backend(dead.0)),
            ("broken", cfg(&broken, Ids::Plain)),
            ("reset", cfg(&reset, Ids::Plain)),
        ],
        fast(),
    )
    .await;
    for p in ["refused", "broken", "reset"] {
        e.record(span(p, "r-1", "attempt main"));
    }
    drain(&e).await;
    let class = |p: &str| e.stats(&pid(p)).and_then(|s| s.last_error);
    assert_eq!(
        (class("refused"), class("broken")),
        (Some("connect"), Some("status"))
    );
    assert!(
        matches!(class("reset"), Some("io" | "connect")),
        "{:?}",
        class("reset")
    );
    for p in ["refused", "broken", "reset"] {
        assert_eq!(
            e.stats(&pid(p)).map(|s| (s.sent, s.failed)),
            Some((0, 1)),
            "{p}"
        );
    }
}

#[tokio::test]
async fn content_is_excluded_by_default_and_masked_when_enabled() {
    let (plain, rich) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let mut on = cfg(&rich, Ids::Plain);
    on.content = true;
    let cfg_map: BTreeMap<ProfileId, ProfileTrace> =
        [(pid("plain"), cfg(&plain, Ids::Plain)), (pid("rich"), on)].into();
    let red = Redactor::new(["zq7-secret-value".to_owned()]);
    let e = Exporter::start(cfg_map, None, red, fast()).expect("start");
    for p in ["plain", "rich"] {
        e.record(
            span(p, "r-1", "chat assistant")
                .attr(key::GEN_AI_INPUT_MESSAGES, "hello zq7-secret-value request")
                .attr(key::GEN_AI_TOOL_CALL_ARGUMENTS, "{\"event\":\"synthetic\"}")
                .attr("bot.approval.nonce", "ap-secret-nonce")
                .attr(key::GEN_AI_REQUEST_MODEL, "assistant"),
        );
    }
    drain(&e).await;
    let (p, r) = (&plain.spans()[0], &rich.spans()[0]);
    for k in [
        key::GEN_AI_INPUT_MESSAGES,
        key::GEN_AI_TOOL_CALL_ARGUMENTS,
        "bot.approval.nonce",
    ] {
        assert!(
            !p.attrs.contains_key(k),
            "content must be excluded by default: {k}"
        );
    }
    assert_eq!(
        (p.str("bot.content.dropped"), p.int("bot.dropped")),
        (Some("off"), Some(3))
    );
    assert_eq!(p.str("gen_ai.request.model"), Some("assistant"));
    assert_eq!(
        r.str(key::GEN_AI_INPUT_MESSAGES),
        Some("hello [redacted] request")
    );
    assert_eq!(
        r.str(key::GEN_AI_TOOL_CALL_ARGUMENTS),
        Some("{\"event\":\"synthetic\"}")
    );
    assert!(!r.attrs.contains_key("bot.approval.nonce") && r.int("bot.dropped") == Some(1));
    for c in [&plain, &rich] {
        assert!(
            !raw_contains(c, "zq7-secret-value")
                && !raw_contains(c, "ap-secret-nonce")
                && !raw_contains(c, "nonce")
        );
    }
}

#[tokio::test]
async fn identities_are_pseudonymized_unless_the_profile_asks_for_plain_ids() {
    let (hashed, plain) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let e = start(
        vec![
            ("hashed", cfg(&hashed, ProfileTrace::default().ids)),
            ("plain", cfg(&plain, Ids::Plain)),
        ],
        fast(),
    )
    .await;
    for p in ["hashed", "plain"] {
        e.record(
            span(p, "r-1", "attempt main")
                .attr(key::BOT_PRINCIPAL, "local:owner")
                .attr(key::BOT_AGENT, "main"),
        );
    }
    drain(&e).await;
    let h = &hashed.spans()[0];
    let pseudo = h.str("bot.principal").expect("pseudonym");
    assert_eq!(pseudo.len(), 12);
    assert!(!raw_contains(&hashed, "owner") && h.str("bot.agent") == Some("main"));
    assert_eq!(plain.spans()[0].str("bot.principal"), Some("local:owner"));
}

#[tokio::test]
async fn spans_of_one_attempt_from_different_components_join_one_trace() {
    let c = collector::start(Mode::Ok).await;
    let e = start(vec![("alpha", cfg(&c, Ids::Plain))], fast()).await;
    let a = attempt("alpha", "r-9", 2);
    // Worker and broker derive matching trace/parent IDs independently from Attempt.
    let worker = SpanRecord::start(&a.profile, "attempt main", SpanIds::attempt(&a));
    let broker = SpanRecord::start(
        &a.profile,
        "execute_tool agenda.delete",
        SpanIds::attempt(&a).child("tool", "c-1"),
    )
    .attr(key::GEN_AI_OPERATION_NAME, "execute_tool")
    .attr(key::GEN_AI_TOOL_TYPE, "function")
    .attr(key::GEN_AI_TOOL_NAME, "agenda.delete")
    .attr(key::GEN_AI_TOOL_CALL_ID, "c-1")
    .attr(key::BOT_TOOL_RISK, "irreversible")
    .event("bot.effect", vec![(key::BOT_EFFECT_STEP, "upload".into())])
    .link([7; 16], [8; 8])
    .fail("tool_failed");
    e.record(worker);
    e.record(broker);
    let other = attempt("alpha", "r-9", 3);
    e.record(SpanRecord::start(
        &other.profile,
        "attempt main",
        SpanIds::attempt(&other),
    ));
    drain(&e).await;
    let all = c.spans();
    let by = |name: &str, n: usize| {
        all.iter()
            .filter(|s| s.name == name)
            .nth(n)
            .cloned()
            .expect("span")
    };
    let (w, t, w2) = (
        by("attempt main", 0),
        by("execute_tool agenda.delete", 0),
        by("attempt main", 1),
    );
    let ids = SpanIds::attempt(&a);
    assert_eq!(
        (w.trace.as_slice(), w.span.as_slice(), w.parent.len()),
        (ids.trace.as_slice(), ids.span.as_slice(), 0)
    );
    assert_eq!(
        (t.trace.as_slice(), t.parent.as_slice()),
        (w.trace.as_slice(), w.span.as_slice()),
        "tool span must parent to the attempt root"
    );
    assert_ne!(w2.trace, w.trace, "new generation starts a new trace");
    assert_eq!(t.str(key::GEN_AI_OPERATION_NAME), Some("execute_tool"));
    assert_eq!(t.str(key::GEN_AI_TOOL_TYPE), Some("function"));
    assert_eq!(t.str(key::GEN_AI_TOOL_NAME), Some("agenda.delete"));
    assert_eq!(t.str(key::GEN_AI_TOOL_CALL_ID), Some("c-1"));
    assert!(
        t.int(key::BOT_DROPPED).is_none(),
        "standard attributes pass allowlist untouched"
    );
    assert_eq!(t.status, (2, "tool_failed".to_owned()));
    assert_eq!(
        (t.links.len(), t.events.len(), t.events[0].0.as_str()),
        (1, 1, "bot.effect")
    );
    assert_eq!(
        t.events[0].2.get("bot.effect.step"),
        Some(&Attr::Str("upload".into()))
    );
    assert!(t.start > 1_600_000_000_000_000_000 && t.end >= t.start);
    assert_eq!(
        (t.scope.0.as_str(), t.resource.get("service.name")),
        ("bot", Some(&Attr::Str("bot".into())))
    );
    assert!(matches!(t.resource.get("service.instance.id"), Some(Attr::Str(s)) if s.len() == 12));
    let post = &c.requests()[0];
    assert_eq!(post.path, "/v1/traces");
    assert!(
        post.headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-protobuf")
    );
}

#[tokio::test]
async fn sampling_follows_the_trace_id_and_zero_exports_nothing() {
    let (half, none) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let mut h = cfg(&half, Ids::Plain);
    h.sample = 0.5;
    let mut z = cfg(&none, Ids::Plain);
    z.sample = 0.0;
    let e = start(vec![("half", h), ("none", z)], fast()).await;
    let runs: Vec<String> = (0..200).map(|i| format!("r-{i}")).collect();
    for r in &runs {
        e.record(span("half", r, "attempt main"));
        e.record(span("none", r, "attempt main"));
    }
    drain(&e).await;
    let want: usize = runs
        .iter()
        .filter(|r| sample_hit(&SpanIds::attempt(&attempt("half", r, 1)).trace, 0.5))
        .count();
    assert_eq!(
        half.spans().len(),
        want,
        "deterministic trace-ID sampling across processes"
    );
    assert!((60..140).contains(&want), "{want}");
    assert!(none.spans().is_empty());
}

#[tokio::test]
async fn oversized_spans_are_dropped_at_record_time_instead_of_filling_the_queue() {
    let c = collector::start(Mode::Ok).await;
    let e = start(vec![("alpha", cfg(&c, Ids::Plain))], fast()).await;
    e.record(span("alpha", "r-big", "attempt main").attr(key::BOT_AGENT, "x".repeat(1024 * 1024)));
    e.record(span("alpha", "r-ok", "attempt main").attr(key::BOT_AGENT, "x".repeat(1000)));
    drain(&e).await;
    let s = e.stats(&pid("alpha")).expect("stats");
    assert_eq!((s.sent, s.dropped, s.queued), (1, 1, 0), "{s:?}");
    assert_eq!(c.spans().len(), 1);
}

#[tokio::test]
async fn private_spans_never_carry_content_even_for_a_content_enabled_profile() {
    let c = collector::start(Mode::Ok).await;
    let mut on = cfg(&c, Ids::Plain);
    on.content = true;
    let e = start(vec![("alpha", on)], fast()).await;
    e.record(
        span("alpha", "r-dm", "execute_tool inbox.receive")
            .attr(key::GEN_AI_TOOL_CALL_ARGUMENTS, "sensitive DM payload")
            .attr("bot.private", true),
    );
    e.record(
        span("alpha", "r-ok", "execute_tool agenda.list")
            .attr(key::GEN_AI_TOOL_CALL_ARGUMENTS, "{}"),
    );
    drain(&e).await;
    let all = c.spans();
    let dm = all
        .iter()
        .find(|s| s.str("bot.run.id") == Some("r-dm"))
        .expect("span");
    assert!(
        dm.str(key::GEN_AI_TOOL_CALL_ARGUMENTS).is_none()
            && dm.str("bot.content.dropped") == Some("private")
    );
    assert!(!raw_contains(&c, "sensitive DM payload") && !raw_contains(&c, "bot.private"));
    let ok = all
        .iter()
        .find(|s| s.str("bot.run.id") == Some("r-ok"))
        .expect("span");
    assert_eq!(
        ok.str(key::GEN_AI_TOOL_CALL_ARGUMENTS),
        Some("{}"),
        "non-private spans in the same profile still export content"
    );
}

#[test]
fn the_trace_id_ignores_the_claim_epoch_so_wakeups_stay_in_one_trace() {
    use bot_core::id::ClaimEpoch;
    let base = attempt("alpha", "r-1", 3);
    let with = |claim: Option<ClaimEpoch>| {
        SpanIds::attempt(&Attempt {
            claim,
            ..base.clone()
        })
    };
    // Resuming after waiting for approval advances claim epoch within the same generation.
    assert_eq!(with(None), with(Some(ClaimEpoch(1))));
    assert_eq!(with(Some(ClaimEpoch(1))), with(Some(ClaimEpoch(9))));
    assert_ne!(
        with(None).trace,
        SpanIds::attempt(&attempt("alpha", "r-1", 4)).trace
    );
}
