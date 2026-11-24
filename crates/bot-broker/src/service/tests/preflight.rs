//! 실행 전 확정 거절의 사전 판정(`Preflight::refusal`): 실행이 고정한 판본만 보고, 저장소·판본 오류는 확정이 아니다.
//! 일반 `Broker::call`의 `Missing`·`Invalid`에는 저장소 오류가 섞이므로, 확정 여부는 이 판정이 따로 가린다.

use super::*;
use crate::{ports::Sealed, service::preflight::Preflight};
use bot_core::{cli::CliRefusal, profile::Tool, state::Risk};

/// 인자 글에 `bad`가 있으면 `Invalid`(스키마 위반), `boom`이 있으면 `Unavailable`(봉인기 장애)로 실패하는 봉인기.
struct PickySealer;

impl Sealer for PickySealer {
    fn seal(&self, tool: &Tool, args: &JsonPayload) -> Result<Sealed, Error> {
        if args.0.contains("bad") {
            return Err(Error::Invalid);
        }
        if args.0.contains("boom") {
            return Err(Error::Unavailable);
        }
        FakeSealer.seal(tool, args)
    }
}

fn attempt(w: &World, run: &str) -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId(run.into()),
        claim: Some(w.ledger.claim_epoch(run)),
        generation: Generation(1),
    }
}

fn ask(
    w: &World,
    broker: &BrokerService,
    tool: &str,
    args: &str,
) -> Result<Option<CliRefusal>, Error> {
    block_on(broker.refusal(
        attempt(w, "r1"),
        ToolId(tool.into()),
        JsonPayload(args.into()),
    ))
}

