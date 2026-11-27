//! Tests for confirmed pre-execution CLI refusal markers (`CliRefusal`).

use super::{
    cli::{OWNER_RUN, call, record},
    *,
};
use crate::{front::cli::PlannedCalls, service::preflight::Preflight};
use bot_core::{
    cli::CliRefusal,
    command::JsonPayload,
    port::{Error, Reply},
};
use std::sync::Mutex;

pub(super) fn judged(wait_ms: u64) -> Option<MakeCalls> {
    Some(Box::new(move |ledger, broker| {
        Arc::new(
            PlannedCalls::new(ledger.clone(), Duration::from_millis(wait_ms))
                .with_poll(Duration::from_millis(10))
                .with_preflight(broker.clone()),
        )
    }))
}

#[derive(Default)]
struct Scripted {
    answer: Mutex<Option<Result<Option<CliRefusal>, ErrorKind>>>,
    asked: Mutex<Vec<String>>,
}

#[derive(Clone, Copy)]
enum ErrorKind {
    Unavailable,
    Stale,
}

impl Scripted {
    fn says(&self, answer: Result<Option<CliRefusal>, ErrorKind>) {
        *self.answer.lock().unwrap() = Some(answer);
    }
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Preflight for Scripted {
    fn refusal(&self, _: Attempt, tool: ToolId, _: JsonPayload) -> Reply<'_, Option<CliRefusal>> {
        self.asked.lock().unwrap().push(tool.0);
        let answer = self.answer.lock().unwrap().unwrap_or(Ok(None));
        Box::pin(async move {
            answer.map_err(|kind| match kind {
                ErrorKind::Unavailable => Error::Unavailable,
                ErrorKind::Stale => Error::Stale,
            })
        })
    }
}

fn scripted(script: &Arc<Scripted>) -> Option<MakeCalls> {
    let script = script.clone();
    Some(Box::new(move |ledger, _| {
        Arc::new(
            PlannedCalls::new(ledger.clone(), Duration::from_millis(300))
                .with_poll(Duration::from_millis(10))
                .with_preflight(script),
        )
    }))
}

pub(super) async fn word(r: &Rig, id: &str, name: &str, args: &str) -> String {
    let reply = rpc(r, &call(id, name, args)).await;
    let body: serde_json::Value = serde_json::from_str(&reply.body).expect("JSON-RPC response");
    assert_eq!(body["result"]["isError"], true, "{}", reply.body);
    body["result"]["content"][0]["text"]
        .as_str()
        .expect("fixed error word")
        .to_owned()
}

#[tokio::test]
async fn a_corrupt_refusal_marker_blocks_the_live_call_before_execution() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    record(&r, 4, &[("toolu_bad_mark", "echo_say", "{}")]);
    r.ledger
        .add_step(OWNER_RUN, "cli-refusal-cli-4-0", r#""unknown_refusal""#);
    assert_eq!(
        word(&r, "toolu_bad_mark", "echo_say", "{}").await,
        "unavailable"
    );
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn the_live_cli_keeps_the_receipt_over_a_marker_at_the_same_position() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    record(&r, 4, &[("toolu_receipt", "echo_say", "{}")]);
    let frame = call("toolu_receipt", "echo_say", "{}");
    let first = rpc(&r, &frame).await;
    r.ledger
        .add_step(OWNER_RUN, "cli-refusal-cli-4-0", r#""invalid""#);
    let replay = rpc(&r, &frame).await;
    assert_eq!(replay.body, first.body);
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn a_planned_call_to_a_tool_the_pinned_revision_lacks_leaves_an_unknown_tool_marker() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    record(&r, 4, &[("toolu_a", "echo_nothing", "{}")]);
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-4-0".into()), CliRefusal::UnknownTool)]
    );
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
    assert!(r.runner.jobs().is_empty());
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert!(
        r.turns
            .refusals()
            .iter()
            .all(|(call, kind)| call.0 == "cli-4-0" && *kind == CliRefusal::UnknownTool)
    );
}

