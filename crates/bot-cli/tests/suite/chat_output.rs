//! Verifies `bot chat` neutralizes terminal control sequences (OSC 52, CSI, C1, BEL, CR, DEL, NUL) in model results
//! and server errors so stdout/stderr emit zero raw control bytes other than `\n` and `\t`.
use crate::chat_support::*;
use crate::process_support::*;
use serde_json::json;

const EVIL: &str = "hello ★\n\u{1b}]52;c;QUJDRA==\u{7}\u{1b}[2J\u{1b}[31mred\u{1b}[0m\u{9b}31m\u{9d}0;title\u{7}\roverwrite\r\nline two\ttab\u{8}\u{7f}\u{0}end";

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
            json!({"type": "result", "state": "succeeded", "result": {"text": EVIL, "attachments": [{"name": "report★\u{1b}[2J.txt"}]}}),
        ],
    );
    let envs = [("BOT_SOCKET", socket.to_str().unwrap())];
    let output = bot(&envs, &["chat", "--request-id", "o1", "hello"]);
    server.join().unwrap();
    assert!(output.status.success(), "{}", text(&output));

    for (name, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert_eq!(raw_controls(bytes), Vec::<String>::new(), "{name}");
    }
    let shown = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "hello ★\n",
        r"\u{1b}]52;c;QUJDRA==\u{7}",
        r"\u{1b}[2J",
        r"\u{1b}[31mred\u{1b}[0m",
        r"\u{9b}31m\u{9d}0;title\u{7}",
        r"\u{d}overwrite",
        "\nline two\ttab",
        r"\u{8}\u{7f}\u{0}end",
        r"[attachment] report★\u{1b}[2J.txt",
    ] {
        assert!(shown.contains(expected), "{expected:?} not in {shown:?}");
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
            json!({"type": "error", "code": "invalid", "message": "rejected\u{1b}]0;pwn\u{7}now\u{9d}x\rrewind"}),
        ],
    );
    let envs = [("BOT_SOCKET", socket.to_str().unwrap())];
    let output = bot(&envs, &["chat", "--request-id", "o2", "hello"]);
    server.join().unwrap();
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert_eq!(raw_controls(&output.stdout), Vec::<String>::new());
    assert_eq!(raw_controls(&output.stderr), Vec::<String>::new());
    let shown = String::from_utf8(output.stderr).unwrap();
    assert!(
        shown.contains(r"rejected\u{1b}]0;pwn\u{7}now\u{9d}x\u{d}rewind"),
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
            json!({"type": "result", "state": "succeeded", "result": "copied\u{1b}]52;c;QUJD\u{7}done"}),
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
            .contains(r"copied\u{1b}]52;c;QUJD\u{7}done")
    );
}
