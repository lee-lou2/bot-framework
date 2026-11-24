//! 프런트가 모델에 돌려주는 **모든** 거절은 그 호출의 확정 결과다: 검증된 CLI 위치에서 실행 전에 거절하면 모델이 볼 단어를 run 체크포인트의 표식으로 **먼저**
//! 남기고 그 뒤에만 돌려준다. 같은 위치의 재전달·재개는 표식을 실행 전에 읽어 그 단어를 재생하고(정상 거절은 시도 횟수를 쓰지 않는다), 같은 위치의 동시 재전달이
//! 거절과 실행으로 둘 다 끝나지 않는다. 저장소 장애·한도가 아닌 `Unavailable`·소유권 상실·시작한 호출은 봉인하지 않는다.

use super::{
    cli::{OWNER_RUN, call, record},
    refusal::{judged, word},
    *,
};
use crate::{
    front::{cli::PlannedCalls, slots::Key},
    rate::RateConfig,
    service::Config,
};
use bot_core::{
    cli::CliRefusal,
    command::{CallResult, JsonPayload, ToolCall},
    port::{Broker, Error, Reply},
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const SAY: &str = r#"{"text":"hi"}"#;

/// 브로커의 답을 시험이 바꿔 끼운다: 주입한 오류가 있으면 실제 브로커를 부르지 않고 그 오류를 돌려준다.
#[derive(Clone, Copy)]
pub(super) enum Inject {
    Missing,
    Invalid,
    Denied,
    Conflict,
    Stale,
    Unavailable,
}

impl Inject {
    fn error(self) -> Error {
        match self {
            Inject::Missing => Error::Missing,
            Inject::Invalid => Error::Invalid,
            Inject::Denied => Error::Denied,
            Inject::Conflict => Error::Conflict,
            Inject::Stale => Error::Stale,
            Inject::Unavailable => Error::Unavailable,
        }
    }
}

#[derive(Default)]
pub(super) struct Wrapped {
    inner: Mutex<Option<Arc<BrokerService>>>,
    error: Mutex<Option<Inject>>,
    calls: AtomicUsize,
}

impl Wrapped {
    pub(super) fn fail_with(&self, error: Option<Inject>) {
        *self.error.lock().unwrap() = error;
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Broker for Wrapped {
    fn call(&self, call: ToolCall) -> Reply<'_, CallResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let injected = *self.error.lock().unwrap();
        let inner = self.inner.lock().unwrap().clone().expect("브로커");
        Box::pin(async move {
            match injected {
                Some(error) => Err(error.error()),
                None => inner.call(call).await,
            }
        })
    }

    fn decide(
        &self,
        nonce: bot_core::id::ApprovalId,
        actor: bot_core::id::PrincipalId,
        allow: bool,
    ) -> Reply<'_, ()> {
        let inner = self.inner.lock().unwrap().clone().expect("브로커");
        Box::pin(async move { inner.decide(nonce, actor, allow).await })
    }
}

pub(super) fn wrapper(wrapped: &Arc<Wrapped>) -> Option<MakeBroker> {
    let wrapped = wrapped.clone();
    Some(Box::new(move |real| {
        *wrapped.inner.lock().unwrap() = Some(real);
        wrapped as Arc<dyn Broker>
    }))
}

async fn injected_rig() -> (Rig, Arc<Wrapped>) {
    let wrapped = Arc::new(Wrapped::default());
    let r = rig_full(
        |_| {},
        |_| {},
        judged(300),
        Config::default(),
        wrapper(&wrapped),
    )
    .await;
    (r, wrapped)
}

fn marker(call: &str) -> String {
    format!("cli-refusal-{call}")
}

#[tokio::test]
async fn a_denial_a_conflict_and_a_stale_condition_are_sealed_before_their_word_and_replayed_for_the_same_position()
 {
    let (r, wrapped) = injected_rig().await;
    let cases = [
        (4, "toolu_d", Inject::Denied, "denied", CliRefusal::Denied),
        (
            5,
            "toolu_c",
            Inject::Conflict,
            "conflict",
            CliRefusal::Conflict,
        ),
        (6, "toolu_s", Inject::Stale, "stale", CliRefusal::Stale),
    ];
    for (turn, id, injected, expected, _) in cases {
        record(&r, turn, &[(id, "echo_say", SAY)]);
        wrapped.fail_with(Some(injected));
        assert_eq!(word(&r, id, "echo_say", SAY).await, expected);
    }
    assert_eq!(
        r.turns.refusals(),
        cases
            .iter()
            .map(|(turn, _, _, _, refusal)| (CallId(format!("cli-{turn}-0")), *refusal))
            .collect::<Vec<_>>(),
        "거절마다 그 단어의 표식이 하나씩이다"
    );
    for (turn, ..) in cases {
        assert!(
            r.ledger
                .step_keys(OWNER_RUN)
                .contains(&marker(&format!("cli-{turn}-0"))),
            "표식은 run 체크포인트에 남는다"
        );
    }
    // 같은 위치의 재전달·재개: 실행할 수 있는 상태가 돼도 브로커를 부르지 않고 모델이 본 그 단어를 재생한다
    wrapped.fail_with(None);
    let before = wrapped.calls();
    for (_, id, _, expected, _) in cases {
        assert_eq!(word(&r, id, "echo_say", SAY).await, expected, "{id}");
    }
    assert_eq!(
        wrapped.calls(),
        before,
        "표식이 있는 위치는 브로커를 부르지 않는다"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(
        r.turns.refusals().len(),
        3,
        "재생은 표식을 다시 쓰지 않는다"
    );
    // 새 위치(모델이 새로 한 호출)는 지금 조건으로 판정해 실행한다
    record(&r, 9, &[("toolu_n", "echo_say", SAY)]);
    let reply = rpc(&r, &call("toolu_n", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn a_storage_failure_is_never_sealed_and_the_retry_runs_the_call() {
    let (r, wrapped) = injected_rig().await;
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    wrapped.fail_with(Some(Inject::Unavailable));
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "unavailable");
    assert!(
        r.turns.refusals().is_empty(),
        "한도가 거절한 것이 아니면 표식은 없다"
    );
    // 저장소가 돌아오면 같은 위치를 다시 판정해 실행한다
    wrapped.fail_with(None);
    let reply = rpc(&r, &call("toolu_a", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
    // 계획과 대조하지 않은 위치(없는 계획)·사전 판정이 아니라고 하는 `Missing`·`Invalid`도 봉인하지 않는다
    record(&r, 8, &[("toolu_m", "echo_say", SAY)]);
    for (injected, expected) in [
        (Inject::Missing, "unknown_tool"),
        (Inject::Invalid, "invalid"),
    ] {
        wrapped.fail_with(Some(injected));
        assert_eq!(word(&r, "toolu_m", "echo_say", SAY).await, expected);
    }
    assert!(r.turns.refusals().is_empty());
}

#[tokio::test]
async fn a_claim_lost_before_the_marker_is_written_leaves_no_marker_and_the_stale_word() {
    let (r, wrapped) = injected_rig().await;
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    wrapped.fail_with(Some(Inject::Denied));
    // 브로커가 거절한 뒤 표식을 쓰기 전에 claim이 넘어갔다: 쓰지 못하고 `stale`이다(새 claim이 같은 위치를 처음부터 판정한다)
    r.turns.fail_refusals_with(Some(Error::Stale));
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "stale");
    assert!(r.turns.refusals().is_empty());
    assert!(!r.ledger.step_keys(OWNER_RUN).contains(&marker("cli-4-0")));
}

/// 표식을 읽지 못하는 호출 ID 원천(저장소 장애를 흉내 낸다): 나머지는 실제 `PlannedCalls`다.
struct Flaky {
    inner: PlannedCalls,
    unreadable: Arc<AtomicBool>,
}

impl CallBook for Flaky {
    fn call_id(
        &self,
        attempt: Attempt,
        tool_use: String,
        name: String,
        args: JsonPayload,
    ) -> Reply<'_, CallId> {
        self.inner.call_id(attempt, tool_use, name, args)
    }
    fn marked(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CliRefusal>> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Box::pin(async { Err(Error::Unavailable) });
        }
        self.inner.marked(attempt, call)
    }
    fn confirm(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args: JsonPayload,
        expected: CliRefusal,
    ) -> Reply<'_, bool> {
        self.inner.confirm(attempt, call, tool, args, expected)
    }
}

#[tokio::test]
async fn an_existing_marker_is_read_before_anything_runs_and_an_unreadable_one_runs_nothing() {
    let unreadable = Arc::new(AtomicBool::new(false));
    let wrapped = Arc::new(Wrapped::default());
    let flaky = {
        let unreadable = unreadable.clone();
        Some(
            Box::new(move |ledger: &Arc<MemLedger>, _: &Arc<BrokerService>| {
                Arc::new(Flaky {
                    inner: PlannedCalls::new(ledger.clone(), Duration::from_millis(300))
                        .with_poll(Duration::from_millis(10)),
                    unreadable,
                }) as Arc<dyn CallBook>
            }) as MakeCalls,
        )
    };
    let r = rig_full(|_| {}, |_| {}, flaky, Config::default(), wrapper(&wrapped)).await;
    // 이미 표식이 있는 위치: 브로커도 실행기도 부르지 않고 그 단어를 재생한다(다른 프로세스·이전 시도가 남긴 표식)
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    r.ledger
        .add_step(OWNER_RUN, &marker("cli-4-0"), r#""denied""#);
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "denied");
    assert_eq!(wrapped.calls(), 0);
    assert!(r.runner.jobs().is_empty());
    // 표식을 읽지 못하면 있는지 없는지 모르니 실행하지 않는다
    record(&r, 8, &[("toolu_b", "echo_say", SAY)]);
    unreadable.store(true, Ordering::SeqCst);
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert_eq!(wrapped.calls(), 0);
    assert!(r.runner.jobs().is_empty());
    unreadable.store(false, Ordering::SeqCst);
    let reply = rpc(&r, &call("toolu_b", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
}

pub(super) fn one_token() -> Config {
    Config {
        rate: RateConfig {
            per_minute: 60,
            burst: 1,
            concurrent: 4,
        },
        ..Config::default()
    }
}

#[tokio::test]
async fn a_limit_that_really_refused_is_sealed_as_unavailable_and_replayed_after_the_bucket_refills()
 {
    let r = rig_full(|_| {}, |_| {}, judged(300), one_token(), None).await;
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let reply = rpc(&r, &call("toolu_a", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    // 두 번째 위치는 한도에 걸린다(Open 기록이 이미 있는 일반 흐름): 모델이 본 `unavailable`이 이 위치의 확정 결과다
    record(&r, 8, &[("toolu_b", "echo_say", SAY)]);
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-8-0".into()), CliRefusal::RateLimited)]
    );
    assert!(r.ledger.has_call(OWNER_RUN, "cli-8-0"), "기록은 열려 있다");
    // 자리가 돌아와도 같은 위치는 재생한다(실행하지 않는다). 새 위치만 실행한다
    r.clock.advance(Duration::from_secs(60));
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert_eq!(r.runner.jobs().len(), 1);
    record(&r, 12, &[("toolu_c", "echo_say", SAY)]);
    let reply = rpc(&r, &call("toolu_c", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 2);
    assert_eq!(r.turns.refusals().len(), 1);
}

#[tokio::test]
async fn the_limit_in_front_of_the_secret_check_is_sealed_without_any_ledger_record() {
    let r = rig_full(|_| {}, |_| {}, judged(300), one_token(), None).await;
    let mut keyed = tool(Risk::Read, &["echo.use"]);
    keyed
        .secrets
        .insert("TOKEN".into(), SecretName::parse("mail-token").unwrap());
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "echo.use", "echo.drop"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "echo.use", "echo.drop"])
            .secret("mail-token")
            .tool("main", "echo.keyed", keyed)
            .build(),
    );
    r.runner.miss_secret("mail-token");
    record(&r, 4, &[("toolu_a", "echo_keyed", "{}")]);
    assert_eq!(word(&r, "toolu_a", "echo_keyed", "{}").await, "denied");
    // 한도는 자격 확인보다 먼저다: 기록도 보관소 조회도 없이 돌려보낸 자리도 봉인한다
    record(&r, 8, &[("toolu_b", "echo_keyed", "{}")]);
    assert_eq!(word(&r, "toolu_b", "echo_keyed", "{}").await, "unavailable");
    assert_eq!(
        r.turns.refusals(),
        vec![
            (CallId("cli-4-0".into()), CliRefusal::Denied),
            (CallId("cli-8-0".into()), CliRefusal::RateLimited),
        ]
    );
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-8-0"));
    assert_eq!(
        r.runner.secret_checks().len(),
        1,
        "한도에 걸린 호출은 보관소를 읽지 않았다"
    );
}

#[tokio::test]
async fn a_stale_condition_is_sealed_only_under_a_live_claim() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    // 실행을 시작하기 직전에 활성 판본이 바뀌었다: 호출은 열려 있고 claim은 살아 있다 → `stale`이 이 위치의 확정 결과다
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let ledger = r.ledger.clone();
    r.ledger
        .set_before_start(move || ledger.set_active_revision("personal", "r2"));
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "stale");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-4-0".into()), CliRefusal::Stale)]
    );
    assert!(r.runner.jobs().is_empty());
    // claim을 잃으면 같은 `stale`이어도 표식을 쓸 수 없다(새 claim이 같은 위치를 처음부터 판정한다)
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let ledger = r.ledger.clone();
    r.ledger.set_before_start(move || {
        ledger.bump_claim("r1");
    });
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "stale");
    assert!(r.turns.refusals().is_empty());
}

