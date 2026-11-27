//! Verifies `BOT_LOG=trace` never logs Slack DM payloads, outbound API bodies, CLI chat bodies, or third-party
//! transport traffic (`bot_cli::logging` restricts verbose levels to workspace crates and caps external crates at WARN).
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

const INBOUND: &str = "inbound-dm-body-TOP-SECRET-7731";
const OUTBOUND: &str = "outbound-body-TOP-SECRET-8842";
const CHAT: &str = "cli-chat-body-TOP-SECRET-9953";

/// Returns the first third-party transport/runtime/DB crate emitting TRACE, DEBUG, or INFO lines.
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
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let captured = Capture::default();
    bot_cli::logging::init_to(Level::TRACE, captured.clone()).expect("install global logger once");
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
    assert!(slack.connected().await, "gateway did not connect to Slack");

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
        "DM envelope was not acknowledged"
    );
    assert_eq!(
        rig.query("select count(*) from {s}.bot_runs"),
        "1",
        "inbound DM must be accepted as a run"
    );

    let api = SlackApi::new(Some(&slack.base));
    let message = json!({"channel": "C1", "text": OUTBOUND});
    let _ = api
        .call("chat.postMessage", "xoxb-test-token", Body::Json(&message))
        .await;
    assert_eq!(slack.calls("chat.postMessage").len(), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.stop().await;

    let output = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    // Confirm workspace crates actually emitted verbose logs so an empty buffer cannot pass vacuously.
    assert!(
        output.contains("bot_cli::logging") && output.contains("logging initialized"),
        "expected verbose bot_cli::logging output:\n{output}"
    );
    for secret in [INBOUND, OUTBOUND, "xoxb-test-token", "xapp-test-token"] {
        assert!(
            !output.contains(secret),
            "secret leaked into logs: {secret}"
        );
    }
    assert_eq!(
        noisy_line(&output),
        None,
        "third-party crate emitted verbose logs"
    );
}

#[test]
fn a_real_bot_process_at_trace_keeps_third_party_traffic_and_message_bodies_out_of_stderr() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
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
        .expect("spawn serve");
    // Drain stderr continuously so a full trace pipe cannot stall the server.
    let mut pipe = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = pipe.read_to_end(&mut text);
        String::from_utf8_lossy(&text).into_owned()
    });
    wait_for("gateway socket did not open", || socket.exists());
    wait_for("active revision was not published", || {
        !schema.active().is_empty()
    });
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
        stderr.contains("bot_cli::logging") && stderr.contains("logging initialized"),
        "BOT_LOG=trace did not apply to workspace crates:\n{stderr}"
    );
    for (who, output) in [("server", &stderr), ("client", &client)] {
        assert!(!output.contains(CHAT), "{who} leaked chat body into output");
        assert_eq!(
            noisy_line(output),
            None,
            "{who}: third-party crate emitted verbose logs"
        );
    }
}
