//! 기다림(`Ctl::resolve`)의 시간 논리를 가짜 시계로 결정적으로 시험한다: 새 조회를 언제까지 시작하는지, 종료 신호·사라진 작업·조회 오류가 어떻게 끝나는지.
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

    /// `wait_ms`·`hard_ms`는 시작 시각으로부터의 기다림 끝과 총 기한이다. 승인 확인은 본문 쪽이 한다고 두어(`at_handoff`) 보관소 없이 돈다.
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
    // 조회는 40·120·280ms에 나가고, 그다음 잠은 기한(450ms)까지다: 기한이 된 뒤에는 새 조회를 시작하지 않고 마지막 확인 상태로 마친다
    let harness = Harness::new();
    let run = Harness::run(ApiState::Active);
    let ctl = harness.ctl(450, 600, Some(run.clone()));
    let resolved = ctl.resolve(run, None).await;
    assert!(notice(&resolved));
    assert_eq!(harness.api.lookups(), 3, "기한 뒤의 조회는 없다");
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
    // 종료 신호가 이미 와 있으면 조회를 하나도 시작하지 않는다
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
    // 원장에는 이 범위의 작업이 없다(심지 않았다): 첫 조회가 «없음»이다
    let ctl = harness.ctl(5_000, 6_000, None);
    let resolved = ctl.resolve(run, None).await;
    assert!(unknown(&resolved));
    assert_eq!(
        Instant::now() - harness.started,
        Duration::from_millis(40),
        "첫 조회에서 곧바로"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_lookup_is_forgiven_by_a_later_one_but_not_by_none() {
    // 조회가 실패했다가 다음 조회가 성공하면 마지막 확인 상태가 있으니 안내로 마친다
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
    // 끝까지 실패하면 마지막 확인이 실패한 채 기한이 온다: 확인 불가
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
