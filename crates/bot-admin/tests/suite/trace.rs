//! 추적 내보내기: 프로필 격리, 장애 격리, 내용·식별자 정책, 한 시도의 추적 합류, 관리 API 연결.

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
    Exporter::start(cfg, None, Redactor::default(), opts).expect("시작")
}

async fn drain(e: &Exporter) {
    assert!(e.flush(Duration::from_secs(10)).await, "큐가 비지 않았다");
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
        // 같은 run ID·세대라도 프로필이 다르면 추적이 다르다.
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
        "프로필 사이에 추적이 겹치지 않는다"
    );
    assert!(
        !raw_contains(&a, "beta") && !raw_contains(&b, "alpha"),
        "다른 프로필의 글자가 섞이지 않는다"
    );
    assert!(
        e.stats(&pid("quiet"))
            .is_some_and(|s| !s.enabled && s.sent == 0 && s.queued == 0)
    );
    assert!(
        e.stats(&pid("gamma")).is_none(),
        "미등록 프로필은 어디로도 가지 않는다"
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
    // 수집기가 응답하지 않아도 기록은 곧바로 돌아온다.
    let started = Instant::now();
    for i in 0..5000 {
        e.record(span("alpha", &format!("r-{i}"), "attempt main"));
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // 다른 스레드(런타임 밖)에서도 안전하다.
    let e2 = e.clone();
    std::thread::spawn(move || e2.record(span("alpha", "r-thread", "attempt main")))
        .join()
        .expect("스레드");
    drain(&e).await;
    let s = e.stats(&pid("alpha")).expect("통계");
    assert_eq!(
        s.sent + s.failed + s.dropped,
        5001,
        "{s:?}: 모든 스팬은 보냄·실패·버림 중 하나로 셈된다"
    );
    assert_eq!((s.sent, s.queued), (0, 0));
    assert!(s.dropped > 4000 && s.failed > 0, "{s:?}");
    assert_eq!((s.circuit, s.last_error), ("open", Some("timeout")));
    // 수집기가 살아나면 차단기가 풀리고 다시 내보낸다.
    c.set(Mode::Ok);
    tokio::time::sleep(Duration::from_millis(300)).await;
    for i in 0..3 {
        e.record(span("alpha", &format!("again-{i}"), "attempt main"));
    }
    drain(&e).await;
    let s = e.stats(&pid("alpha")).expect("통계");
    assert_eq!((s.sent, s.circuit), (3, "closed"), "{s:?}");
    assert_eq!(c.spans().len(), 3);
}

#[tokio::test]
async fn refused_and_failing_collectors_are_counted_by_class() {
    // 닫힌 포트: 잡았다가 놓으면 연결이 거절된다.
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("바인드");
        l.local_addr().expect("주소").port()
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
    // 수집기가 있든 없든 기록 호출은 바로 돌아오고 다른 프로필의 내보내기에 영향이 없다.
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
    let e = Exporter::start(cfg_map, None, red, fast()).expect("시작");
    for p in ["plain", "rich"] {
        e.record(
            span(p, "r-1", "chat assistant")
                .attr(key::GEN_AI_INPUT_MESSAGES, "안녕 zq7-secret-value 요청")
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
            "기본은 내용을 내보내지 않는다: {k}"
        );
    }
    assert_eq!(
        (p.str("bot.content.dropped"), p.int("bot.dropped")),
        (Some("off"), Some(3))
    );
    assert_eq!(
        p.str("gen_ai.request.model"),
        Some("assistant"),
        "메타데이터는 그대로 나간다"
    );
    // 켠 프로필: 내용은 비밀이 가려져 나가고, 승인 nonce는 어느 쪽에도 없다.
    assert_eq!(
        r.str(key::GEN_AI_INPUT_MESSAGES),
        Some("안녕 [redacted] 요청")
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
    let pseudo = h.str("bot.principal").expect("가명");
    assert_eq!(pseudo.len(), 12);
    assert!(!raw_contains(&hashed, "owner") && h.str("bot.agent") == Some("main"));
    assert_eq!(plain.spans()[0].str("bot.principal"), Some("local:owner"));
}

#[tokio::test]
async fn spans_of_one_attempt_from_different_components_join_one_trace() {
    let c = collector::start(Mode::Ok).await;
    let e = start(vec![("alpha", cfg(&c, Ids::Plain))], fast()).await;
    let a = attempt("alpha", "r-9", 2);
    // 워커와 브로커(다른 프로세스라고 보자)는 문맥을 주고받지 않고 각자 같은 시도에서 ID를 계산한다.
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
            .expect("스팬")
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
        "도구 스팬은 시도 루트의 자식이다"
    );
    assert_ne!(w2.trace, w.trace, "세대가 바뀌면 새 추적이다");
    assert_eq!(t.str(key::GEN_AI_OPERATION_NAME), Some("execute_tool"));
    assert_eq!(t.str(key::GEN_AI_TOOL_TYPE), Some("function"));
    assert_eq!(t.str(key::GEN_AI_TOOL_NAME), Some("agenda.delete"));
    assert_eq!(t.str(key::GEN_AI_TOOL_CALL_ID), Some("c-1"));
    assert!(
        t.int(key::BOT_DROPPED).is_none(),
        "표준 속성은 허용 어휘를 그대로 통과한다"
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
    assert_eq!(half.spans().len(), want, "모든 프로세스가 같은 결론이다");
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
    let s = e.stats(&pid("alpha")).expect("통계");
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
            .attr(key::GEN_AI_TOOL_CALL_ARGUMENTS, "민감 DM 원문")
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
        .expect("스팬");
    assert!(
        dm.str(key::GEN_AI_TOOL_CALL_ARGUMENTS).is_none()
            && dm.str("bot.content.dropped") == Some("private")
    );
    assert!(!raw_contains(&c, "민감 DM 원문") && !raw_contains(&c, "bot.private"));
    let ok = all
        .iter()
        .find(|s| s.str("bot.run.id") == Some("r-ok"))
        .expect("스팬");
    assert_eq!(
        ok.str(key::GEN_AI_TOOL_CALL_ARGUMENTS),
        Some("{}"),
        "같은 프로필의 일반 스팬은 내용이 나간다"
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
    // 승인 대기 뒤 정상 재개는 같은 세대의 새 점유다. 관리 화면(claim 없음)과 워커(claim 있음)가 같은 추적을 가리킨다.
    assert_eq!(with(None), with(Some(ClaimEpoch(1))));
    assert_eq!(with(Some(ClaimEpoch(1))), with(Some(ClaimEpoch(9))));
    // 세대가 바뀌면 새 추적이다.
    assert_ne!(
        with(None).trace,
        SpanIds::attempt(&attempt("alpha", "r-1", 4)).trace
    );
}