#[tokio::test]
async fn a_host_only_tool_name_and_a_bad_builtin_input_are_sealed_at_their_verified_positions() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    // 호스트 전용 ID: 모델이 부를 수 없다(`unknown_tool`)
    record(&r, 4, &[("toolu_x", "schedule_deliver", "{}")]);
    assert_eq!(
        word(&r, "toolu_x", "schedule_deliver", "{}").await,
        "unknown_tool"
    );
    // 내장 예약 도구의 틀린 입력(구조·지나간 시각)은 기록 전에 거절되고 확정이다
    let scopes = ["run", "echo.use", "echo.drop", "schedule.create"];
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &scopes)
            .member("local:owner", &["owner"])
            .policy_scopes(&scopes)
            .build(),
    );
    r.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        "local:owner",
        "main",
        &scopes,
    ));
    let past = r#"{"at":"1970-01-01T00:00:10Z","instruction":"x"}"#;
    record(&r, 8, &[("toolu_y", "schedule_create", "{}")]);
    record(&r, 12, &[("toolu_z", "schedule_create", past)]);
    assert_eq!(
        word(&r, "toolu_y", "schedule_create", "{}").await,
        "invalid"
    );
    assert_eq!(
        word(&r, "toolu_z", "schedule_create", past).await,
        "invalid"
    );
    assert_eq!(
        r.turns.refusals(),
        vec![
            (CallId("cli-4-0".into()), CliRefusal::UnknownTool),
            (CallId("cli-8-0".into()), CliRefusal::Invalid),
            (CallId("cli-12-0".into()), CliRefusal::Invalid),
        ]
    );
    // 틀린 입력은 호출을 기록하지 않았다
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-8-0"));
    // 같은 위치는 몇 번을 다시 받아도 모델이 본 단어를 재생한다(모델은 새 호출로 다시 해야 한다)
    assert_eq!(
        word(&r, "toolu_z", "schedule_create", past).await,
        "invalid"
    );
    assert_eq!(r.turns.refusals().len(), 3, "표식은 위치마다 한 번이다");
}

