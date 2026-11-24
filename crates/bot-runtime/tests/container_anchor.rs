//! 진짜 Apple `container`로 확인하는 닻 VM과 시작·소유: 닻이 있어야 게이트웨이 주소에 바인드할 수 있고, 모델 VM이 그 프런트에 닿으며, 닻이 죽으면 다시 뜨거나
//! (못 뜨는 동안) 모델 VM을 거절하고, 시작이 이 인스턴스의 고아만 치우고 다른 인스턴스·남의 VM은 그대로 둔다. 방화벽 init이 없는 이미지(`alpine:3.20`)다.
//! `BOT_CONTAINER_CANARY=required`이면 환경이 없을 때 실패한다. 실행: `BOT_CONTAINER_CANARY=required cargo test -p bot-runtime --test container_anchor`

mod vm;

use std::{
    process::{Command, Stdio},
    time::Duration,
};
use vm::*;

/// 모델 VM 하나를 띄워 호스트의 릴레이에 한 줄을 보내게 하고 그 출력을 돌려준다.
async fn knock(sandbox: &ContainerSandbox, env: &Env, run: &str) -> Vec<String> {
    let script = "echo from-vm-$BOT_RELAY_PORT | nc -w10 $BOT_RELAY_IP $BOT_RELAY_PORT; echo rc=$?";
    let mut spec = spec(&env.root, run, script, Vec::new());
    spec.stdin = None;
    let mut vm = sandbox.spawn(spec).await.expect("모델 VM");
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
    .unwrap_or_else(|_| panic!("닻이 세대 {generation}으로 돌아와야 한다"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_an_anchor_the_gateway_cannot_be_bound_and_with_one_a_model_vm_reaches_the_front() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program.clone(), "anch1");
    // 망만 있고 VM이 없으면 게이트웨이 주소는 호스트에 없다(그래서 닻이 필요하다)
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
        "VM이 없으면 {gateway}는 호스트에 없다"
    );

    // 시작이 닻을 세운다: 주소는 설정이 아니라 망에서 확인한 값이고, 이제 바인드할 수 있다
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    assert_eq!(anchored.address(), gateway);
    assert_eq!(env.ids().len(), 1, "닻 하나: {:?}", env.ids());
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("릴레이");
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
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("릴레이");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();
    let before = running_anchor(&program, &env.instance).await;
    // 닻이 밖에서 죽는다: 새 세대로 돌아온다(그동안 모델 VM이 없어 인터페이스가 사라질 수 있다)
    assert!(kill_eventually(&program, &before).await);
    until_up(&mut health, 2).await;
    let after = running_anchor(&program, &env.instance).await;
    assert_ne!(before, after);
    // 닻이 다시 뜬 뒤에도 먼저 바인드한 같은 리스너로 모델 VM이 닿는다(프런트를 다시 열 필요가 없다)
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
    let anchored = ContainerSandbox::start(config).await.expect("시작");
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("릴레이");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();
    // 다시 띄우지 못하게 막고 닻을 죽인다
    std::fs::write(&flag, "x").unwrap();
    let anchor = running_anchor(&program, &env.instance).await;
    assert!(kill_eventually(&program, &anchor).await);
    tokio::time::timeout(Duration::from_secs(60), async {
        while *health.borrow_and_update() != Health::Down {
            health.changed().await.unwrap();
        }
    })
    .await
    .expect("닻이 내려가야 한다");
    let mut refused = spec(&env.root, "refused", "echo should-not-run", Vec::new());
    refused.stdin = None;
    assert!(matches!(
        sandbox.spawn(refused.clone()).await,
        Err(Error::Unavailable)
    ));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        *health.borrow(),
        Health::Down,
        "못 띄우는 동안은 계속 닫혀 있다"
    );
    assert!(matches!(
        sandbox.spawn(refused).await,
        Err(Error::Unavailable)
    ));
    assert!(
        env.ids().is_empty(),
        "거절한 모델 VM은 띄워지지 않았다: {:?}",
        env.ids()
    );
    // 다시 띄울 수 있으면 돌아오고 모델 VM을 받는다
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
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    let gateway = anchored.address();
    let (port, received, _front) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(port).await.expect("릴레이");
    while received.try_recv().is_ok() {}
    let mut health = sandbox.health().unwrap();
    // 같은 기계의 다른 프로젝트가 하는 일: 기본(NAT) 망에 VM이 떴다 내려간다. vmnet이 호스트 전용 망의 호스트 쪽 주소를 잠시 지울 수 있다.
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
    // 주소가 사라졌다면 어댑터는 모델 VM을 거절하고(닫힌 채 실패) 닻을 다시 띄워 되살린다. 어느 쪽이든 끝에는 모델 VM이 프런트에 닿는다
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
    let mut vm = vm.expect("90초 안에 다시 열려야 한다");
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

/// 라벨·이름을 지정해 오래 사는 VM 하나를 띄운다(고아·남의 VM을 흉내 낸다).
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
    // 같은 망에서 다른 인스턴스(접두사가 이쪽을 포함한다)가 이미 돌고 있다
    let other = Env::new(program.clone(), "own1b");
    let mut other_config = other.config();
    other_config.network = env.network.clone();
    let other_sandbox = ContainerSandbox::start(other_config)
        .await
        .expect("다른 인스턴스");
    // 이 인스턴스의 이전 실행이 남긴 고아와, 소유를 확인할 수 없는 VM들
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
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    // 고아는 치웠고, 확인할 수 없는 것과 다른 인스턴스의 VM은 그대로다
    let ids = env.ids();
    assert!(!ids.contains(&orphan), "{ids:?}");
    assert!(
        ids.contains(&unlabeled),
        "소유를 확인할 수 없으면 보존한다: {ids:?}"
    );
    assert_eq!(
        other.ids().len(),
        1,
        "다른 인스턴스의 닻은 그대로다: {:?}",
        other.ids()
    );
    // 같은 이름의 둘째 어댑터는 시작 때 거절되고 아무것도 건드리지 않는다.
    // (막 뜬 닻은 목록에 늦게 보일 수 있다: 보일 때까지 기다린 뒤의 목록과 견준다)
    env.until(30, "닻이 목록에 보인다", |e| e.ids().len() == 2)
        .await;
    let before = env.ids();
    let second = ContainerSandbox::start(env.config()).await.err();
    assert_eq!(second, Some(StartError("instance_in_use")));
    assert_eq!(env.ids(), before);
    // 같은 인스턴스 이름에 작업 폴더만 다른 설정도 거절된다: 잠금은 작업 폴더가 아니라 이름과 호스트 사용자로 정해진다. 첫 어댑터의 살아 있는 닻은 그대로다
    let other_root = env.root.join("other-root");
    std::fs::create_dir_all(other_root.join("input")).unwrap();
    std::fs::create_dir_all(other_root.join("work")).unwrap();
    let mut elsewhere = env.config();
    elsewhere.mount_root = other_root;
    let third = ContainerSandbox::start(elsewhere).await.err();
    assert_eq!(third, Some(StartError("instance_in_use")));
    assert_eq!(env.ids(), before, "첫 어댑터의 닻을 지우지 않았다");
    anchored.abort().await;
    assert_eq!(
        env.ids(),
        std::slice::from_ref(&unlabeled),
        "닻만 치우고 확인할 수 없는 VM은 남긴다"
    );
    other_sandbox.abort().await;
    assert!(other.ids().is_empty());
    ok(&program, &["kill", &unlabeled]);
}
