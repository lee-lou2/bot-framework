use super::*;
use crate::{
    host::HostConfig,
    ingress::Ingress,
    testing::{MemoryPrivate, MemorySecrets, MemoryStore, TestCatalog, api::MemoryApi},
};
use bot_core::{
    api::ApiKey,
    id::{AgentId, BindingId, RunId},
};

struct Harness {
    api: Arc<MemoryApi>,
    stop: CancellationToken,
    started: Instant,
}

impl Harness {
    fn new() -> Self {
        Self {
            api: MemoryApi::new(),
            stop: CancellationToken::new(),
            started: Instant::now(),
        }
    }

    fn run(state: ApiState) -> ApiRun {
        ApiRun {
            run: RunId("r1".into()),
            created: 1,
            model: "assistant".into(),
            binding: BindingId("assistant".into()),
            agent: AgentId("main".into()),
            revision: bot_core::id::Revision("rev".into()),
            digest: [0; 32],
            state,
        }
    }

    fn ctl(&self, wait_ms: u64, hard_ms: u64, seed: Option<ApiRun>) -> Ctl {
        let ingress = Arc::new(Ingress::new(
            TestCatalog::new([]),
            Arc::new(HostConfig::from_json("{}").unwrap()),
            MemoryStore::new(),
            MemoryPrivate::new(),
        ));
        let gateway = Arc::new(HttpGateway::new(
            ingress,
            MemoryStore::new(),
            MemorySecrets::new(&[]),
        ));
        let authed = Authed::for_test("openai", "build");
        let profile = ProfileId("personal".into());
        if let Some(run) = seed {
            self.api.seed(
                &ApiKey {
                    profile: profile.clone(),
                    endpoint: "openai".into(),
                    principal: authed.principal.clone(),
                    key: RequestId("k1".into()),
                },
                run,
            );
        }
        Ctl {
            gateway,
            api: self.api.clone(),
            authed,
            profile,
            key: "k1".into(),
            wait_until: self.started + Duration::from_millis(wait_ms),
            hard: self.started + Duration::from_millis(hard_ms),
            stop: self.stop.clone(),
            at_handoff: true,
        }
    }
}

fn notice(resolved: &Resolved) -> bool {
    matches!(resolved, Resolved::Answer(Answer::Notice(text)) if *text == render::still_running())
}

fn unknown(resolved: &Resolved) -> bool {
    matches!(resolved, Resolved::Failure(failure) if failure.code == "request_outcome_unknown")
}

#[test]
fn the_body_gets_at_most_ten_seconds_and_never_more_than_the_total_deadline() {
    let start = Instant::now();
    let long = start + Duration::from_secs(120);
    assert_eq!(read_deadline(start, long), start + BODY_READ);
    let short = start + Duration::from_millis(800);
    assert_eq!(read_deadline(start, short), short);
}

#[tokio::test(start_paused = true)]
async fn no_lookup_is_started_once_the_wait_is_over() {
    // Lookups occur at 40, 120, and 280 ms; the next sleep hits the 450 ms deadline without another lookup.
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(450, 600, Some(run.clone()));
    let resolved = ctl.resolve(run, None).await;
    assert!(notice(&resolved));
    assert_eq!(harness.api.lookups(), 3, "no lookup after wait deadline");
    assert_eq!(Instant::now() - harness.started, Duration::from_millis(450));
}

#[tokio::test(start_paused = true)]
async fn a_stop_signal_ends_the_wait_at_once_with_the_last_confirmed_state() {
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(5_000, 6_000, Some(run.clone()));
    let stop = harness.stop.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.cancel();
    });
    let resolved = ctl.resolve(run, None).await;
    assert!(notice(&resolved));
    assert!(
        Instant::now() - harness.started < Duration::from_millis(300),
        "{:?}",
        Instant::now() - harness.started
    );
    let harness = Harness::new();
    harness.stop.cancel();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(5_000, 6_000, Some(run.clone()));
    assert!(notice(&ctl.resolve(run, None).await));
    assert_eq!(harness.api.lookups(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_run_that_vanishes_from_the_ledger_is_an_unknown_outcome_at_once_not_at_the_deadline() {
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(5_000, 6_000, None);
    let resolved = ctl.resolve(run, None).await;
    assert!(unknown(&resolved));
    assert_eq!(
        Instant::now() - harness.started,
        Duration::from_millis(40),
        "fails immediately on first lookup"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_lookup_is_forgiven_by_a_later_one_but_not_by_none() {
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(450, 600, Some(run.clone()));
    harness.api.fail_lookups(true);
    let api = harness.api.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        api.fail_lookups(false);
    });
    assert!(notice(&ctl.resolve(run.clone(), None).await));
    let harness = Harness::new();
    let ctl = harness.ctl(450, 600, Some(run.clone()));
    harness.api.fail_lookups(true);
    assert!(unknown(&ctl.resolve(run, None).await));
}

#[tokio::test(start_paused = true)]
async fn a_lookup_cut_off_by_the_end_of_the_wait_is_no_news_not_a_failure() {
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    harness.api.delay_lookups(Duration::from_secs(60));
    let ctl = harness.ctl(450, 600, Some(run.clone()));
    let resolved = ctl.resolve(run, None).await;
    assert!(notice(&resolved));
    assert_eq!(Instant::now() - harness.started, Duration::from_millis(450));
}
