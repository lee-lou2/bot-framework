//! Tests for Claude Code session completion verification, draining, and deadline enforcement.

use super::*;

#[tokio::test]
async fn a_placeholder_result_stops_the_cli_and_fails_the_attempt_instead_of_letting_it_finish() {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    port.unfinished.lock().unwrap().insert((3, 0));
    let sandbox = FakeSandbox::new([slow().hangs()]).inspect(|spec| {
        for line in &lines()[..5] {
            append(spec, line);
        }
    });
    let error = runner(&sandbox, "freeze")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_unsaved",
            transient: true
        },
        "retries so host resume can settle the unfinished slot"
    );
    assert_eq!(sandbox.killed(), 1, "CLI must be killed once stuck");
    assert!(
        port.commits().is_empty(),
        "batch containing a placeholder result must not be committed"
    );
}

#[tokio::test]
async fn a_cli_that_ends_while_a_relay_call_could_not_be_saved_does_not_succeed() {
    let port = FakePort::new(vec![]);
    let sandbox = sandbox_writing(4);
    let error = runner(&sandbox, "unsaved-end")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_unsaved",
            transient: true
        }
    );
    assert!(port.commits().is_empty());
}

fn oversized_line() -> String {
    json!({"type":"user","uuid":"big","message":{"role":"user","content":"x".repeat(crate::model::session::MAX_LINE_BYTES)}}).to_string()
}

const UNSAVED: ModelError = ModelError::Failed {
    code: "session_unsaved",
    transient: true,
};

#[tokio::test]
async fn a_saved_plan_whose_call_never_ran_does_not_succeed_even_when_the_plan_landed_late() {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    port.never_ran.lock().unwrap().insert((3, 0));
    let sandbox = sandbox_writing(4);
    let error = runner(&sandbox, "late-plan")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(error, UNSAVED);
    assert_eq!(port.commits().len(), 1, "plan was committed");
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    let out = runner(&sandbox_writing(4), "late-plan-ok")
        .step(request(port))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { .. }));
}

#[tokio::test]
async fn a_relay_call_after_more_than_one_read_chunk_is_still_seen_when_the_cli_ends() {
    // Write ~4.8 MiB (> 4 MiB READ_CHUNK) before the relay call so finish() must drain multiple chunks.
    let port = FakePort::new(vec![]);
    let relay_call = json!({"type":"assistant","uuid":"r1","parentUuid":null,"message":{"id":"msg_r","role":"assistant","content":[{"type":"tool_use","id":"toolu_r","name":"mcp__relay__nothing","input":{}}]}}).to_string();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        use std::io::Write;
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        for n in 0..6 {
            let filler = json!({"type":"user","uuid":format!("u{n}"),"message":{"role":"user","content":"x".repeat(800 * 1024)}});
            writeln!(file, "{filler}").unwrap();
        }
        writeln!(file, "{relay_call}").unwrap();
    });
    let error = runner(&sandbox, "big-tail")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(error, UNSAVED);
    assert!(port.commits().is_empty());
}

#[tokio::test]
async fn a_record_too_large_to_read_to_the_end_closes_the_run() {
    let port = FakePort::new(vec![]);
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(|spec| {
        use std::io::Write;
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = std::fs::File::create(path).unwrap();
        // Sparse 80 MiB file exceeds the 16 * 4 MiB finish drain cap.
        file.set_len(80 << 20).unwrap();
        drop(file);
        let mut tail = std::fs::OpenOptions::new()
            .append(true)
            .open(session_file(spec))
            .unwrap();
        writeln!(tail).unwrap();
    });
    let error = runner(&sandbox, "huge-tail")
        .step(request(port))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_too_large",
            transient: false
        }
    );
}

#[tokio::test]
async fn the_final_check_obeys_the_total_time_and_the_cancel_signal() {
    for cancel in [false, true] {
        let port = FakePort::new(vec![]);
        port.confirm(probe_response());
        port.ended_hangs
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut req = request(port.clone());
        req.limits.total = Duration::from_millis(150);
        let (tx, rx) = watch::channel(false);
        req.cancel = rx;
        if cancel {
            req.limits.total = Duration::from_secs(10);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = tx.send(true);
            });
        }
        let started = std::time::Instant::now();
        let error = runner(
            &sandbox_writing(4),
            if cancel { "cancel-end" } else { "total-end" },
        )
        .step(req)
        .await
        .unwrap_err();
        assert_eq!(
            error,
            if cancel {
                ModelError::Cancelled
            } else {
                ModelError::Timeout
            }
        );
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "{cancel}: hung final check must respect deadline ({:?})",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn a_cli_that_only_uses_vm_built_ins_after_the_journal_froze_still_succeeds() {
    let port = FakePort::new(vec![]);
    let builtin = json!({"type":"assistant","uuid":"b1","parentUuid":null,"message":{"id":"msg_b","role":"assistant","content":[{"type":"tool_use","id":"toolu_b","name":"Read","input":{"file":"a"}}]}}).to_string();
    let sandbox = FakeSandbox::new([slow()]).inspect(move |spec| {
        use std::io::Write;
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        writeln!(file, "{}", oversized_line()).unwrap();
        writeln!(file, "{builtin}").unwrap();
    });
    let out = runner(&sandbox, "builtin")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { .. }));
}

#[tokio::test]
async fn a_relay_call_after_an_oversized_line_stops_the_run_for_good() {
    let port = FakePort::new(vec![]);
    let relay_call = json!({"type":"assistant","uuid":"r1","parentUuid":null,"message":{"id":"msg_r","role":"assistant","content":[{"type":"tool_use","id":"toolu_r","name":"mcp__relay__nothing","input":{}}]}}).to_string();
    let sandbox = FakeSandbox::new([slow().hangs()]).inspect(move |spec| {
        use std::io::Write;
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        writeln!(file, "{}", oversized_line()).unwrap();
        writeln!(file, "{relay_call}").unwrap();
    });
    let error = runner(&sandbox, "toolarge")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_too_large",
            transient: false
        }
    );
    assert_eq!(sandbox.killed(), 1);
}

#[tokio::test]
async fn the_last_record_without_a_trailing_newline_is_still_checked_and_a_truncated_one_is_not_a_clean_end()
 {
    let relay_call = json!({"type":"assistant","uuid":"r1","parentUuid":null,"message":{"id":"msg_r","role":"assistant","content":[{"type":"tool_use","id":"toolu_r","name":"mcp__relay__nothing","input":{}}]}}).to_string();
    let writing = |tail: String| {
        FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
            use std::io::Write;
            let path = session_file(spec);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut file = std::fs::File::create(path).unwrap();
            write!(file, "{tail}").unwrap();
        })
    };
    for (tag, tail) in [
        ("no-newline", relay_call.clone()),
        ("truncated", relay_call[..relay_call.len() / 2].to_owned()),
    ] {
        let port = FakePort::new(vec![]);
        let error = runner(&writing(tail), tag)
            .step(request(port.clone()))
            .await
            .unwrap_err();
        assert_eq!(error, UNSAVED, "{tag}");
        assert!(port.commits().is_empty(), "{tag}");
    }
    let port = FakePort::new(vec![]);
    let error = runner(&writing(format!("{relay_call}\n")), "newline")
        .step(request(port))
        .await
        .unwrap_err();
    assert_eq!(error, UNSAVED);
    let port = FakePort::new(vec![]);
    let out = runner(&writing("\n  \n".into()), "blank-end")
        .step(request(port))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { .. }));
}
