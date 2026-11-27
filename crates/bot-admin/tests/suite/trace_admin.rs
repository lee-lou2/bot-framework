//! Admin API and trace exporter integration: exporter status, run trace IDs/links, and admin decision spans.

use crate::support::collector::{self, Collector, Mode};
use bot_admin::{
    fake::Fake,
    trace::{Backend, Exporter, Ids, Options, ProfileTrace, Redactor, SpanIds},
};
use bot_core::id::{Attempt, Generation, ProfileId, RunId};
use std::{sync::Arc, time::Duration};

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

async fn start(map: Vec<(&str, ProfileTrace)>) -> Arc<Exporter> {
    let opts = Options {
        interval: Duration::from_millis(20),
        timeout: Duration::from_millis(300),
        ..Options::default()
    };
    let cfg = map.into_iter().map(|(k, v)| (pid(k), v)).collect();
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
async fn the_admin_api_shows_exporter_state_trace_links_and_leaves_decision_spans() {
    let (a, b) = (
        collector::start(Mode::Ok).await,
        collector::start(Mode::Ok).await,
    );
    let mut linked = cfg(&a, Ids::Plain);
    if let Some(be) = &mut linked.backend {
        be.link = Some("https://traces.example/t/{trace_id}".into());
    }
    let e = start(vec![("personal", linked), ("work", cfg(&b, Ids::Plain))]).await;
    let s = crate::support::start_with(Fake::demo(), Some(e.clone())).await;
    let st = crate::support::get(&s, crate::support::OPERATOR, "/v1/admin/status")
        .await
        .json();
    let obs = &st["profiles"]["personal"]["obs"];
    assert_eq!(
        (obs["enabled"].as_bool(), obs["circuit"].as_str()),
        (Some(true), Some("closed"))
    );
    assert_eq!(
        obs["backends"][0]["origin"],
        a.addr.to_string(),
        "only origin is shown; paths and headers stay hidden"
    );
    assert_eq!(obs["backends"][0]["kind"], "otlp-http");
    assert!(!st.to_string().contains("/v1/traces"));
    let runs = crate::support::get(&s, crate::support::OPERATOR, "/v1/admin/runs?limit=200")
        .await
        .json();
    let expect =
        |p: &str, r: &str| bot_admin::json::hex(&SpanIds::attempt(&attempt(p, r, 1)).trace);
    for item in runs["items"].as_array().expect("list") {
        let (p, r) = (
            item["profile"].as_str().expect("p"),
            item["run"].as_str().expect("r"),
        );
        if r == "r-4" {
            continue; // generation 2
        }
        assert_eq!(
            item["trace_id"].as_str(),
            Some(expect(p, r).as_str()),
            "{p}/{r}"
        );
        match p {
            "personal" => assert_eq!(
                item["trace_link"].as_str(),
                Some(format!("https://traces.example/t/{}", expect(p, r)).as_str())
            ),
            _ => assert!(item["trace_link"].is_null(), "no template means no link"),
        }
    }
    let detail = crate::support::get(&s, crate::support::OPERATOR, "/v1/admin/runs/personal/r-2")
        .await
        .json();
    assert!(
        detail["lineage"][0]["trace_link"].is_string() && detail["run"]["trace_link"].is_string()
    );
    // Approving attaches a decision span to the run attempt trace without emitting the nonce.
    let seen = bot_admin::json::hex(&bot_admin::fake::digest("c-1:agenda.delete"));
    let r = crate::support::post(
        &s,
        crate::support::OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    drain(&e).await;
    let d = a
        .spans()
        .into_iter()
        .find(|x| x.name == "admin approval.decide")
        .expect("decision span");
    let root = SpanIds::attempt(&attempt("personal", "r-1", 1));
    assert_eq!(
        (d.trace.as_slice(), d.parent.as_slice()),
        (root.trace.as_slice(), root.span.as_slice())
    );
    assert_eq!(
        (
            d.str("bot.admin.action"),
            d.str("bot.admin.verdict"),
            d.str("bot.approval.risk")
        ),
        (Some("approval.decide"), Some("allow"), Some("irreversible"))
    );
    assert!(!raw_contains(&a, "ap-1") && b.spans().is_empty());
}

#[tokio::test]
async fn repeated_admin_decisions_on_one_call_get_distinct_span_ids() {
    let c = collector::start(Mode::Ok).await;
    let e = start(vec![("personal", cfg(&c, Ids::Plain))]).await;
    let s = crate::support::start_with(Fake::demo(), Some(e.clone())).await;
    s.fake.edit(|d| {
        let call = d
            .calls
            .iter()
            .find(|c| c.call.0 == "c-9")
            .cloned()
            .expect("call");
        d.effects.push(bot_admin::fake::effect_view(
            &call,
            "e-3",
            "archive",
            bot_core::state::EffectState::Held,
            None,
        ));
    });
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    for effect in ["e-2", "e-3"] {
        let body = format!(
            r#"{{"verdict":"not_executed","evidence":"no record at recipient","effect":"{effect}","hold_id":"h-{effect}"}}"#
        );
        assert_eq!(
            crate::support::post(&s, crate::support::OPERATOR, path, &body)
                .await
                .status,
            200
        );
    }
    drain(&e).await;
    let spans: Vec<_> = c
        .spans()
        .into_iter()
        .filter(|x| x.name == "admin hold.resolve")
        .collect();
    assert_eq!(spans.len(), 2);
    assert_ne!(
        spans[0].span, spans[1].span,
        "two decisions on one call get distinct span IDs"
    );
    assert_eq!(spans[0].trace, spans[1].trace, "same attempt trace");
    assert_eq!(spans[0].str("bot.admin.verdict"), Some("not_executed"));
}

#[tokio::test]
async fn a_repeated_judgment_leaves_no_second_decision_span() {
    let c = collector::start(Mode::Ok).await;
    let e = start(vec![("personal", cfg(&c, Ids::Plain))]).await;
    let s = crate::support::start_with(Fake::demo(), Some(e.clone())).await;
    let path = "/v1/admin/holds/personal/r-4/c-9/resolve";
    let body = r#"{"verdict":"not_executed","evidence":"no record at recipient","effect":"e-2","hold_id":"h-e-2"}"#;
    for repeated in [false, true, true] {
        let r = crate::support::post(&s, crate::support::OPERATOR, path, body).await;
        assert_eq!(
            (r.status, r.json()["repeated"].as_bool()),
            (200, Some(repeated)),
            "{}",
            r.body
        );
    }
    drain(&e).await;
    let spans: Vec<_> = c
        .spans()
        .into_iter()
        .filter(|x| x.name == "admin hold.resolve")
        .collect();
    assert_eq!(
        spans.len(),
        1,
        "only the first applied verdict emits a span"
    );
}