#[tokio::test]
async fn an_input_the_pinned_declaration_rejects_leaves_an_invalid_marker() {
    let r = rig_with(|_| {}, |_| {}, judged(300)).await;
    // FakeSealer expects `recipient|content` for explicit-destination tools, rejecting JSON args.
    let mut mail = tool(Risk::Write, &["echo.use"]);
    mail.destination = Some(bot_core::profile::Destination::Explicit {
        recipient: "args.to".into(),
        content: "args.body".into(),
    });
    r.catalog.publish(
        snapshot("personal", "r1")
            .role("owner", &["run", "echo.use", "echo.drop"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "echo.use", "echo.drop"])
            .tool("main", "echo.say", tool(Risk::Read, &["echo.use"]))
            .tool("main", "echo.mail", mail)
            .build(),
    );
    let args = r#"{"body":"b","to":"a"}"#;
    record(&r, 6, &[("toolu_m", "echo_mail", args)]);
    assert_eq!(word(&r, "toolu_m", "echo_mail", args).await, "invalid");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-6-0".into()), CliRefusal::Invalid)]
    );
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-6-0"));
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn nothing_is_confirmed_for_other_results_a_mismatch_or_a_failed_judgment() {
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
    let cases: [(Result<Option<CliRefusal>, ErrorKind>, &str); 4] = [
        (Ok(None), "unknown_tool"),
        (Ok(Some(CliRefusal::Invalid)), "unknown_tool"),
        (Err(ErrorKind::Unavailable), "unavailable"),
        (Err(ErrorKind::Stale), "stale"),
    ];
    for (i, (answer, expected)) in cases.into_iter().enumerate() {
        script.says(answer);
        let (id, turn) = (format!("toolu_{i}"), 10 + i as u64);
        record(&r, turn, &[(id.as_str(), "echo_nothing", "{}")]);
        assert_eq!(word(&r, &id, "echo_nothing", "{}").await, expected, "{i}");
    }
    assert_eq!(script.asked().len(), 4, "preflight asked once per refusal");
    assert!(r.turns.refusals().is_empty(), "no markers should be sealed");
}

#[tokio::test]
async fn a_marker_that_cannot_be_written_is_never_reported_as_a_confirmed_refusal() {
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
    script.says(Ok(Some(CliRefusal::UnknownTool)));
    record(&r, 4, &[("toolu_a", "echo_nothing", "{}")]);
    r.turns.fail_refusals_with(Some(Error::Unavailable));
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unavailable",
        "must not return refusal word before marker persists"
    );
    assert!(r.turns.refusals().is_empty());
    r.turns.fail_refusals_with(None);
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert_eq!(r.turns.refusals().len(), 1);
    record(&r, 8, &[("toolu_b", "echo_nothing", "{}")]);
    r.turns.fail_refusals_with(Some(Error::Stale));
    assert_eq!(word(&r, "toolu_b", "echo_nothing", "{}").await, "stale");
    assert_eq!(r.turns.refusals().len(), 1);
}