#[test]
fn a_tool_the_pinned_revision_does_not_show_the_agent_is_a_confirmed_unknown_tool() {
    let w = World::new();
    assert_eq!(
        ask(&w, &w.broker, "agenda.nothing", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    // 일반 호출은 같은 자리를 `Missing`으로 거절한다(저장소 오류도 같은 `Missing`이라 그 결과만으로는 확정할 수 없다)
    assert!(matches!(
        w.call("c1", "agenda.nothing", "{}"),
        Err(Error::Missing)
    ));
    // 다른 에이전트(자식)만 선언한 도구는 이 에이전트에게 보이지 않는다
    w.publish(
        base("r1")
            .tool(
                "main/research",
                "research.only",
                tool(Risk::Read, &["calendar.read"]),
            )
            .build(),
    );
    assert_eq!(
        ask(&w, &w.broker, "research.only", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    // 보이는 도구는 거절이 아니다
    assert_eq!(ask(&w, &w.broker, "agenda.list", "{}").unwrap(), None);
}

#[test]
fn only_the_pinned_revision_decides_a_tool_withdrawn_from_the_current_one_is_not_confirmed() {
    let w = World::new();
    // 지금 활성 판본에서는 그 도구가 빠졌다(고정 판본 r1에는 있다)
    w.publish(
        snapshot("personal", "r2")
            .role("owner", &ALL)
            .member("local:owner", &["owner"])
            .policy_scopes(&ALL)
            .build(),
    );
    assert_eq!(ask(&w, &w.broker, "agenda.list", "{}").unwrap(), None);
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn an_input_the_pinned_declaration_rejects_is_invalid_but_a_sealer_failure_is_not() {
    let w = World::new();
    let picky = w.bare_with(Arc::new(PickySealer));
    assert_eq!(
        ask(&w, &picky, "agenda.list", r#"{"x":"bad"}"#).unwrap(),
        Some(CliRefusal::Invalid)
    );
    assert!(matches!(
        w.call_on(&picky, "r1", 1, "c1", "agenda.list", r#"{"x":"bad"}"#),
        Err(Error::Invalid)
    ));
    assert_eq!(
        ask(&w, &picky, "agenda.list", r#"{"x":"ok"}"#).unwrap(),
        None
    );
    // 봉인기 자체의 장애는 입력 거절이 아니다(확정하지 않고 오류로 돌려준다)
    assert!(matches!(
        ask(&w, &picky, "agenda.list", r#"{"x":"boom"}"#),
        Err(Error::Unavailable)
    ));
}

#[test]
fn an_unverifiable_revision_or_a_lost_claim_or_a_down_ledger_confirms_nothing() {
    let w = World::new();
    // 고정 판본을 카탈로그에서 찾을 수 없다: 일반 호출은 `Missing`이지만 도구가 없다는 확정이 아니다
    let mut ctx = run_context("personal", "r5", 1, "local:owner", "main", &ALL);
    ctx.work.revision = Revision("gone".into());
    w.ledger.add_run(ctx);
    let lost = block_on(w.broker.refusal(
        attempt(&w, "r5"),
        ToolId("agenda.list".into()),
        JsonPayload("{}".into()),
    ));
    assert!(matches!(lost, Err(Error::Unavailable)), "{lost:?}");
    assert!(matches!(
        w.call_on(&w.broker, "r5", 1, "c1", "agenda.list", "{}"),
        Err(Error::Missing)
    ));
    // claim을 잃은 시도
    let old = attempt(&w, "r1");
    w.ledger.bump_claim("r1");
    let result = block_on(w.broker.refusal(
        old,
        ToolId("agenda.nothing".into()),
        JsonPayload("{}".into()),
    ));
    assert!(matches!(result, Err(Error::Stale)), "{result:?}");
    // 원장이 답하지 못한다
    w.ledger.set_context_down(true);
    assert!(matches!(
        ask(&w, &w.broker, "agenda.nothing", "{}"),
        Err(Error::Unavailable)
    ));
}

#[test]
fn host_builtin_tools_are_judged_only_by_their_own_argument_checks_never_by_the_profile() {
    let w = World::new();
    // 바른 인자는 확정 거절이 아니다(내장 도구는 프로필의 도구 선언을 보지 않는다)
    for (id, args) in [
        ("memory.share", r#"{"id":"m-1"}"#),
        (
            "schedule.create",
            r#"{"at":"2030-01-01T12:00:00Z","instruction":"x"}"#,
        ),
        ("schedule.list", "{}"),
        ("schedule.cancel", r#"{"id":"s-1"}"#),
        ("delegate", r#"{"agent":"main/research","instruction":"x"}"#),
    ] {
        assert_eq!(ask(&w, &w.broker, id, args).unwrap(), None, "{id}");
    }
    // 호출을 기록하기 전에 틀린 인자(구조·문법·지나간 시각)는 확정 거절이다: 브로커가 그 호출을 `Invalid`로 거절하는 것과 같은 검사다
    for (id, args) in [
        ("memory.share", "{}"),
        ("memory.share", r#"{"id":"m-1","extra":1}"#),
        ("schedule.create", "{}"),
        (
            "schedule.create",
            r#"{"at":"1970-01-02T00:00:00Z","instruction":"x"}"#,
        ),
        ("schedule.list", r#"{"x":1}"#),
        ("schedule.cancel", "{}"),
    ] {
        assert_eq!(
            ask(&w, &w.broker, id, args).unwrap(),
            Some(CliRefusal::Invalid),
            "{id} {args}"
        );
    }
    // 호스트 전용 ID는 모델이 부를 수 없다: 늘 `unknown_tool`이다. 위임은 프런트가 입력·인가를 직접 판정한다
    assert_eq!(
        ask(&w, &w.broker, "schedule.deliver", "{}").unwrap(),
        Some(CliRefusal::UnknownTool)
    );
    assert_eq!(ask(&w, &w.broker, "delegate", "{}").unwrap(), None);
    assert!(w.ledger.call_ids("r1").is_empty(), "호출을 기록하지 않는다");
}

#[test]
fn asking_changes_nothing_and_always_gives_the_same_answer() {
    let w = World::new();
    for _ in 0..3 {
        assert_eq!(
            ask(&w, &w.broker, "agenda.nothing", "{}").unwrap(),
            Some(CliRefusal::UnknownTool)
        );
    }
    assert!(w.ledger.call_ids("r1").is_empty(), "호출을 기록하지 않는다");
    assert!(w.audit.entries().is_empty(), "감사에도 남기지 않는다");
    assert!(w.runner.jobs().is_empty());
}
