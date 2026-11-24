//! Claude Code 세션 종료 판정: 저장하지 못한 도구 호출·끝까지 읽기·마지막 확인의 마감. CLI가 끝나도 실행되지 못한 호출이 있으면 성공으로 올리지 않는다.

use super::*;

#[tokio::test]
async fn a_placeholder_result_stops_the_cli_and_fails_the_attempt_instead_of_letting_it_finish() {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    // 도구 결과 줄(5번째)이 가리키는 호출이 원장에서 끝나지 않았다: 이 묶음은 쓰지 못하고, 그 호출은 실행되지 못한다
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
        "재개하면 호스트가 그 자리를 채우므로 다시 시도한다"
    );
    assert_eq!(sandbox.killed(), 1, "더 도구를 쓰지 못하는 CLI는 끝낸다");
    assert!(
        port.commits().is_empty(),
        "자리표시 결과를 담은 묶음은 쓰지 않는다"
    );
}

#[tokio::test]
async fn a_cli_that_ends_while_a_relay_call_could_not_be_saved_does_not_succeed() {
    // 프런트가 확인하지 못한(그룹이 저장되지 않은) 릴레이 호출이 있는데 CLI가 답을 내고 끝났다: 그 호출은 실행되지 못했다
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

/// 체크포인트 한 건에 들어가지 않는 대화 줄(상한을 넘는다)
fn oversized_line() -> String {
    json!({"type":"user","uuid":"big","message":{"role":"user","content":"x".repeat(crate::model::session::MAX_LINE_BYTES)}}).to_string()
}

const UNSAVED: ModelError = ModelError::Failed {
    code: "session_unsaved",
    transient: true,
};

#[tokio::test]
async fn a_saved_plan_whose_call_never_ran_does_not_succeed_even_when_the_plan_landed_late() {
    // 계획은 (늦게라도) 저장됐지만 프런트가 기다리다 실패시켜 호출은 실행되지 못했다: 원장에 끝난 기록이 없다
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    port.never_ran.lock().unwrap().insert((3, 0));
    let sandbox = sandbox_writing(4);
    let error = runner(&sandbox, "late-plan")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(error, UNSAVED);
    assert_eq!(port.commits().len(), 1, "계획은 저장됐다");
    // 같은 상황에서 호출이 원장에서 끝났으면 성공이다
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
    // 정상 줄 약 4.8 MiB 뒤에 릴레이 호출이 있고 CLI는 곧바로 끝난다: 첫 구간(4 MiB)만 읽으면 그 호출을 놓친다
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
        // 구간 상한(16×4 MiB)보다 큰 파일(희소 파일이라 디스크는 거의 쓰지 않는다): 끝까지 볼 수 없다
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
            "{cancel}: 응답하지 않는 마지막 확인이 마감을 넘기면 안 된다 ({:?})",
            started.elapsed()
        );
    }
}

#[tokio::test]
async fn a_cli_that_only_uses_vm_built_ins_after_the_journal_froze_still_succeeds() {
    // 얼린 뒤의 호출이 VM 내장 도구뿐이면 프런트가 막은 것이 없다
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
    // 상한을 넘은 줄 뒤에 릴레이 도구를 부르면 그 호출은 저장되지 못한다. 다시 해도 같으므로 다시 시도하지 않는다
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
    // 개행 없이 끝난 마지막 기록이 릴레이 호출이고 저장되지 못했다: 성공으로 끝내지 않는다
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
    // 같은 기록이 개행으로 끝나도 같다. 공백만 남은 끝은 정상이다
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