#[tokio::test]
async fn a_call_the_ledger_already_started_gets_no_marker_but_an_open_record_may_still_be_sealed() {
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
    script.says(Ok(Some(CliRefusal::UnknownTool)));
    record(&r, 4, &[("toolu_a", "echo_nothing", "{}")]);
    r.ledger
        .begin(
            r.attempt.clone(),
            CallId("cli-4-0".into()),
            ToolId("echo.nothing".into()),
            [0; 32],
        )
        .await
        .unwrap();
    r.ledger.force_started(OWNER_RUN, "cli-4-0", 1, "owner");
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert!(r.turns.refusals().is_empty());
    assert!(
        script.asked().is_empty(),
        "started calls must skip preflight"
    );
    record(&r, 8, &[("toolu_b", "echo_nothing", "{}")]);
    r.ledger
        .begin(
            r.attempt.clone(),
            CallId("cli-8-0".into()),
            ToolId("echo.nothing".into()),
            [0; 32],
        )
        .await
        .unwrap();
    assert_eq!(
        word(&r, "toolu_b", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-8-0".into()), CliRefusal::UnknownTool)]
    );
}

#[tokio::test]
async fn without_a_preflight_or_a_verified_position_nothing_is_sealed_and_a_ledger_denial_needs_no_marker()
 {
    let plain = rig_with(
        |_| {},
        |_| {},
        Some(Box::new(|ledger, _| {
            Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_millis(300))
                    .with_poll(Duration::from_millis(10)),
            )
        })),
    )
    .await;
    record(&plain, 4, &[("toolu_a", "echo_nothing", "{}")]);
    assert_eq!(
        word(&plain, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert!(plain.turns.refusals().is_empty());
    let direct = rig(|_| {}, |_| {}).await;
    assert_eq!(
        word(&direct, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert!(direct.turns.refusals().is_empty());
    // Scope denial already records in the call ledger, so no refusal marker is written.
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
    record(&r, 4, &[("toolu_a", "echo_locked", "{}")]);
    assert_eq!(word(&r, "toolu_a", "echo_locked", "{}").await, "denied");
    assert!(
        r.ledger.has_call(OWNER_RUN, "cli-4-0"),
        "denial recorded in ledger"
    );
    assert!(script.asked().is_empty() && r.turns.refusals().is_empty());
    // Tool removed from active revision (still in pinned): unrecorded `denied` seals directly without preflight.
    r.catalog.publish(
        snapshot("personal", "r2")
            .role("owner", &["run", "echo.use", "echo.drop"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["run", "echo.use", "echo.drop"])
            .build(),
    );
    r.ledger.set_active_revision("personal", "r2");
    record(&r, 5, &[("toolu_b", "echo_say", "{}")]);
    assert_eq!(word(&r, "toolu_b", "echo_say", "{}").await, "denied");
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-5-0"));
    assert!(script.asked().is_empty(), "denied must not query preflight");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-5-0".into()), CliRefusal::Denied)]
    );
}

#[tokio::test]
async fn a_missing_secret_is_a_sealed_denial_the_same_position_replays_it_and_a_new_position_judges_again()
 {
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
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
    record(&r, 4, &[("toolu_k", "echo_keyed", "{}")]);
    assert_eq!(word(&r, "toolu_k", "echo_keyed", "{}").await, "denied");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-4-0".into()), CliRefusal::Denied)],
        "audit reason must not leak into refusal marker"
    );
    assert!(script.asked().is_empty());
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
    assert!(r.runner.jobs().is_empty());
    // Restoring the secret does not re-evaluate the already-sealed position.
    r.runner.restore_secret("mail-token");
    assert_eq!(word(&r, "toolu_k", "echo_keyed", "{}").await, "denied");
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.turns.refusals().len(), 1, "marker written once");
    record(&r, 8, &[("toolu_n", "echo_keyed", "{}")]);
    let reply = rpc(&r, &call("toolu_n", "echo_keyed", "{}")).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs().len(), 1);
    assert_eq!(r.turns.refusals().len(), 1);
}

#[tokio::test]
async fn the_checkpoint_sink_writes_the_core_key_and_the_exact_word_and_is_idempotent() {
    use crate::front::observe::{CheckpointSink, TurnSink};
    let runs = Arc::new(FakeRuns::new());
    let sink = CheckpointSink::new(runs.clone());
    let attempt = Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r1".into()),
        generation: Generation(1),
        claim: Some(bot_core::id::ClaimEpoch(1)),
    };
    let call = CallId::cli(7, 0);
    sink.refused(attempt.clone(), call.clone(), CliRefusal::Invalid)
        .await
        .unwrap();
    sink.refused(attempt.clone(), call.clone(), CliRefusal::Invalid)
        .await
        .unwrap();
    let steps = runs.steps();
    assert_eq!(steps.len(), 1, "duplicate writes must be idempotent");
    assert_eq!(steps[0].1.key, CliRefusal::key(&call));
    assert_eq!(steps[0].1.key, "cli-refusal-cli-7-0");
    assert_eq!(steps[0].1.body.0, r#""invalid""#);
    assert!(
        sink.refused(attempt, call, CliRefusal::UnknownTool)
            .await
            .is_err()
    );
}
