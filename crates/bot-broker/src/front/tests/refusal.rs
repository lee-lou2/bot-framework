//! 실행 전에 확정된 CLI 거절(`CliRefusal`): 검증된 계획 위치에서, 실행이 고정한 판본으로 확정되는 두 경우(고정 판본에 도구가 없음·그 선언의
//! 입력 검사 실패)에만 run 체크포인트에 표식을 남기고, 남긴 **뒤에만** 같은 단어를 돌려준다. 저장소 오류·확인할 수 없는 판본·세대 상실·
//! 원장에 기록이 있는 호출·계획과 대조하지 않는 원천에는 표식이 없다.

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

/// 실제 브로커를 사전 판정으로 쓴다.
pub(super) fn judged(wait_ms: u64) -> Option<MakeCalls> {
    Some(Box::new(move |ledger, broker| {
        Arc::new(
            PlannedCalls::new(ledger.clone(), Duration::from_millis(wait_ms))
                .with_poll(Duration::from_millis(10))
                .with_preflight(broker.clone()),
        )
    }))
}

/// 미리 정한 답을 주고 물은 도구를 기록하는 사전 판정.
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
    let body: serde_json::Value = serde_json::from_str(&reply.body).expect("JSON-RPC 응답");
    assert_eq!(body["result"]["isError"], true, "{}", reply.body);
    body["result"]["content"][0]["text"]
        .as_str()
        .expect("고정 단어")
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
    // 일반 호출 규약은 그대로다: 원장에는 아무 기록도 없고 아무것도 실행하지 않는다
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
    assert!(r.runner.jobs().is_empty());
    // 같은 호출을 다시 받아도 같은 표식이다(재개·재전달)
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
    // 명시 수신자가 있는 도구: 시험용 봉인기는 `수신자|내용` 글이 아닌 인자(JSON)를 입력 검사 실패로 본다.
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
        // 판정이 거절이 아니라고 한다: 저장소 오류가 섞인 `Missing`이었다
        (Ok(None), "unknown_tool"),
        // 브로커가 거절한 종류와 다르다
        (Ok(Some(CliRefusal::Invalid)), "unknown_tool"),
        // 판정이 오류다: 그 오류의 단어가 모델에 간다
        (Err(ErrorKind::Unavailable), "unavailable"),
        (Err(ErrorKind::Stale), "stale"),
    ];
    for (i, (answer, expected)) in cases.into_iter().enumerate() {
        script.says(answer);
        let (id, turn) = (format!("toolu_{i}"), 10 + i as u64);
        record(&r, turn, &[(id.as_str(), "echo_nothing", "{}")]);
        assert_eq!(word(&r, &id, "echo_nothing", "{}").await, expected, "{i}");
    }
    assert_eq!(script.asked().len(), 4, "거절마다 한 번씩 물었다");
    assert!(r.turns.refusals().is_empty(), "표식은 하나도 없다");
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
        "확정을 남기지 못했으면 거절 단어를 돌려주지 않는다"
    );
    assert!(r.turns.refusals().is_empty());
    // 저장소가 돌아오면 같은 호출이 같은 단어와 표식으로 끝난다
    r.turns.fail_refusals_with(None);
    assert_eq!(
        word(&r, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert_eq!(r.turns.refusals().len(), 1);
    // 세대를 잃었으면(`Stale`) 표식도 단어도 없다
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
    // 원장이 이 호출을 시작·보류·완료·거절로 말한다: 표식이 아니라 원장을 따른다
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
        "시작한 기록이 있으면 판정도 묻지 않는다"
    );
    // 아직 열려 있기만 한 기록(시작 전)은 표식으로 닫을 수 있다: 재개는 표식을 따른다
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
    // 사전 판정을 붙이지 않은 `PlannedCalls`
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
    // 계획과 대조하지 않는 원천(`DirectIds`)은 호출 위치를 믿지 않는다
    let direct = rig(|_| {}, |_| {}).await;
    assert_eq!(
        word(&direct, "toolu_a", "echo_nothing", "{}").await,
        "unknown_tool"
    );
    assert!(direct.turns.refusals().is_empty());
    // 원장이 거절로 기록하는 스코프 거절은 표식이 필요 없다(원장이 이미 이 호출을 말한다)
    let script = Arc::new(Scripted::default());
    let r = rig_with(|_| {}, |_| {}, scripted(&script)).await;
    record(&r, 4, &[("toolu_a", "echo_locked", "{}")]);
    assert_eq!(word(&r, "toolu_a", "echo_locked", "{}").await, "denied");
    assert!(
        r.ledger.has_call(OWNER_RUN, "cli-4-0"),
        "원장에 거절로 남았다"
    );
    assert!(script.asked().is_empty() && r.turns.refusals().is_empty());
    // 원장에 기록이 남지 않는 거절은 판정을 묻지 않고 표식으로 닫는다: 지금 활성 판본에서 도구가 빠졌다(고정 판본에는 있다) → `denied`
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
    assert!(
        script.asked().is_empty(),
        "`denied`는 사전 판정을 묻지 않는다"
    );
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
    // 도구가 보관소에 없는 자격을 선언했다(프로필은 그 이름을 허용한다)
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
    // 기록과 승인 카드보다 먼저 닫는다: 호출 기록·실행 0, 사전 판정 질문 0. 모델이 본 `denied`는 이 위치의 확정 결과다(표식)
    assert_eq!(word(&r, "toolu_k", "echo_keyed", "{}").await, "denied");
    assert_eq!(
        r.turns.refusals(),
        vec![(CallId("cli-4-0".into()), CliRefusal::Denied)],
        "감사 사유(secret_missing)는 표식에 싣지 않는다"
    );
    assert!(script.asked().is_empty());
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
    assert!(r.runner.jobs().is_empty());
    // 운영자가 자격을 채워도 같은 위치는 모델이 본 단어를 그대로 재생한다(다시 판정하지도 실행하지도 않는다)
    r.runner.restore_secret("mail-token");
    assert_eq!(word(&r, "toolu_k", "echo_keyed", "{}").await, "denied");
    assert!(r.runner.jobs().is_empty());
    assert_eq!(r.turns.refusals().len(), 1, "표식은 한 번만 쓴다");
    // 새 위치(모델이 새로 한 호출)는 지금 조건으로 판정해 실행한다
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
    assert_eq!(steps.len(), 1, "같은 내용의 재기록은 멱등이다");
    assert_eq!(steps[0].1.key, CliRefusal::key(&call));
    assert_eq!(steps[0].1.key, "cli-refusal-cli-7-0");
    assert_eq!(steps[0].1.body.0, r#""invalid""#);
    // 같은 자리에 다른 단어는 저장소가 거절한다
    assert!(
        sink.refused(attempt, call, CliRefusal::UnknownTool)
            .await
            .is_err()
    );
}
