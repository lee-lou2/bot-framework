//! Canary test running the real `claude` CLI (builtin Read enabled) inside the firewalled model VM to confirm the session token cannot be read from any file, `/proc`, or mounted directory.
//! Run: `BOT_CONTAINER_CANARY=required BOT_CLAUDE_CANARY=required BOT_MODEL_IMAGE=... BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test container_credentials`

mod support;
mod vm;

use bot_broker::testing::FakeUpstream;
use bot_core::{id::*, profile::Builtin, sandbox::ModelSandbox};
use bot_runtime::{Outcome, model::container::ContainerSandbox};
use std::{sync::Arc, time::Duration};
use support::{cli::*, sweep::Sweeping, view::*, *};

/// Relative paths resolve against `/bot`; the remaining paths probe `/work`, `/proc`, and `/etc`.
const READS: [(&str, &str, &str); 8] = [
    ("toolu_in", "Read", r#"{"file_path":"AGENTS.md"}"#),
    (
        "toolu_cfg",
        "Read",
        r#"{"file_path":"/work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cfg2",
        "Read",
        r#"{"file_path":"../work/mcp.claude.json"}"#,
    ),
    (
        "toolu_cli",
        "Read",
        r#"{"file_path":"/work/claude/.claude.json"}"#,
    ),
    ("toolu_env", "Read", r#"{"file_path":"/proc/self/environ"}"#),
    ("toolu_env1", "Read", r#"{"file_path":"/proc/1/environ"}"#),
    ("toolu_pass", "Read", r#"{"file_path":"/etc/passwd"}"#),
    ("toolu_mcp", "mcp__relay__agenda_list", "{}"),
];

#[tokio::test(flavor = "multi_thread")]
async fn the_real_claude_in_the_vm_cannot_read_the_session_token_from_any_file() {
    let Some(w) = world().await else {
        return;
    };
    let Some(program) = vm::available() else {
        return;
    };
    let _serial = vm::serial().await;
    let Some(image) = vm::model_image(&program) else {
        return;
    };
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&AgentId("main".into()))
        .expect("root agent")
        .builtin = [Builtin::Read].into();
    w.catalog.publish(Arc::new(snapshot));

    let env = vm::Env::new(program.clone(), "cred");
    let mut config = env.firewalled(&image);
    config.cpus = 2;
    config.memory_mib = 2048;
    let anchored = ContainerSandbox::start(config).await.expect("start");

    let upstream = Arc::new(FakeUpstream::new());
    script(&upstream, tools_turn("msg_vc1", &READS));
    script(&upstream, final_turn("msg_vc2", "Read complete"));
    let run = w.accept("req-vm-credentials", "Read the files").await;

    let sandbox = Arc::new(std::sync::Mutex::new(None));
    let slot = sandbox.clone();
    let bind = anchored.bind_addr(0);
    let host = host_at(
        &w,
        w.pg.peer("worker-cred").await,
        upstream.clone(),
        bind,
        async move |front| {
            let relay = anchored.relay(front.port()).await.expect("relay");
            let sweeping = Arc::new(Sweeping::new(relay));
            *slot.lock().unwrap() = Some(sweeping.clone());
            sweeping as Arc<ModelSandbox>
        },
        &env.root.join("ws"),
        w.broker.clone(),
    )
    .await;
    let sandbox = sandbox.lock().unwrap().take().expect("sandbox");

    let outcome = tokio::time::timeout(Duration::from_secs(150), host.worker.run_once())
        .await
        .expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    w.pg.assert_state(&run, "succeeded").await;

    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    let result = |id: &str| result_text(&seen[1], id).unwrap_or_else(|| panic!("missing {id}"));
    let mcp = result("toolu_mcp");
    assert!(
        mcp.contains("Walk"),
        "relay tool must return real data: {mcp}"
    );
    assert_eq!(w.runner.jobs().len(), 1);

    let agents = result("toolu_in");
    assert_eq!(result_error(&seen[1], "toolu_in"), Some(false), "{agents}");
    assert!(agents.starts_with("1\t"), "{agents}");
    for (id, why) in [
        ("toolu_cfg", "requested permissions to read"),
        ("toolu_cfg2", "requested permissions to read"),
        ("toolu_cli", "requested permissions to read"),
        ("toolu_pass", "requested permissions to read"),
        ("toolu_env", "device or process file"),
        ("toolu_env1", "device or process file"),
    ] {
        assert_eq!(result_error(&seen[1], id), Some(true), "{id}");
        assert!(result(id).contains(why), "{id}: {}", result(id));
    }

    let tokens = host.tokens();
    assert!(!tokens.is_empty());
    for body in &seen {
        let text = body.to_string();
        for token in &tokens {
            assert!(
                !text.contains(token.as_str()),
                "token leaked into model request"
            );
        }
    }
    let swept = sandbox.swept();
    assert_eq!(swept.len(), 1, "{swept:?}");
    assert!(swept[0].token_len >= 16, "{swept:?}");
    assert!(swept[0].files >= 3, "{swept:?}");
    assert!(
        swept[0].hits.is_empty(),
        "session token found in mounted directory: {:?}",
        swept[0].hits
    );

    env.until(20, "only anchor should remain", |e| e.ids().len() == 1)
        .await;
    sandbox.inner().shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}
