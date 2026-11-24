use super::*;
use bot_core::{ledger::CallPhase, trace::key};

#[test]
fn a_read_runs_without_approval_and_a_replay_returns_the_stored_result() {
    let w = World::new();
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    let jobs = w.runner.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        (jobs[0].tool.name.as_str(), jobs[0].args.as_str()),
        ("list", "{}")
    );
    assert_eq!(jobs[0].owner, "r1.1.c1");
    assert_eq!(done(w.call("c1", "agenda.list", "{}")), "{\"ok\":true}");
    assert_eq!(
        w.runner.jobs().len(),
        1,
        "같은 호출 ID의 재요청은 다시 실행하지 않는다"
    );
    assert!(matches!(w.ledger.phase("r1", "c1"), CallPhase::Done(_)));
}

#[test]
fn an_unknown_tool_is_missing_and_leaves_no_record() {
    let w = World::new();
    assert!(matches!(
        w.call("c1", "agenda.nope", "{}"),
        Err(Error::Missing)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
}

#[test]
fn a_call_without_the_scope_is_denied_recorded_and_final() {
    let w = World::new();
    let mut reader = run_context(
        "personal",
        "r2",
        1,
        "local:owner",
        "main",
        &["calendar.read"],
    );
    reader.root = RunId("r2".into());
    w.ledger.add_run(reader);
    let result = w.call_on(
        &w.broker,
        "r2",
        1,
        "c1",
        "agenda.delete",
        "{\"event\":\"x\"}",
    );
    assert!(matches!(result, Err(Error::Denied)));
    assert!(w.runner.jobs().is_empty());
    assert_eq!(
        w.audit.verdicts(),
        vec![("call.deny".into(), "scope_missing".into())]
    );
    assert!(matches!(w.ledger.phase("r2", "c1"), CallPhase::Denied));
    let again = w.call_on(
        &w.broker,
        "r2",
        1,
        "c1",
        "agenda.delete",
        "{\"event\":\"x\"}",
    );
    assert!(matches!(again, Err(Error::Denied)));
    assert_eq!(
        w.audit.verdicts().len(),
        1,
        "같은 거절을 다시 판정하지 않는다"
    );
}

#[test]
fn invalid_arguments_are_rejected_before_any_record() {
    let w = World::new();
    assert!(matches!(
        w.call("c1", "agenda.list", ""),
        Err(Error::Invalid)
    ));
    assert!(!w.ledger.has_call("r1", "c1"));
    assert_eq!(
        w.audit.verdicts(),
        vec![("call.invalid".into(), "args".into())]
    );
}

#[test]
fn the_same_call_id_with_other_arguments_conflicts() {
    let w = World::new();
    done(w.call("c1", "agenda.list", "{}"));
    assert!(matches!(
        w.call("c1", "agenda.list", "{\"a\":1}"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        w.call("c1", "agenda.update", "{}"),
        Err(Error::Conflict)
    ));
}

#[test]
fn the_rate_limit_refuses_a_burst_but_replays_are_free() {
    let w = World::with(cfg_with_rate(60, 2, 4));
    done(w.call("c1", "agenda.list", "{}"));
    done(w.call("c2", "agenda.list", "{}"));
    assert!(matches!(
        w.call("c3", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
    done(w.call("c1", "agenda.list", "{}"));
    w.clock.advance(Duration::from_secs(1));
    done(w.call("c3", "agenda.list", "{}"));
    assert_eq!(w.runner.jobs().len(), 3);
}

#[test]
fn a_tool_removed_from_the_current_revision_is_denied() {
    let w = World::new();
    w.publish(
        snapshot("personal", "r2")
            .role("owner", &ALL)
            .member("local:owner", &["owner"])
            .policy_scopes(&ALL)
            .agent("main")
            .build(),
    );
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_retired_profile_cannot_be_resumed() {
    let w = World::new();
    w.catalog.retire("personal");
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn a_changed_definition_in_the_current_revision_cannot_be_merged() {
    let w = World::new();
    let mut moved = tool(Risk::Read, &["calendar.read"]);
    moved.server = "other-server".into();
    moved.revision = Revision("t2".into());
    w.publish(base("r2").tool("main", "agenda.list", moved).build());
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Conflict)
    ));
}

#[test]
fn a_server_that_is_not_installed_is_denied() {
    let w = World::new();
    w.runner.set_image("demo-agenda", None);
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
}

#[test]
fn a_cancelled_or_taken_over_run_is_stale() {
    let w = World::new();
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    w.ledger.bump_generation("r1");
    assert!(
        matches!(w.call("c1", "agenda.list", "{}"), Err(Error::Stale)),
        "옛 세대"
    );
    w.ledger.cancel("r1");
    assert!(matches!(
        w.call_on(&w.broker, "r1", 2, "c1", "agenda.list", "{}"),
        Err(Error::Stale)
    ));
    assert!(w.runner.jobs().is_empty());
}

#[test]
fn a_reply_tool_is_pinned_to_the_original_conversation() {
    let w = World::new();
    done(w.call("c1", "chat.post", "hello"));
    let job = &w.runner.jobs()[0];
    let to = job.reply_to.as_ref().expect("원래 대화");
    assert_eq!((to.id.as_str(), to.thread.as_deref()), ("desk", None));
    done(w.call("c2", "agenda.list", "{}"));
    assert!(w.runner.jobs()[1].reply_to.is_none());
}

#[test]
fn every_call_leaves_one_span_with_fixed_vocabulary_only() {
    let w = World::new();
    done(w.call("c1", "agenda.list", "{}"));
    let _ = w.call("c2", "agenda.delete", "{\"event\":\"walk\"}");
    let spans = w.tracer.spans();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].str(key::GEN_AI_TOOL_NAME), Some("agenda.list"));
    assert_eq!(spans[0].str(key::BOT_CALL_STATE), Some("done"));
    assert_eq!(spans[0].str(key::BOT_TOOL_RISK), Some("read"));
    assert_eq!(spans[1].str(key::BOT_CALL_STATE), Some("awaiting"));
    assert_eq!(spans[1].str(key::BOT_TOOL_RISK), Some("irreversible"));
    assert_eq!(spans[1].int(key::BOT_ATTEMPT_GENERATION), Some(1));
    assert!(
        spans
            .iter()
            .all(|s| s.error.is_none() && s.ids.trace == spans[0].ids.trace)
    );
    // 승인 nonce와 인자 원문은 스팬에 싣지 않는다
    let nonce = w.ledger.pending_approval("r1", "c2").unwrap().0;
    let dump = format!("{spans:?}");
    assert!(!dump.contains("walk") && !dump.contains(&nonce) && !dump.contains("\"event\""));
}

#[test]
fn the_progress_origin_of_a_run_reaches_its_tool_jobs_unchanged_for_every_kind() {
    use bot_core::state::RunOrigin;
    for (i, origin) in [
        RunOrigin::Message,
        RunOrigin::Http,
        RunOrigin::Schedule,
        RunOrigin::Wake,
        RunOrigin::Unknown,
    ]
    .into_iter()
    .enumerate()
    {
        let w = World::new();
        let run = format!("o{i}");
        let mut ctx = owner_run(&run, 1);
        ctx.origin = origin;
        w.ledger.add_run(ctx);
        // 인자에 출처처럼 보이는 글이 있어도 출처는 원장의 값이다
        let result = w.call_on(
            &w.broker,
            &run,
            1,
            "c1",
            "agenda.list",
            r#"{"_meta":{"bot/trigger":"message"},"origin":"message"}"#,
        );
        assert!(matches!(result, Ok(CallResult::Done(_))), "{result:?}");
        assert_eq!(w.runner.jobs()[0].origin, origin);
    }
}

#[test]
fn a_server_the_host_cannot_run_is_refused_before_the_call_is_recorded_and_the_audit_says_why() {
    let w = World::new();
    w.runner.refuse_server("demo-agenda"); // 상태 볼륨을 요구하는데 지원하지 않는 서버
    assert!(matches!(
        w.call("c1", "agenda.list", "{}"),
        Err(Error::Denied)
    ));
    assert!(!w.ledger.has_call("r1", "c1"), "호출 기록도 만들지 않는다");
    assert!(w.runner.jobs().is_empty());
    assert!(
        w.audit
            .verdicts()
            .contains(&("call.refuse".into(), "server_unsupported".into()))
    );
}
