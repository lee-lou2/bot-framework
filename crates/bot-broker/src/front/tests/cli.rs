//! Tests for planned CLI tool-call verification against checkpointed turn groups.

use super::*;
use crate::front::cli::PlannedCalls;

pub(super) const OWNER_RUN: &str = "r1";

pub(super) fn planned(wait_ms: u64) -> Option<MakeCalls> {
    Some(Box::new(move |ledger, _| {
        Arc::new(
            PlannedCalls::new(ledger.clone(), Duration::from_millis(wait_ms))
                .with_poll(Duration::from_millis(10)),
        )
    }))
}

pub(super) fn call(id: &str, name: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"{name}","arguments":{args},"_meta":{{"claudecode/toolUseId":"{id}"}}}}}}"#
    )
}

/// Simulates turn-observer checkpoints: writes `cli-use-*` entries followed by `cli-group-<turn>`.
pub(super) fn record(r: &Rig, turn: u64, uses: &[(&str, &str, &str)]) {
    for (pos, (id, name, input)) in uses.iter().enumerate() {
        let body = format!(
            r#"{{"seq":{turn},"turn":{turn},"pos":{pos},"name":"mcp__relay__{name}","input":{input}}}"#
        );
        r.ledger
            .add_step(OWNER_RUN, &format!("cli-use-{id}"), &body);
    }
    let ids: Vec<String> = uses.iter().map(|(id, _, _)| format!("\"{id}\"")).collect();
    let body = format!(r#"{{"seq":{turn},"uses":[{}]}}"#, ids.join(","));
    r.ledger
        .add_step(OWNER_RUN, &format!("cli-group-{turn}"), &body);
}

#[tokio::test]
async fn a_planned_call_runs_once_under_its_logical_id_and_a_redelivery_reuses_it() {
    let r = rig_with(|_| {}, |_| {}, planned(300)).await;
    record(&r, 4, &[("toolu_a", "echo_say", r#"{"text":"hi"}"#)]);
    let reply = rpc(&r, &call("toolu_a", "echo_say", r#"{"text":"hi"}"#)).await;
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs()[0].call.0, "cli-4-0");
    rpc(&r, &call("toolu_a", "echo_say", r#"{"text":"hi"}"#)).await;
    assert_eq!(r.runner.jobs().len(), 1);
}

#[tokio::test]
async fn siblings_with_identical_tool_and_arguments_are_separate_calls_by_position() {
    let r = rig_with(|_| {}, |_| {}, planned(300)).await;
    let same = r#"{"text":"same"}"#;
    record(
        &r,
        7,
        &[("toolu_x", "echo_say", same), ("toolu_y", "echo_say", same)],
    );
    rpc(&r, &call("toolu_y", "echo_say", same)).await;
    rpc(&r, &call("toolu_x", "echo_say", same)).await;
    let ids: Vec<String> = r.runner.jobs().iter().map(|j| j.call.0.clone()).collect();
    assert_eq!(
        ids,
        ["cli-7-1", "cli-7-0"],
        "distinct positions must produce distinct logical call IDs"
    );
    record(&r, 9, &[("toolu_z", "echo_say", same)]);
    rpc(&r, &call("toolu_z", "echo_say", same)).await;
    assert_eq!(r.runner.jobs().len(), 3);
    assert_eq!(r.runner.jobs()[2].call.0, "cli-9-0");
}

#[tokio::test]
async fn without_a_complete_group_nothing_runs() {
    let r = rig_with(|_| {}, |_| {}, planned(120)).await;
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    // Use without a group is corrupt because both commit together.
    r.ledger.add_step(
        OWNER_RUN,
        "cli-use-toolu_b",
        r#"{"seq":2,"turn":2,"pos":0,"name":"mcp__relay__echo_say","input":{}}"#,
    );
    let reply = rpc(&r, &call("toolu_b", "echo_say", "{}")).await;
    let invalid: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(invalid["result"]["isError"], true);
    assert_eq!(invalid["result"]["content"][0]["text"], "invalid");
    r.ledger
        .add_step(OWNER_RUN, "cli-group-3", r#"{"seq":3,"uses":["toolu_c"]}"#);
    let reply = rpc(&r, &call("toolu_c", "echo_say", "{}")).await;
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    assert!(r.runner.jobs().is_empty());
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-2-0") && !r.ledger.has_call(OWNER_RUN, "m-toolu_a"));
}

#[tokio::test]
async fn a_group_that_shows_up_while_waiting_lets_the_call_through() {
    let r = rig_with(|_| {}, |_| {}, planned(2_000)).await;
    let (addr, request) = {
        let body = call("toolu_a", "echo_say", r#"{"text":"late"}"#);
        let token = format!("Bearer {}", r.token);
        (
            r.front.addr(),
            post("/mcp", Some(&token), "authorization", &body),
        )
    };
    let waiting = tokio::spawn(async move { raw(addr, &request).await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        r.runner.jobs().is_empty(),
        "must not execute before the group is visible"
    );
    record(&r, 5, &[("toolu_a", "echo_say", r#"{"text":"late"}"#)]);
    let reply = waiting.await.unwrap();
    assert!(reply.body.contains("ok"), "{}", reply.body);
    assert_eq!(r.runner.jobs()[0].call.0, "cli-5-0");
}

#[tokio::test]
async fn a_call_that_differs_from_the_plan_is_refused_before_it_runs() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    record(&r, 4, &[("toolu_a", "echo_say", r#"{"text":"planned"}"#)]);
    for (name, args) in [
        ("echo_say", r#"{"text":"changed"}"#),
        ("echo_say", r#"{"text":"planned","extra":1}"#),
        ("echo_drop", r#"{"text":"planned"}"#),
    ] {
        let reply = rpc(&r, &call("toolu_a", name, args)).await;
        assert!(
            reply.body.contains("conflict"),
            "{name} {args}: {}",
            reply.body
        );
    }
    assert!(r.runner.jobs().is_empty());
    // Equivalent canonical JSON matches regardless of key order or whitespace.
    record(&r, 6, &[("toolu_b", "echo_say", r#"{ "b": 1,  "a": 2 }"#)]);
    rpc(&r, &call("toolu_b", "echo_say", r#"{"a":2,"b":1}"#)).await;
    assert_eq!(r.runner.jobs()[0].call.0, "cli-6-0");
}

#[tokio::test]
async fn a_plan_that_contradicts_itself_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    r.ledger
        .add_step(OWNER_RUN, "cli-group-1", r#"{"seq":2,"uses":["toolu_a"]}"#);
    r.ledger.add_step(
        OWNER_RUN,
        "cli-use-toolu_a",
        r#"{"seq":2,"turn":2,"pos":0,"name":"mcp__relay__echo_say","input":{}}"#,
    );
    assert!(
        rpc(&r, &call("toolu_a", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    r.ledger.add_step(
        OWNER_RUN,
        "cli-group-3",
        r#"{"seq":3,"uses":["toolu_p","toolu_b"]}"#,
    );
    r.ledger.add_step(
        OWNER_RUN,
        "cli-use-toolu_b",
        r#"{"seq":3,"turn":3,"pos":0,"name":"mcp__relay__echo_say","input":{}}"#,
    );
    assert!(
        rpc(&r, &call("toolu_b", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn a_session_that_no_longer_owns_the_run_cannot_read_the_plan() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    r.ledger.bump_claim(OWNER_RUN);
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn revoking_while_the_plan_is_awaited_answers_at_once_and_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(5_000)).await;
    let (addr, request) = {
        let body = call("toolu_a", "echo_say", "{}");
        let token = format!("Bearer {}", r.token);
        (
            r.front.addr(),
            post("/mcp", Some(&token), "authorization", &body),
        )
    };
    let waiting = tokio::spawn(async move {
        let began = tokio::time::Instant::now();
        (raw(addr, &request).await, began.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    r.sessions.revoke(&r.token);
    let (reply, took) = waiting.await.unwrap();
    assert!(reply.body.contains("stale"), "{}", reply.body);
    assert!(
        took < Duration::from_secs(2),
        "must abort immediately on revocation: {took:?}"
    );
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r.runner.jobs().is_empty());
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
}

#[tokio::test]
async fn only_the_exact_relay_prefix_can_stand_for_the_mcp_name() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    for (i, planned_name) in [
        "mcp__other__echo_say",
        "echo_say",
        "Bash",
        "mcp__relay__echo_drop",
        "mcp__relay__mcp__relay__echo_say",
    ]
    .iter()
    .enumerate()
    {
        let id = format!("toolu_{i}");
        r.ledger.add_step(
            OWNER_RUN,
            &format!("cli-use-{id}"),
            &format!(r#"{{"seq":{i},"turn":{i},"pos":0,"name":"{planned_name}","input":{{}}}}"#),
        );
        r.ledger.add_step(
            OWNER_RUN,
            &format!("cli-group-{i}"),
            &format!(r#"{{"seq":{i},"uses":["{id}"]}}"#),
        );
        let reply = rpc(&r, &call(&id, "echo_say", "{}")).await;
        assert!(
            reply.body.contains("conflict"),
            "{planned_name}: {}",
            reply.body
        );
    }
    assert!(r.runner.jobs().is_empty());
}

#[tokio::test]
async fn a_group_with_a_missing_repeated_or_misplaced_use_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    let put = |id: &str, turn: u64, pos: u64| {
        r.ledger.add_step(
            OWNER_RUN,
            &format!("cli-use-{id}"),
            &format!(
                r#"{{"seq":{turn},"turn":{turn},"pos":{pos},"name":"mcp__relay__echo_say","input":{{}}}}"#
            ),
        );
    };
    let group = |turn: u64, ids: &str| {
        r.ledger.add_step(
            OWNER_RUN,
            &format!("cli-group-{turn}"),
            &format!(r#"{{"seq":{turn},"uses":[{ids}]}}"#),
        );
    };
    // Missing sibling use blocks the entire group.
    put("toolu_a", 1, 0);
    group(1, r#""toolu_a","toolu_b""#);
    assert!(
        rpc(&r, &call("toolu_a", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    // Duplicate ID within the group.
    put("toolu_c", 2, 0);
    group(2, r#""toolu_c","toolu_c""#);
    assert!(
        rpc(&r, &call("toolu_c", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    // Mismatched pos or turn on a sibling use.
    put("toolu_d", 3, 0);
    put("toolu_e", 3, 5);
    group(3, r#""toolu_d","toolu_e""#);
    assert!(
        rpc(&r, &call("toolu_d", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    put("toolu_f", 4, 0);
    put("toolu_g", 9, 1);
    group(4, r#""toolu_f","toolu_g""#);
    assert!(
        rpc(&r, &call("toolu_f", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    assert!(r.runner.jobs().is_empty());
    put("toolu_h", 6, 0);
    put("toolu_i", 6, 1);
    group(6, r#""toolu_h","toolu_i""#);
    rpc(&r, &call("toolu_i", "echo_say", "{}")).await;
    assert_eq!(r.runner.jobs()[0].call.0, "cli-6-1");
}

#[tokio::test]
async fn the_wait_and_the_ledger_reads_share_one_absolute_deadline() {
    let r = rig_with(|_| {}, |_| {}, planned(10)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    r.ledger.set_context_delay(Duration::from_millis(200));
    let began = tokio::time::Instant::now();
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    let took = began.elapsed();
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    assert!(
        took < Duration::from_millis(150),
        "exceeded deadline: {took:?}"
    );
    assert!(r.runner.jobs().is_empty());
}

async fn with_cache(limit: usize) -> (Rig, Arc<PlannedCalls>) {
    let slot: Arc<std::sync::Mutex<Option<Arc<PlannedCalls>>>> = Arc::default();
    let put = slot.clone();
    let r = rig_with(
        |_| {},
        |_| {},
        Some(Box::new(move |ledger, _| {
            let book = Arc::new(
                PlannedCalls::new(ledger.clone(), Duration::from_millis(300))
                    .with_poll(Duration::from_millis(10))
                    .with_cache_limit(limit),
            );
            *put.lock().unwrap() = Some(book.clone());
            book
        })),
    )
    .await;
    let book = slot.lock().unwrap().clone().expect("book");
    (r, book)
}

#[tokio::test]
async fn the_memory_of_verified_groups_stays_bounded_and_forgetting_is_safe() {
    let (r, book) = with_cache(3).await;
    for turn in 1..=10u64 {
        let id = format!("toolu_{turn}");
        record(&r, turn, &[(id.as_str(), "echo_say", "{}")]);
        rpc(&r, &call(&id, "echo_say", "{}")).await;
        assert!(
            book.cached_groups() <= 3,
            "turn {turn}: {}",
            book.cached_groups()
        );
    }
    assert_eq!(book.cached_groups(), 3);
    assert_eq!(r.runner.jobs().len(), 10);
    rpc(&r, &call("toolu_1", "echo_say", "{}")).await;
    assert_eq!(r.runner.jobs().len(), 10);
    assert!(book.cached_groups() <= 3);
}

#[tokio::test]
async fn a_forgotten_group_is_checked_again_in_full() {
    let (r, book) = with_cache(1).await;
    record(
        &r,
        1,
        &[("toolu_a", "echo_say", "{}"), ("toolu_b", "echo_say", "{}")],
    );
    rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert_eq!(book.cached_groups(), 1);
    // Corrupting a cached group is not detected until eviction forces full re-verification.
    r.ledger.remove_step(OWNER_RUN, "cli-use-toolu_b");
    rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert_eq!(
        r.runner.jobs().len(),
        1,
        "duplicate call ID must not re-execute"
    );
    record(&r, 2, &[("toolu_c", "echo_say", "{}")]);
    rpc(&r, &call("toolu_c", "echo_say", "{}")).await;
    assert_eq!(book.cached_groups(), 1);
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("invalid"), "{}", reply.body);
}

#[tokio::test]
async fn a_limit_of_zero_remembers_nothing_and_checks_every_time() {
    let (r, book) = with_cache(0).await;
    record(
        &r,
        1,
        &[("toolu_a", "echo_say", "{}"), ("toolu_b", "echo_say", "{}")],
    );
    rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert_eq!(book.cached_groups(), 0);
    r.ledger.remove_step(OWNER_RUN, "cli-use-toolu_b");
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("invalid"), "{}", reply.body);
}
