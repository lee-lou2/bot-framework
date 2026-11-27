//! Canary test for the model VM adapter using Apple `container` CLI and local `alpine:3.20` (no firewall init).
//! Verifies readonly/scratch mounts, readonly rootfs, dropped capabilities, secret env handling, host-only isolation, `--rm` cleanup, and scoped `reap`.
//! Also demonstrates that without firewall init, other ports on the same gateway remain open (motivating `container_firewall.rs`).
//! Run: `BOT_CONTAINER_CANARY=required cargo test -p bot-runtime --test container_canary -- --nocapture`

mod vm;

use std::time::Duration;
use vm::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_real_vm_is_isolated_as_promised_and_reap_removes_only_this_attempts_vm() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program, "iso");
    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    let gateway = anchored.address();
    let (relay_port, relay_rx, _relay) = service(anchored.bind_addr(0));
    let (other_port, other_rx, _other) = service(anchored.bind_addr(0));
    let (loop_port, loop_rx, _loop) = service("127.0.0.1:0".parse().unwrap());
    let sandbox = anchored.relay(relay_port).await.expect("relay");
    // Drain the readiness probe connection made by `anchored.relay(...)`.
    while relay_rx.try_recv().is_ok() {}

    let script = format!(
        r#"
echo "input=$(cat /bot/in.txt)"
echo "stdin=$(head -n1)"
(touch /bot/x 2>/dev/null && echo ro-input=WRITABLE) || echo ro-input=readonly
echo "scratch=$(echo hi > /work/out.txt && cat /work/out.txt)"
(touch /etc/canary 2>/dev/null && echo rootfs=WRITABLE) || echo rootfs=readonly
echo "tmp=$(echo t > /tmp/t && cat /tmp/t)"
(chown 12345 /work/out.txt 2>/dev/null && echo caps=KEPT) || echo caps=dropped
echo "env_plain=$PLAIN_VALUE"
echo "secret_len=${{#CANARY_API_TOKEN}}"
echo "home=$HOME"
echo "relay=$BOT_RELAY_IP:$BOT_RELAY_PORT"
echo "url=$ANTHROPIC_BASE_URL"
(wget -T3 -qO- http://1.1.1.1 >/dev/null 2>&1 && echo internet=REACHABLE) || echo internet=unreachable
(ping -c1 -W2 1.1.1.1 >/dev/null 2>&1 && echo ping=REACHABLE) || echo ping=unreachable
(echo from-vm | nc -w3 {gateway} {relay_port} && echo relay=reached) || echo relay=UNREACHABLE
(echo other | nc -w3 {gateway} {other_port} && echo other-gateway-port=open) || echo other-gateway-port=closed
(echo x | nc -w3 {gateway} {loop_port} && echo loopback-service=REACHABLE) || echo loopback-service=unreachable
echo done
"#
    );
    let env_vars = vec![
        ("PLAIN_VALUE".to_owned(), "plain-1".to_owned()),
        ("CANARY_API_TOKEN".to_owned(), "token-secret-123".to_owned()),
        ("HOME".to_owned(), "/work".to_owned()),
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            format!("http://{gateway}:{relay_port}"),
        ),
    ];
    let mut spec = spec(&env.root, "isolation", &script, env_vars.clone());
    let mut vm = sandbox.spawn(spec.clone()).await.expect("VM");
    let out = lines(&mut vm).await;
    let exit = vm.wait().await;
    let text = out.join("\n");
    for expected in [
        "input=read-only input".to_owned(),
        "stdin=stdin body".into(),
        "ro-input=readonly".into(),
        "scratch=hi".into(),
        "rootfs=readonly".into(),
        "tmp=t".into(),
        "caps=dropped".into(),
        "env_plain=plain-1".into(),
        "secret_len=16".into(),
        "home=/work".into(),
        format!("relay={gateway}:{relay_port}"),
        format!("url=http://{gateway}:{relay_port}"),
        "internet=unreachable".into(),
        "ping=unreachable".into(),
        "relay=reached".into(),
        // Without firewall init, other ports on the gateway remain reachable (why production requires trusted init).
        "other-gateway-port=open".into(),
        "loopback-service=unreachable".into(),
        "done".into(),
    ] {
        assert!(out.contains(&expected), "{expected}\n{text}");
    }
    assert_eq!(exit.code, Some(0), "{text}");
    assert_eq!(
        relay_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "from-vm"
    );
    assert_eq!(
        other_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "other"
    );
    assert!(loop_rx.try_recv().is_err());
    assert_eq!(
        std::fs::read_to_string(env.root.join("work/out.txt"))
            .unwrap()
            .trim(),
        "hi"
    );
    assert!(!env.root.join("input/x").exists());

    // Mismatched relay URL in env must be rejected before spawning.
    spec.env[3].1 = "http://192.168.1.1:80".into();
    assert!(matches!(sandbox.spawn(spec).await, Err(Error::Invalid)));

    env.until(15, "only anchor should remain", |e| e.ids().len() == 1)
        .await;
    let owner = ContainerSandbox::owner_of(&Attempt {
        profile: ProfileId("canary".into()),
        run: RunId("other".into()),
        claim: Some(ClaimEpoch(1)),
        generation: Generation(1),
    });
    let mut other = sandbox
        .spawn(spec_sleep(&env, "other"))
        .await
        .expect("other attempt VM");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(90), other.next_line())
            .await
            .unwrap()
            .as_deref(),
        Some("up")
    );
    sandbox
        .reap("canary.nobody.g1.c1")
        .await
        .expect("unrelated owner");
    assert_eq!(env.ids().len(), 2, "expected anchor and attempt VM");
    sandbox.reap(&owner).await.expect("reap");
    env.until(15, "attempt VM should be removed", |e| e.ids().len() == 1)
        .await;
    drop(other);
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
    assert!(matches!(
        sandbox.spawn(spec_sleep(&env, "late")).await,
        Err(Error::Unavailable)
    ));
}

fn spec_sleep(env: &Env, run: &str) -> ModelSpec {
    let mut spec = spec(
        &env.root,
        run,
        "echo up; sleep 60",
        vec![("ANTHROPIC_BASE_URL".to_owned(), String::new())],
    );
    spec.env.clear();
    spec.stdin = None;
    spec
}
