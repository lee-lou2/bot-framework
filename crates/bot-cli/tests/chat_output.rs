//! `bot chat`이 터미널에 찍는 글(모델·도구가 만든 결과, 서버가 돌려준 오류 문구)에는 제어 문자가 섞일 수 있다 — ESC 시퀀스(OSC 52 클립보드 쓰기·창 제목·화면 지우기)·8비트 C1·BEL·CR.
//! 실제 `bot` 프로세스를 가짜 게이트웨이(유닉스 소켓)에 붙여, 출력 바이트에 제어 문자가 하나도 없고(줄바꿈·탭 제외) 읽을 수 있는 표기로 바뀌었는지 확인한다.
//! 저장소·모델은 필요 없다.
mod chat_support;
mod process_support;
use chat_support::*;
use process_support::*;
use serde_json::json;

const EVIL: &str = "안녕하세요\n\u{1b}]52;c;QUJDRA==\u{7}\u{1b}[2J\u{1b}[31m빨강\u{1b}[0m\u{9b}31m\u{9d}0;제목\u{7}\r덮어쓰기\r\n둘째 줄\t탭\u{8}\u{7f}\u{0}끝";

#[test]
fn model_output_with_terminal_control_sequences_is_shown_but_never_obeyed() {
    let dir = tempfile::Builder::new()
        .prefix("o")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![
            json!({"type": "accepted"}),
            json!({"type": "result", "state": "succeeded", "result": {"text": EVIL, "attachments": [{"name": "표\u{1b}[2J.txt"}]}}),
        ],
    );
    let envs = [("BOT_SOCKET", socket.to_str().unwrap())];
    let output = bot(&envs, &["chat", "--request-id", "o1", "안녕"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));

    // 출력 바이트(표준출력·표준오류 모두)에 터미널이 명령으로 읽을 것이 하나도 없다
    for (name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert_eq!(raw_controls(bytes), Vec::<String>::new(), "{name}");
    }
    let shown = String::from_utf8(output.stdout).unwrap();
    // 글은 그대로 보인다: 한글·줄바꿈·탭, 그리고 시퀀스는 읽을 수 있는 표기로(ESC가 없으니 `]52;…`는 평범한 글이다)
    for expected in [
        "안녕하세요\n",
        r"\u{1b}]52;c;QUJDRA==\u{7}",
        r"\u{1b}[2J",
        r"\u{1b}[31m빨강\u{1b}[0m",
        r"\u{9b}31m\u{9d}0;제목\u{7}",
        r"\u{d}덮어쓰기",
        "\n둘째 줄\t탭",
        r"\u{8}\u{7f}\u{0}끝",
        // 첨부 이름도 같다
        r"[첨부] 표\u{1b}[2J.txt",
    ] {
        assert!(shown.contains(expected), "{expected:?} ∉ {shown:?}");
    }
}

#[test]
fn an_error_message_from_the_server_is_cleaned_the_same_way() {
    let dir = tempfile::Builder::new()
        .prefix("o")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![
            json!({"type": "error", "code": "invalid", "message": "거절\u{1b}]0;pwn\u{7}했어요\u{9d}x\r되돌림"}),
        ],
    );
    let envs = [("BOT_SOCKET", socket.to_str().unwrap())];
    let output = bot(&envs, &["chat", "--request-id", "o2", "안녕"]);
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert_eq!(raw_controls(&output.stdout), Vec::<String>::new());
    assert_eq!(raw_controls(&output.stderr), Vec::<String>::new());
    let shown = String::from_utf8(output.stderr).unwrap();
    assert!(
        shown.contains(r"거절\u{1b}]0;pwn\u{7}했어요\u{9d}x\u{d}되돌림"),
        "{shown:?}"
    );
}

#[test]
fn reattaching_to_a_result_shows_it_the_same_safe_way() {
    let dir = tempfile::Builder::new()
        .prefix("o")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let server = fake_gateway(
        &socket,
        0o700,
        vec![
            json!({"type": "result", "state": "succeeded", "result": "복사\u{1b}]52;c;QUJD\u{7}됐어요"}),
        ],
    );
    let envs = [("BOT_SOCKET", socket.to_str().unwrap())];
    let output = bot(&envs, &["chat", "--attach", "--request-id", "o3"]);
    let seen = server.join().unwrap();
    assert!(
        seen.request.contains(r#""op":"result""#),
        "{}",
        seen.request
    );
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(raw_controls(&output.stdout), Vec::<String>::new());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains(r"복사\u{1b}]52;c;QUJD\u{7}됐어요")
    );
}
