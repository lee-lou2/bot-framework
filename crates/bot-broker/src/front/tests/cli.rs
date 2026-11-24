//! CLI 재개 규약: 저장된 모델 차례의 계획과 대조해야만 도구가 실행된다.

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

/// ②가 남기는 기록을 흉내 낸다: 한 모델 응답의 도구들(`(id, name, input)`)을 먼저 쓰고 그룹을 마지막에 쓴다.
pub(super) fn record(r: &Rig, turn: u64, uses: &[(&str, &str, &str)]) {
    for (pos, (id, name, input)) in uses.iter().enumerate() {
        // 제공자가 보는 이름은 릴레이 접두사가 붙는다
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
    // 같은 toolUseId를 다시 보내도(재전달·재개) 같은 호출이라 다시 실행하지 않는다
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
        "같은 인자라도 위치가 다르면 다른 호출이다"
    );
    // 새 차례의 같은 도구·인자는 또 새 호출이다
    record(&r, 9, &[("toolu_z", "echo_say", same)]);
    rpc(&r, &call("toolu_z", "echo_say", same)).await;
    assert_eq!(r.runner.jobs().len(), 3);
    assert_eq!(r.runner.jobs()[2].call.0, "cli-9-0");
}

#[tokio::test]
async fn without_a_complete_group_nothing_runs() {
    let r = rig_with(|_| {}, |_| {}, planned(120)).await;
    // 계획이 아예 없다
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    // 한 묶음으로 커밋한 사용에 그룹이 없으면 손상된 기록이다.
    r.ledger.add_step(
        OWNER_RUN,
        "cli-use-toolu_b",
        r#"{"seq":2,"turn":2,"pos":0,"name":"mcp__relay__echo_say","input":{}}"#,
    );
    let reply = rpc(&r, &call("toolu_b", "echo_say", "{}")).await;
    let invalid: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(invalid["result"]["isError"], true);
    assert_eq!(invalid["result"]["content"][0]["text"], "invalid");
    // 그룹에는 있는데 `cli-use`가 없다
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
        "그룹이 보이기 전에는 실행하지 않는다"
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
    // 인자가 다르다 / 도구가 다르다
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
    // 키 순서·공백이 달라도 정규 JSON이 같으면 같은 인자다
    record(&r, 6, &[("toolu_b", "echo_say", r#"{ "b": 1,  "a": 2 }"#)]);
    rpc(&r, &call("toolu_b", "echo_say", r#"{"a":2,"b":1}"#)).await;
    assert_eq!(r.runner.jobs()[0].call.0, "cli-6-0");
}

#[tokio::test]
async fn a_plan_that_contradicts_itself_runs_nothing() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    // 그룹의 차례와 키가 다르다 / use의 위치가 그룹 안 순서와 다르다 / 같은 id가 두 그룹에 있다
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
        "기다리지 않고 바로 끝난다: {took:?}"
    );
    // 계획이 뒤늦게 나타나도 이미 회수된 세션으로는 아무것도 실행되지 않는다
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r.runner.jobs().is_empty());
    assert!(!r.ledger.has_call(OWNER_RUN, "cli-4-0"));
}

#[tokio::test]
async fn only_the_exact_relay_prefix_can_stand_for_the_mcp_name() {
    let r = rig_with(|_| {}, |_| {}, planned(150)).await;
    // 다른 MCP 서버·내장 도구·접두사 없는 이름은 같은 도구 이름으로 대신할 수 없다
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
    // 그룹에는 둘인데 하나의 use가 없다: 다른 하나도 실행하지 않는다
    put("toolu_a", 1, 0);
    group(1, r#""toolu_a","toolu_b""#);
    assert!(
        rpc(&r, &call("toolu_a", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    // 그룹 안에서 ID가 겹친다
    put("toolu_c", 2, 0);
    group(2, r#""toolu_c","toolu_c""#);
    assert!(
        rpc(&r, &call("toolu_c", "echo_say", "{}"))
            .await
            .body
            .contains("invalid")
    );
    // 같은 그룹의 다른 use의 위치·차례가 어긋난다
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
    // 완전한 그룹은 그대로 통한다
    put("toolu_h", 6, 0);
    put("toolu_i", 6, 1);
    group(6, r#""toolu_h","toolu_i""#);
    rpc(&r, &call("toolu_i", "echo_say", "{}")).await;
    assert_eq!(r.runner.jobs()[0].call.0, "cli-6-1");
}

#[tokio::test]
async fn the_wait_and_the_ledger_reads_share_one_absolute_deadline() {
    // 기다림은 10ms인데 원장이 한 번 읽는 데 200ms 걸린다: 읽기가 끝나기를 기다리지 않고 기한에 끊는다
    let r = rig_with(|_| {}, |_| {}, planned(10)).await;
    record(&r, 4, &[("toolu_a", "echo_say", "{}")]);
    r.ledger.set_context_delay(Duration::from_millis(200));
    let began = tokio::time::Instant::now();
    let reply = rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    let took = began.elapsed();
    assert!(reply.body.contains("unavailable"), "{}", reply.body);
    assert!(took < Duration::from_millis(150), "기한을 넘겼다: {took:?}");
    assert!(r.runner.jobs().is_empty());
}

/// 한도를 정해 만든 `PlannedCalls`와 그 손잡이(기억한 그룹 수를 보려고).
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
    // 그룹 열 개를 차례로 쓴다: 기억은 상한을 넘지 않는다
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
    // 잊은 그룹을 다시 쓰면 다시 읽어 대조하고 같은 호출 ID로 간다(새로 실행하지 않는다)
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
    rpc(&r, &call("toolu_a", "echo_say", "{}")).await; // 그룹 전체를 맞춰 보고 기억한다
    assert_eq!(book.cached_groups(), 1);
    // 기억하는 동안은 다시 읽지 않는다(그룹 기록은 바뀌지 않는다고 본다: 여기서는 일부러 기록을 망가뜨려 캐시가 쓰이는지만 본다)
    r.ledger.remove_step(OWNER_RUN, "cli-use-toolu_b");
    rpc(&r, &call("toolu_a", "echo_say", "{}")).await;
    assert_eq!(
        r.runner.jobs().len(),
        1,
        "같은 호출이라 다시 실행하지 않는다"
    );
    // 다른 그룹이 기억을 밀어내면(상한 1) 원래 그룹은 잊히고, 다시 쓰면 전체를 다시 대조해 망가진 기록을 잡는다
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
