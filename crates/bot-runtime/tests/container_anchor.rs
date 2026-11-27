//! Canary tests against the real Apple `container` CLI for anchor VM lifecycle, gateway binding, failover, and per-instance orphan cleanup (using unfirewalled `alpine:3.20`).
//! Run: `BOT_CONTAINER_CANARY=required cargo test -p bot-runtime --test container_anchor`

mod vm;

use std::{
    process::{Command, Stdio},
    time::Duration,
};
use vm::*;

async fn knock(sandbox: &ContainerSandbox, env: &Env, run: &str) -> Vec<String> {
    let script = "echo from-vm-$BOT_RELAY_PORT | nc -w10 $BOT_RELAY_IP $BOT_RELAY_PORT; echo rc=$?";
    let mut spec = spec(&env.root, run, script, Vec::new());
    spec.stdin = None;
    let mut vm = sandbox.spawn(spec).await.expect("model VM");
    let out = lines(&mut vm).await;
    vm.wait().await;
    out
}

async fn until_up(health: &mut tokio::sync::watch::Receiver<Health>, generation: u64) {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if matches!(*health.borrow_and_update(), Health::Up { generation: g } if g >= generation)
            {
                return;
            }
            health.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("anchor must reach generation {generation}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_anchor_the_gateway_cannot_be_bound_and_with_one_a_model_vm_reaches_the_front() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "anch1");
    // Without a running VM on the internal network, the gateway IP does not exist on the host.
    assert!(ok(
        &program,
        &[
            "network",
            "create",
            "--internal",
            "--label",
            "bot.managed=1",
            "--subnet",
            &env.subnet(),
            &env.network
        ]
    ));
    let inspected = Command::new(&program)
        .args(["network", "inspect", &env.network])
        .output()
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    let gateway: std::net::Ipv4Addr = json[0]["status"]["ipv4Gateway"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        std::net::TcpListener::bind((gateway, 0)).is_err(),
        "{gateway} should not be bindable without a running VM"
    );

    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    assert_eq!(anchored.address(), gateway);
    assert_eq!(env.ids().len(), 1, "expected one anchor: {:?}", env.ids());
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("relay");
    while received.try_recv().is_ok() {}
    let out = knock(&sandbox, &env, "first").await;
    assert!(out.iter().any(|l| l == "rc=0"), "{out:?}");
    assert_eq!(
        received.recv_timeout(Duration::from_secs(5)).unwrap(),
        format!("from-vm-{port}")
    );
    sandbox.shutdown().await;
    assert!(env.ids().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn killing_the_anchor_relaunches_it_and_the_listener_bound_before_still_works() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "anch2");
    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("relay");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();
    let before = running_anchor(&program, &env.instance).await;
    assert!(kill_eventually(&program, &before).await);
    until_up(&mut health, 2).await;
    let after = running_anchor(&program, &env.instance).await;
    assert_ne!(before, after);
    let out = knock(&sandbox, &env, "second").await;
    assert!(out.iter().any(|l| l == "rc=0"), "{out:?}");
    assert_eq!(
        received.recv_timeout(Duration::from_secs(5)).unwrap(),
        format!("from-vm-{port}")
    );
    sandbox.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn while_the_anchor_cannot_be_relaunched_model_vms_are_refused_and_they_run_again_after() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "anch3");
    let (wrapper, flag) = blockable(&env);
    let mut config = env.config();
    config.program = wrapper.to_str().unwrap().to_owned();
    let anchored = ContainerSandbox::start(config).await.expect("start");
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("relay");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();

    std::fs::write(&flag, "x").unwrap();
    let anchor = running_anchor(&program, &env.instance).await;
    assert!(kill_eventually(&program, &anchor).await);
    tokio::time::timeout(Duration::from_secs(60), async {
        while *health.borrow_and_update() != Health::Down {
            health.changed().await.unwrap();
        }
    })
    .await
    .expect("anchor must transition to Down");
    let mut refused = spec(&env.root, "refused", "echo should-not-run", Vec::new());
    refused.stdin = None;
    assert!(matches!(
        sandbox.spawn(refused.clone()).await,
        Err(Error::Unavailable)
    ));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(*health.borrow(), Health::Down);
    assert!(matches!(
        sandbox.spawn(refused).await,
        Err(Error::Unavailable)
    ));
    assert!(
        env.ids().is_empty(),
        "refused model VM must not spawn: {:?}",
        env.ids()
    );

    std::fs::remove_file(&flag).unwrap();
    until_up(&mut health, 2).await;
    let out = knock(&sandbox, &env, "again").await;
    assert!(out.iter().any(|l| l == "rc=0"), "{out:?}");
    sandbox.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vm_coming_and_going_on_the_default_network_never_leaves_the_front_unreachable() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "anch4");
    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    let gateway = anchored.address();
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("relay");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();
    // Churning VMs on the default NAT network can briefly drop vmnet host-only gateway addresses.
    for n in 0..3 {
        ok(
            &program,
            &[
                "run",
                "--rm",
                "--name",
                &format!("{}-nat{n}", env.instance),
                IMAGE,
                "true",
            ],
        );
    }
    let vanished = std::net::TcpListener::bind((gateway, 0)).is_err();
    let mut vm = None;
    for _ in 0..900 {
        let mut probe = spec(&env.root, "after-nat", "echo up", Vec::new());
        probe.stdin = None;
        match sandbox.spawn(probe).await {
            Ok(process) => {
                vm = Some(process);
                break;
            }
            Err(Error::Unavailable) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(other) => panic!("{other:?}"),
        }
    }
    let mut vm = vm.expect("sandbox must recover within 90s");
    let out = lines(&mut vm).await;
    vm.wait().await;
    assert!(out.iter().any(|l| l == "up"), "{out:?}");
    let out = knock(&sandbox, &env, "after-nat-front").await;
    assert!(out.iter().any(|l| l == "rc=0"), "{out:?}");
    assert_eq!(
        received.recv_timeout(Duration::from_secs(10)).unwrap(),
        format!("from-vm-{port}")
    );
    eprintln!(
        "address vanished: {vanished}, anchor generation now: {:?}",
        *health.borrow_and_update()
    );
    sandbox.shutdown().await;
}