fn position(call: &str) -> Key {
    (
        ProfileId("personal".into()),
        RunId(OWNER_RUN.into()),
        CallId(call.into()),
    )
}

async fn until_waiting(r: &Rig, call: &str, count: usize) {
    let started = std::time::Instant::now();
    while r.front.shared.slots.waiting(&position(call)) != count {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{call}: 문 앞에서 기다리는 요청이 {count}이 되지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_redelivery_waits_for_a_refusal_in_flight_and_replays_it_instead_of_executing()
{
    let (r, wrapped) = injected_rig().await;
    let r = Arc::new(r);
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    // 먼저 온 요청이 거절을 확정하고 표식을 쓰는 중이다(문을 쥔 채 멈춘다)
    wrapped.fail_with(Some(Inject::Denied));
    let gate = r.turns.hold_next_refusal();
    let first = {
        let r = r.clone();
        tokio::spawn(async move { word(&r, "toolu_a", "echo_say", SAY).await })
    };
    r.turns.refusal_entered().await;
    // 같은 위치의 재전달이 도착한다. 이제 실행할 수 있는 상태다. 그래도 문 앞에서 기다린다
    wrapped.fail_with(None);
    let second = {
        let r = r.clone();
        tokio::spawn(async move { word(&r, "toolu_a", "echo_say", SAY).await })
    };
    until_waiting(&r, "cli-4-0", 1).await;
    assert!(r.runner.jobs().is_empty(), "기다리는 동안 실행하지 않는다");
    assert_eq!(wrapped.calls(), 1);
    // 표식이 쓰이면 기다리던 요청은 같은 단어를 재생한다: 거절과 실행으로 둘 다 끝나지 않는다
    gate.add_permits(1);
    assert_eq!(first.await.unwrap(), "denied");
    assert_eq!(second.await.unwrap(), "denied");
    assert_eq!(wrapped.calls(), 1, "재전달은 브로커를 부르지 않았다");
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.turns.refusals().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_concurrent_redelivery_waits_for_the_running_call_and_gets_its_result_without_running_again()
 {
    let r = Arc::new(rig_with(|_| {}, |_| {}, judged(300)).await);
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    let gate = r.runner.hold_next();
    let first = {
        let r = r.clone();
        tokio::spawn(async move { rpc(&r, &call("toolu_a", "echo_say", SAY)).await.body })
    };
    let started = std::time::Instant::now();
    while r.runner.jobs().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "실행이 시작되지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let second = {
        let r = r.clone();
        tokio::spawn(async move { rpc(&r, &call("toolu_a", "echo_say", SAY)).await.body })
    };
    until_waiting(&r, "cli-4-0", 1).await;
    gate.add_permits(1);
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert!(
        first.contains("ok") && second.contains("ok"),
        "{first} / {second}"
    );
    assert_eq!(r.runner.jobs().len(), 1, "같은 위치는 한 번만 실행했다");
    assert!(r.turns.refusals().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn positions_that_differ_never_wait_for_each_other() {
    let r = Arc::new(rig_with(|_| {}, |_| {}, judged(300)).await);
    record(
        &r,
        4,
        &[("toolu_a", "echo_say", SAY), ("toolu_b", "echo_say", SAY)],
    );
    let gate = r.runner.hold_next();
    let first = {
        let r = r.clone();
        tokio::spawn(async move { rpc(&r, &call("toolu_a", "echo_say", SAY)).await.body })
    };
    let started = std::time::Instant::now();
    while r.runner.jobs().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "실행이 시작되지 않는다"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 첫 호출이 실행 중이어도 다른 위치는 기다리지 않는다
    let other = rpc(&r, &call("toolu_b", "echo_say", SAY)).await;
    assert!(other.body.contains("ok"), "{}", other.body);
    assert_eq!(r.front.shared.slots.waiting(&position("cli-4-1")), 0);
    gate.add_permits(1);
    assert!(first.await.unwrap().contains("ok"));
    assert_eq!(r.runner.jobs().len(), 2);
}
