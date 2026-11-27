//! 로그에 원문이 남지 않는다: `BOT_LOG=trace`로 실행해도 Slack 웹소켓으로 받은 DM 원문과 Slack API로 보내는 본문이 로그에 없다. 전송 라이브러리(웹소켓·HTTP)는
//! `trace`에서 오가는 프레임·본문을 그대로 기록하므로(예: 웹소켓 라이브러리의 «Received message …») `bot_cli::logging`이 이 프로그램의 크레이트만 요청한 수준으로
//! 열고 나머지는 경고까지만 허용한다.
//! - 첫 시험은 이 프로세스에 전역 로그 구독자를 한 번 설치하고 serve를 같은 프로세스에서 돌려 출력을 모은다(가짜 Slack과 실제 웹소켓·HTTP를 쓴다).
//! - 둘째 시험은 실제 `bot` 프로세스를 `BOT_LOG=trace`로 띄워 `main`이 같은 설정을 쓰는지 본다(표준 오류).
//!
//! 실제 PG가 필요하다(없으면 건너뜀). claude는 필요 없다.

use crate::claude_support::*;
use crate::process_support::*;
use bot_cli::cli::Role;
use bot_gateway::{
    slack::api::{Body, SlackApi},
    testing::slack::{FakeSlack, dm_frame},
};
use serde_json::json;
use std::{
    io::{self, Read},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

/// 로그 출력을 메모리에 모은다.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

const PROFILE: &str = r#"version: 1
id: personal
timezone: UTC
model: assistant
roles:
  owner: [run]
members:
  - {identity: "slack:home/U1", roles: [owner]}
bindings:
  - {id: dm, adapter: slack, workspace: home, dm: true, agent: main}
secrets: []
policy:
  scopes: [run]
  approve: []
"#;

const INBOUND: &str = "받은-DM-원문-TOP-SECRET-7731";
const OUTBOUND: &str = "보내는-본문-TOP-SECRET-8842";
const CHAT: &str = "CLI-대화-원문-TOP-SECRET-9953";

/// 외부 전송·런타임·DB 라이브러리의 상세 로그(프레임·본문·SQL을 싣는 trace/debug/info)가 열렸는가: 이런 줄이 하나라도 있으면 그 이름을 돌려준다.
fn noisy_line(output: &str) -> Option<&'static str> {
    [
        "tungstenite",
        "hyper_util",
        "hyper::",
        "reqwest",
        "rustls",
        "h2::",
        "sqlx::",
        "tokio_util",
        "axum::serve",
        "mio::",
    ]
    .into_iter()
    .find(|noisy| {
        output.lines().any(|line| {
            line.contains(noisy)
                && ["TRACE", "DEBUG", "INFO"]
                    .iter()
                    .any(|level| line.contains(level))
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_logging_never_records_message_bodies_from_the_websocket_or_the_slack_api() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let captured = Capture::default();
    bot_cli::logging::init_to(Level::TRACE, captured.clone())
        .expect("전역 로그를 한 번만 설치해요");
    let slack = FakeSlack::start().await;
    let gateway = format!(
        r#"{}, "slack": {{"workspaces": {{"home": {{"team_id": "T1", "bot_token": "bot-token", "app_token": "app-token", "dm": true}}}}}}"#,
        cli_gateway()
    );
    let rig = Rig::start_slack(
        schema,
        PathBuf::from("/nonexistent/claude"),
        PROFILE,
        &gateway,
        &[Role::Gateway],
        &slack.base,
    )
    .await;
    assert!(slack.connected().await, "Slack 소켓에 붙지 않았어요");

    // 인바운드: 웹소켓으로 DM이 온다 → 접수·ack
    slack
        .push(dm_frame(
            "E1",
            "T1",
            "D1",
            "U1",
            "1700000001.000100",
            INBOUND,
        ))
        .await;
    assert!(
        slack.until(|s| s.acks().iter().any(|id| id == "E1")).await,
        "접수가 끝나지 않았어요"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs"),
        "1",
        "원문은 일반 접수로 들어갔어요(이 시험이 실제로 DM을 처리했다)"
    );

    // 아웃바운드: Slack API로 본문을 보낸다(HTTP 클라이언트의 요청 기록)
    let api = SlackApi::new(Some(&slack.base));
    let message = json!({"channel": "C1", "text": OUTBOUND});
    let _ = api
        .call("chat.postMessage", "xoxb-test-token", Body::Json(&message))
        .await;
    assert_eq!(slack.calls("chat.postMessage").len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.stop().await;

    let output = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    // 이 프로그램의 크레이트가 요청한 수준(trace)으로 로그를 남긴다 — 비어 있는 출력을 통과로 보지 않게
    assert!(
        output.contains("bot_cli::logging") && output.contains("로그를 시작했어요"),
        "로그가 비었어요:\n{output}"
    );
    for secret in [INBOUND, OUTBOUND, "xoxb-test-token", "xapp-test-token"] {
        assert!(!output.contains(secret), "로그에 {secret}가 남았어요");
    }
    assert_eq!(
        noisy_line(&output),
        None,
        "외부 라이브러리의 상세 로그가 열렸어요"
    );
}

/// 실제 `bot` 프로세스를 `BOT_LOG=trace`로 띄운 표준 오류: 이 프로그램의 로그만 나오고 CLI 대화 원문·외부 라이브러리의 상세 로그는 없다.
#[test]
fn a_real_bot_process_at_trace_keeps_third_party_traffic_and_message_bodies_out_of_stderr() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("l")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let env = env_for(&examples(), &schema.url(), &socket, &[("BOT_LOG", "trace")]);
    let mut child = Command::new(BOT)
        .args(["serve", "--role", "gateway"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("serve 시작");
    // 표준 오류를 계속 읽는다(trace가 많아 파이프가 차면 서버가 멈춘다)
    let mut pipe = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = pipe.read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    wait_for("게이트웨이 소켓이 열리지 않았어요", || {
        socket.exists()
    });
    wait_for("활성 판본이 게시되지 않았어요", || {
        !schema.active().is_empty()
    });
    // CLI 대화 원문이 게이트웨이(소켓)를 거쳐 접수된다
    let chat = bot(
        &refs(&env),
        &["chat", "--no-wait", "--request-id", "l1", CHAT],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    assert_eq!(schema.count("bot_runs"), "1");
    let _ = child.kill();
    let _ = child.wait();
    let stderr = reader.join().unwrap();
    let client = text(&chat);

    assert!(
        stderr.contains("bot_cli::logging") && stderr.contains("로그를 시작했어요"),
        "BOT_LOG=trace가 이 프로그램의 로그에 적용되지 않았어요:\n{stderr}"
    );
    for (who, output) in [("서버", &stderr), ("클라이언트", &client)] {
        assert!(!output.contains(CHAT), "{who}의 출력에 원문이 남았어요");
        assert_eq!(
            noisy_line(output),
            None,
            "{who}: 외부 라이브러리의 상세 로그가 열렸어요"
        );
    }
}
