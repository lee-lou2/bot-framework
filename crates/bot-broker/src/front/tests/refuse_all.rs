//! Every pre-execution refusal at a verified CLI position is sealed as a checkpoint marker before returning, replayed on redelivery, and serialized against concurrent calls.

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
        let inner = self.inner.lock().unwrap().clone().expect("broker");
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
        let inner = self.inner.lock().unwrap().clone().expect("broker");
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
        "one refusal marker per refused position"
    );
    for (turn, ..) in cases {
        assert!(
            r.ledger
                .step_keys(OWNER_RUN)
                .contains(&marker(&format!("cli-{turn}-0"))),
            "marker must persist in run checkpoints"
        );
    }
    // Redelivery replays the sealed code without calling the broker even after the condition clears.
    wrapped.fail_with(None);
    let before = wrapped.calls();
    for (_, id, _, expected, _) in cases {
        assert_eq!(word(&r, id, "echo_say", SAY).await, expected, "{id}");
    }
    assert_eq!(
        wrapped.calls(),
        before,
        "sealed position must not call broker"
    );
    assert!(r.runner.jobs().is_empty());
    assert_eq!(
        r.turns.refusals().len(),
        3,
        "replay must not rewrite the marker"
    );
    // A new position is evaluated under current conditions.
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
        "non-limit unavailable must not be sealed"
    );
    wrapped.fail_with(None);
    let reply = rpc(&r, &call("toolu_a", "echo_say", SAY)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
    // Unconfirmed Missing/Invalid responses are not sealed.
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
    r.turns.fail_refusals_with(Some(Error::Stale));
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "stale");
    assert!(r.turns.refusals().is_empty());
    assert!(!r.ledger.step_keys(OWNER_RUN).contains(&marker("cli-4-0")));
}

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
    record(&r, 4, &[("toolu_a", "echo_say", SAY)]);
    r.ledger
        .add_step(OWNER_RUN, &marker("cli-4-0"), r#""denied""#);
    assert_eq!(word(&r, "toolu_a", "echo_say", SAY).await, "denied");
    assert_eq!(wrapped.calls(), 0);
    assert!(r.runner.jobs().is_empty());
    // Unreadable marker fails closed without executing.
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
    record(&r, 8, &[("toolu_b", "echo_say", SAY)]);
    assert_eq!(word(&r, "toolu_b", "echo_say", SAY).await, "unavailable");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-8-0".into()), CliRefusal::RateLimited)]
    );
    assert!(r.ledger.has_call(OWNER_RUN, "cli-8-0"), "call stays open");
    // After refill, the sealed position still replays while a new position executes.
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
    // Rate limit runs before secret check and seals even without a ledger record.
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
        "rate-limited call must not check secret store"
    );
}

#[tokio::test]
async fn a_stale_condition_is_sealed_only_under_a_live_claim() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    // Active revision changed before start while claim is still live: seal stale.
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
    // Lost claim cannot write a refusal marker.
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
    // Host-only tool ID cannot be called by the model.
    record(&r, 4, &[("toolu_x", "schedule_deliver", "{}")]);
    assert_eq!(
        word(&r, "toolu_x", "schedule_deliver", "{}").await,
        "unknown_tool"
    );
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
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-8-0"));
    assert_eq!(
        word(&r, "toolu_z", "schedule_create", past).await,
        "invalid"
    );
    assert_eq!(r.turns.refusals().len(), 3, "one marker per position");
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
            "{call}: timed out waiting for {count} waiter(s) on slot"
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
    // First request pauses while persisting its refusal marker, holding the slot.
    wrapped.fail_with(Some(Inject::Denied));
    let gate = r.turns.hold_next_refusal();
    let first = {
        let r = r.clone();
        tokio::spawn(async move { word(&r, "toolu_a", "echo_say", SAY).await })
    };
    r.turns.refusal_entered().await;
    // Concurrent redelivery waits on the slot even after the injected failure clears.
    wrapped.fail_with(None);
    let second = {
        let r = r.clone();
        tokio::spawn(async move { word(&r, "toolu_a", "echo_say", SAY).await })
    };
    until_waiting(&r, "cli-4-0", 1).await;
    assert!(
        r.runner.jobs().is_empty(),
        "must not execute while waiting on slot"
    );
    assert_eq!(wrapped.calls(), 1);
    gate.add_permits(1);
    assert_eq!(first.await.unwrap(), "denied");
    assert_eq!(second.await.unwrap(), "denied");
    assert_eq!(wrapped.calls(), 1, "redelivery must not call broker");
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
            "runner did not start execution"
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
    assert_eq!(r.runner.jobs().len(), 1, "same position must run only once");
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
            "runner did not start execution"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let other = rpc(&r, &call("toolu_b", "echo_say", SAY)).await;
    assert!(other.body.contains("ok"), "{}", other.body);
    assert_eq!(r.front.shared.slots.waiting(&position("cli-4-1")), 0);
    gate.add_permits(1);
    assert!(first.await.unwrap().contains("ok"));
    assert_eq!(r.runner.jobs().len(), 2);
}
