//! Claude Code session resume tests: pending slot settlement, trusted ledger recall, and refusal replay.

use super::*;

#[tokio::test]
async fn resuming_fills_the_pending_slot_before_the_file_and_continues_without_the_original_request()
 {
    let port = FakePort::new(saved_steps(4));
    port.settle
        .lock()
        .unwrap()
        .push_back(Ok(Slot::Result(ToolOutput::from_payload(
            r#"{"content":[{"type":"text","text":"done"}],"isError":false}"#,
        ))));
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        let text = std::fs::read_to_string(session_file(spec)).unwrap();
        *sink.lock().unwrap() = text.lines().map(str::to_owned).collect();
        append(spec, lines()[5]);
    });
    let out = runner(&sandbox, "resume")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { .. }));
    assert_eq!(
        *port.settled.lock().unwrap(),
        [(3, 0, "toolu_probe1".to_owned())],
        "only unsettled slots should be settled"
    );
    let commits = port.commits();
    assert_eq!(
        commits.len(),
        1,
        "one result line; appended assistant line has no tools"
    );
    assert_eq!(keys(&commits[0]), ["cli-rec-4"]);
    let written = seen.lock().unwrap().clone();
    assert_eq!(written.len(), 5);
    assert_eq!(&written[..4], &lines()[..4]);
    let result: serde_json::Value = serde_json::from_str(&written[4]).unwrap();
    assert_eq!(result["parentUuid"], "cf3bcfae-3ae9-4dbb-94dd-7fb7c1abaf9f");
    let block = &result["message"]["content"][0];
    assert_eq!(block["tool_use_id"], "toolu_probe1");
    assert_eq!(block["content"], json!([{"type":"text","text":"done"}]));
    assert_eq!(
        commits[0][0].body.0,
        serde_json::to_string(&written[4]).unwrap()
    );

    let spec = &sandbox.specs()[0];
    let at = spec.args.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(spec.args[at + 1], sid());
    assert!(
        !spec
            .args
            .iter()
            .any(|a| a == "--session-id" || a == "--no-session-persistence")
    );
    assert!(spec.args.iter().any(|a| a == prompt::RESUME_INSTRUCTION));
    assert!(!spec.args.iter().any(|a| a == prompt::INSTRUCTION));
    assert!(
        spec.stdin.is_none(),
        "resume must not re-send the original request on stdin"
    );
}

#[tokio::test]
async fn a_resumed_session_with_nothing_pending_writes_no_result_line() {
    let mut steps = saved_steps(4);
    steps.push(crate::model::session::rec_step(4, lines()[4]));
    let port = FakePort::new(steps);
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        let text = std::fs::read_to_string(session_file(spec)).unwrap();
        *sink.lock().unwrap() = text.lines().map(str::to_owned).collect();
    });
    runner(&sandbox, "answered")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(port.settled.lock().unwrap().is_empty());
    assert_eq!(
        *port.recalled.lock().unwrap(),
        [(3, 0, "toolu_probe1".to_owned())]
    );
    assert!(port.commits().is_empty(), "no new result line expected");
    let written = seen.lock().unwrap().clone();
    assert_eq!(written.len(), 5);
    let result: serde_json::Value = serde_json::from_str(&written[4]).unwrap();
    let block = &result["message"]["content"][0];
    assert_eq!(
        block["content"],
        json!([{"type":"text","text":"ledger result"}])
    );
    assert_eq!(block["is_error"], false);
    assert!(result.get("toolUseResult").is_none());
    assert_eq!(
        result["uuid"], "57caa3ad-f7ba-4d4f-b85b-13f330f99dae",
        "uuid chain must remain intact"
    );
}

#[tokio::test]
async fn an_answered_relay_tool_without_a_finished_call_is_marked_aborted_never_executed() {
    let mut steps = saved_steps(4);
    steps.push(crate::model::session::rec_step(4, lines()[4]));
    let port = FakePort::new(steps);
    port.unfinished.lock().unwrap().insert((3, 0));
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        let text = std::fs::read_to_string(session_file(spec)).unwrap();
        *sink.lock().unwrap() = text.lines().map(str::to_owned).collect();
    });
    runner(&sandbox, "unfinished")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(
        port.settled.lock().unwrap().is_empty(),
        "answered calls without ledger completion must not re-execute"
    );
    let written = seen.lock().unwrap().clone();
    let result: serde_json::Value = serde_json::from_str(&written[4]).unwrap();
    let block = &result["message"]["content"][0];
    assert_eq!(block["content"], json!([{"type":"text","text":"aborted"}]));
    assert_eq!(block["is_error"], true);
}

#[tokio::test]
async fn a_denied_answered_tool_is_replaced_by_the_fixed_word_in_the_restored_session() {
    let mut steps = saved_steps(4);
    steps.push(crate::model::session::rec_step(4, lines()[4]));
    let port = FakePort::new(steps);
    port.recall
        .lock()
        .unwrap()
        .push_back(Some(ToolOutput::notice("denied")));
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        let text = std::fs::read_to_string(session_file(spec)).unwrap();
        *sink.lock().unwrap() = text.lines().map(str::to_owned).collect();
    });
    runner(&sandbox, "denied")
        .step(request(port))
        .await
        .unwrap();
    let written = seen.lock().unwrap().clone();
    let result: serde_json::Value = serde_json::from_str(&written[4]).unwrap();
    let block = &result["message"]["content"][0];
    assert_eq!(block["content"], json!([{"type":"text","text":"denied"}]));
    assert_eq!(block["is_error"], true);
}

#[tokio::test]
async fn a_confirmed_denial_reads_the_same_after_every_resume() {
    let first = FakePort::new(saved_steps(4));
    first
        .settle
        .lock()
        .unwrap()
        .push_back(Ok(Slot::Result(ToolOutput::notice("denied"))));
    let sandbox = FakeSandbox::new([slow()]);
    runner(&sandbox, "denied-1")
        .step(request(first.clone()))
        .await
        .unwrap();
    let written = first.commits();
    assert_eq!(written.len(), 1);
    let mut steps = saved_steps(4);
    steps.extend(written[0].clone());
    let second = FakePort::new(steps);
    second
        .recall
        .lock()
        .unwrap()
        .push_back(Some(ToolOutput::notice("denied")));
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        let text = std::fs::read_to_string(session_file(spec)).unwrap();
        *sink.lock().unwrap() = text.lines().map(str::to_owned).collect();
    });
    runner(&sandbox, "denied-2")
        .step(request(second.clone()))
        .await
        .unwrap();
    assert!(
        second.settled.lock().unwrap().is_empty(),
        "already settled refusal must not re-execute"
    );
    let lines = seen.lock().unwrap().clone();
    let result: serde_json::Value = serde_json::from_str(&lines[4]).unwrap();
    let block = &result["message"]["content"][0];
    assert_eq!(block["content"], json!([{"type":"text","text":"denied"}]));
    assert_eq!(block["is_error"], true);
}
