//! End-to-end canary running the real `claude` binary inside the firewalled model VM image against a real broker front and PG (fake upstream).
//! Inspects the live VM via `container exec` mid-stream to verify unprivileged execution (uid 10001, zero caps, NoNewPrivs) and egress restricted solely to the relay port.
//! Run: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=... BOT_TEST_DATABASE_URL=... cargo test -p bot-runtime --test container_claude -- --nocapture`

mod support;
mod vm;

use bot_broker::testing::{End, FakeUpstream};
use bot_core::sandbox::ModelSandbox;
use bot_runtime::{Outcome, model::container::ContainerSandbox};
use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{cli::*, view::*, *};

const TRUSTED: &str = "TRUSTED-INSTRUCTION-4410 Reply briefly in English";

fn system_text(body: &serde_json::Value) -> String {
    match &body["system"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Probes `claude` process credentials and parallel TCP reachability inside the live VM.
const INSIDE: &str = r#"B=/bin/busybox
for p in /proc/[0-9]*; do
  case "$($B cat "$p/comm" 2>/dev/null)" in
    claude) $B grep -E '^(Uid|Gid|Groups|CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' "$p/status" | $B tr -s '\t ' ' ' | $B sed 's/: */=/; s/ $//; s/^/claude./' ;;
  esac
done
try() {
  s=$($B date +%s)
  if echo "probe-$1" | $B nc -w 3 "$2" "$3" >/dev/null 2>&1; then echo "$1=connected"
  else
    d=$(( $($B date +%s) - s ))
    if [ "$d" -ge 2 ]; then echo "$1=dropped"; else echo "$1=refused"; fi
  fi
}
try relay @GATEWAY@ @RELAY@ >/tmp/p1 &
try other-port @GATEWAY@ @OTHER@ >/tmp/p2 &
try external-443 1.1.1.1 443 >/tmp/p3 &
try external-dns 8.8.8.8 53 >/tmp/p4 &
try wildcard @GATEWAY@ @WILD@ >/tmp/p5 &
wait
$B cat /tmp/p1 /tmp/p2 /tmp/p3 /tmp/p4 /tmp/p5
echo inspected
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_real_claude_in_the_vm_reaches_only_the_front_and_runs_unprivileged() {
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
    let env = vm::Env::new(program.clone(), "claude");
    let mut config = env.firewalled(&image);
    config.cpus = 2;
    config.memory_mib = 2048;
    let anchored = ContainerSandbox::start(config).await.expect("start");
    let gateway = anchored.address();
    let (other_port, other_rx, _other) = vm::service(anchored.bind_addr(0));
    let (wild_port, wild_rx, _wild) = vm::service("0.0.0.0:0".parse().unwrap());

    // Stream the second response slowly (3s per chunk) so `inspect` can exec into the live VM.
    let upstream = Arc::new(FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_vm1", "toolu_vm1", "mcp__relay__agenda_list", "{}"),
    );
    let last = final_turn("msg_vm2", "Running well inside the VM");
    let refs: Vec<&[u8]> = last.iter().map(Vec::as_slice).collect();
    upstream.respond_slowly(200, &refs, Duration::from_secs(3), End::Eof);
    let mut snapshot = (*profile()).clone();
    snapshot
        .agents
        .get_mut(&bot_core::id::AgentId("main".into()))
        .expect("root agent")
        .instructions = TRUSTED.into();
    w.catalog.publish(Arc::new(snapshot));
    let run = w.accept("req-vm", "Show today's agenda").await;

    let sandbox_slot = Mutex::new(None);
    let host = host_at(
        &w,
        w.pg.peer("worker-vm").await,
        upstream.clone(),
        anchored.bind_addr(0),
        async |front| {
            let sandbox = Arc::new(anchored.relay(front.port()).await.expect("relay"));
            *sandbox_slot.lock().unwrap() = Some(sandbox.clone());
            sandbox as Arc<ModelSandbox>
        },
        &env.root.join("ws"),
        w.broker.clone(),
    )
    .await;
    let relay_port = host.front.addr().port();
    assert_eq!(host.front.addr().ip(), std::net::IpAddr::from(gateway));
    let sandbox = sandbox_slot.lock().unwrap().take().expect("sandbox");

    let inside = INSIDE
        .replace("@GATEWAY@", &gateway.to_string())
        .replace("@RELAY@", &relay_port.to_string())
        .replace("@OTHER@", &other_port.to_string())
        .replace("@WILD@", &wild_port.to_string());
    let inspect = async {
        let anchor = vm::running_anchor(&program, &env.instance).await;
        let mut vm_id = None;
        for _ in 0..1200 {
            vm_id = vm::ls_ids(&program, &format!("{}-", env.instance))
                .into_iter()
                .find(|id| *id != anchor);
            if vm_id.is_some() && upstream.requests().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let vm_id = vm_id.expect("model VM must start");
        assert!(upstream.requests().len() >= 2, "expected second request");
        let (program, inside) = (program.clone(), inside.clone());
        tokio::task::spawn_blocking(move || {
            let out = Command::new(&program)
                .args(["exec", &vm_id, "/bin/busybox", "sh", "-c", &inside])
                .output()
                .expect("container exec");
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
        .await
        .unwrap()
    };
    let (outcome, report) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(150), host.worker.run_once()),
        inspect
    );
    eprintln!("{report}");
    let outcome = outcome.expect("run must complete");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    w.pg.assert_state(&run, "succeeded").await;
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"Running well inside the VM"}"#
    );

    let expect = |line: &str| {
        assert!(
            report.lines().any(|l| l == line),
            "{line}\n--- exec output\n{report}"
        )
    };
    expect("claude.Uid=10001 10001 10001 10001");
    expect("claude.Gid=10001 10001 10001 10001");
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        expect(&format!("claude.{set}=0000000000000000"));
    }
    expect("claude.NoNewPrivs=1");
    expect("relay=connected");
    expect("other-port=dropped");
    expect("wildcard=dropped");
    assert!(
        !report.contains("external-443=connected") && !report.contains("external-dns=connected"),
        "{report}"
    );
    expect("inspected");
    assert!(
        other_rx.try_recv().is_err(),
        "other host port received data"
    );
    assert!(wild_rx.try_recv().is_err(), "0.0.0.0 service received data");

    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    assert!(result_text(&seen[1], "toolu_vm1").is_some());
    for body in &seen {
        let system = system_text(body);
        assert!(
            system.contains(TRUSTED),
            "instructions missing from VM model request: {system}"
        );
    }
    assert_eq!(w.runner.jobs().len(), 1, "{:?}", w.runner.jobs());
    let keys = step_keys(&w, &run).await;
    assert!(keys.iter().any(|k| k.starts_with("cli-rec-")), "{keys:?}");

    env.until(20, "only anchor should remain", |e| e.ids().len() == 1)
        .await;
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}