fn detached(program: &std::path::Path, network: &str, name: &str, labels: &[&str]) {
    let mut command = Command::new(program);
    command.args([
        "run",
        "-d",
        "--rm",
        "--no-dns",
        "--network",
        network,
        "--name",
        name,
    ]);
    for label in labels {
        command.args(["--label", label]);
    }
    let status = command
        .args([IMAGE, "sleep", "600"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "{name}");
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_removes_only_this_instances_orphans_and_a_second_adapter_with_the_same_name_is_refused()
 {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "own1");
    // Another instance whose prefix extends this one runs on the same network.
    let other = Env::new(program.clone(), "own1b");
    let mut other_config = other.config();
    other_config.network = env.network.clone();
    let other_sandbox = ContainerSandbox::start(other_config)
        .await
        .expect("other instance");
    let orphan = format!("{}-aaaaaaaaaaaa-9", env.instance);
    let unlabeled = format!("{}-bbbbbbbbbbbb-9", env.instance);
    detached(
        &program,
        &env.network,
        &orphan,
        &[
            "bot.managed=1",
            &format!("bot.instance={}", env.instance),
            "bot.incarnation=previous-run",
            "bot.owner=canary.old.g1.c1",
        ],
    );
    detached(&program, &env.network, &unlabeled, &["bot.managed=1"]);
    assert_eq!(env.ids().len(), 2);
    let anchored = ContainerSandbox::start(env.config()).await.expect("start");
    let ids = env.ids();
    assert!(!ids.contains(&orphan), "{ids:?}");
    assert!(
        ids.contains(&unlabeled),
        "unowned VM must be preserved: {ids:?}"
    );
    assert_eq!(
        other.ids().len(),
        1,
        "other instance anchor must be preserved: {:?}",
        other.ids()
    );
    // Wait for the newly spawned anchor to appear in `ls` before snapshotting `before`.
    env.until(30, "anchor must appear in ls", |e| e.ids().len() == 2)
        .await;
    let before = env.ids();
    let second = ContainerSandbox::start(env.config()).await.err();
    assert_eq!(second, Some(StartError("instance_in_use")));
    assert_eq!(env.ids(), before);
    // Instance lock is keyed by instance name + host user, not mount root.
    let other_root = env.root.join("other-root");
    std::fs::create_dir_all(other_root.join("input")).unwrap();
    std::fs::create_dir_all(other_root.join("work")).unwrap();
    let mut elsewhere = env.config();
    elsewhere.mount_root = other_root;
    let third = ContainerSandbox::start(elsewhere).await.err();
    assert_eq!(third, Some(StartError("instance_in_use")));
    assert_eq!(env.ids(), before);
    anchored.abort().await;
    assert_eq!(
        env.ids(),
        std::slice::from_ref(&unlabeled),
        "abort removes only the anchor and keeps unowned VMs"
    );
    other_sandbox.abort().await;
    assert!(other.ids().is_empty());
    ok(&program, &["kill", &unlabeled]);
}
