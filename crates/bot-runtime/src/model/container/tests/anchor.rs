//! 닻 VM: 라벨·최소 자원·망에서 확인한 주소·죽으면 다시 띄우기와 그동안의 거절. 진짜 VM은 `tests/container_anchor.rs`가 본다.

use super::*;
use std::process::Command as StdCommand;

/// 가짜 CLI에 `kill`을 직접 보낸다(닻 VM이 밖에서 죽은 상황).
fn kill_vm(fake: &Fake, name: &str) {
    let status = StdCommand::new(fake.dir.join("container"))
        .args(["kill", name])
        .status()
        .unwrap();
    assert!(status.success());
}

/// 가장 최근에 띄운 닻 VM의 이름. 가짜 CLI 프로세스가 뜨는 데 시간이 걸리므로 로그와 pid 파일이 생길 때까지 기다린다.
async fn anchor_name(fake: &Fake) -> String {
    for _ in 0..200 {
        let found = fake
            .argv("run")
            .into_iter()
            .rfind(|a| a.iter().any(|x| x.starts_with("bot.owner=anchor.")))
            .map(|run| run[run.iter().position(|a| a == "--name").unwrap() + 1].clone())
            .filter(|name| fake.dir.join(format!("pid-{name}")).exists());
        if let Some(name) = found {
            return name;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("닻 VM을 띄우지 않았다");
}

async fn until_health(
    health: &mut watch::Receiver<Health>,
    want: impl Fn(Health) -> bool,
) -> Health {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let now = *health.borrow_and_update();
            if want(now) {
                return now;
            }
            health.changed().await.unwrap();
        }
    })
    .await
    .expect("닻의 건강이 바뀌어야 한다")
}

#[tokio::test]
async fn an_anchor_is_labeled_minimal_and_returns_the_gateway_verified_from_the_network() {
    let fake = Fake::new("anchor-up");
    let anchored = start(&fake, fake.config()).await;
    assert_eq!(anchored.address(), Ipv4Addr::new(192, 168, 99, 1));
    assert_eq!(anchored.bind_addr(0).to_string(), "192.168.99.1:0");
    assert_eq!(*anchored.health().borrow(), Health::Up { generation: 1 });
    let name = anchor_name(&fake).await;
    let run = fake
        .argv("run")
        .into_iter()
        .find(|a| a.contains(&name))
        .expect("닻을 띄웠다");
    let at = |flag: &str| run.iter().position(|a| a == flag).unwrap();
    assert_eq!(run[at("--network") + 1], "botvm-net");
    for label in [
        "bot.managed=1",
        "bot.instance=botvm",
        "bot.owner=anchor.botvm.g0.c0",
    ] {
        assert!(run.contains(&label.to_owned()), "{label}: {run:?}");
    }
    assert!(run.iter().any(|a| a.starts_with("bot.incarnation=")));
    assert_eq!(
        (run[at("-c") + 1].as_str(), run[at("-m") + 1].as_str()),
        ("2", "2048M")
    );
    assert!(!run.contains(&"--mount".to_owned()) && !run.contains(&"-i".to_owned()));
    assert_eq!(run[run.len() - 3..], [PINNED, "sleep", "2147483647"]);
    // 릴레이를 열기 전에는 모델 VM이 없다. 연 뒤에는 닻이 살아 있는 동안 모델 VM을 띄운다
    let sandbox = anchored.relay(8787).await.expect("릴레이");
    assert_eq!(sandbox.address(), Ipv4Addr::new(192, 168, 99, 1));
    sandbox.spawn(fake.spec()).await.expect("모델 VM");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_gateway_that_is_not_a_private_ipv4_is_refused_before_any_vm_starts() {
    let fake = Fake::new("anchor-gateway");
    for bad in [
        "0.0.0.0",
        "127.0.0.1",
        "8.8.8.8",
        "169.254.1.1",
        "224.0.0.1",
        "255.255.255.255",
    ] {
        fake.set(json!({"networks": {"botvm-net": {"mode": "hostOnly", "gateway": bad}}}));
        let error = ContainerSandbox::start_probed(
            fake.config(),
            |_| true,
            |_| true,
            Duration::from_secs(1),
        )
        .await
        .err();
        assert_eq!(error, Some(StartError("gateway_invalid")), "{bad}");
    }
    assert!(fake.argv("run").is_empty(), "닻도 모델도 띄우지 않았다");
}

#[tokio::test]
async fn an_anchor_that_never_gets_its_address_or_exits_early_fails_and_leaves_nothing() {
    let fake = Fake::new("anchor-fail");
    // 주소가 호스트에 생기지 않는다
    let error = ContainerSandbox::start_probed(
        fake.config(),
        |_| false,
        |_| true,
        Duration::from_millis(600),
    )
    .await
    .err();
    assert_eq!(error, Some(StartError("anchor_not_ready")));
    let name = anchor_name(&fake).await;
    assert!(
        fake.read()["killed"]
            .as_array()
            .unwrap()
            .contains(&json!(name))
    );
    // 닻 VM이 주소가 생기기 전에 끝난다
    fake.set(json!({"anchor_fails": true, "killed": []}));
    let error =
        ContainerSandbox::start_probed(fake.config(), |_| false, |_| true, Duration::from_secs(20))
            .await
            .err();
    assert_eq!(error, Some(StartError("anchor_exited")));
    // 실패한 시작은 인스턴스 소유권을 쥐고 있지 않다
    fake.set(json!({"anchor_fails": false}));
    start(&fake, fake.config()).await.abort().await;
}

#[tokio::test]
async fn model_vms_are_refused_while_the_anchor_is_down_and_allowed_again_once_it_is_relaunched() {
    let fake = Fake::new("anchor-relaunch");
    let sandbox = up(&fake).await;
    let mut health = sandbox.health().expect("닻의 건강");
    // 다시 띄우지 못하는 상태에서 닻이 죽는다
    fake.set(json!({"anchor_fails": true}));
    kill_vm(&fake, &anchor_name(&fake).await);
    until_health(&mut health, |h| h == Health::Down).await;
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        *health.borrow(),
        Health::Down,
        "못 띄우는 동안은 계속 닫혀 있다"
    );
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    // 다시 띄울 수 있게 되면 닻이 새 세대로 돌아오고 모델 VM을 받는다
    fake.set(json!({"anchor_fails": false}));
    let up_again = until_health(&mut health, |h| matches!(h, Health::Up { .. })).await;
    assert_eq!(up_again, Health::Up { generation: 2 });
    sandbox.spawn(fake.spec()).await.expect("다시 연다");
    assert_eq!(
        sandbox.address(),
        Ipv4Addr::new(192, 168, 99, 1),
        "주소는 그대로다"
    );
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_gateway_address_that_vanishes_while_the_anchor_lives_closes_models_and_relaunches_the_anchor()
 {
    let fake = Fake::new("anchor-address");
    let hidden = fake.dir.join("address-hidden");
    let (seen, flag) = (address_while_anchored(&fake), hidden.clone());
    let anchored = ContainerSandbox::start_probed(
        fake.config(),
        move |address| seen(address) && !flag.exists(),
        |_| true,
        Duration::from_secs(5),
    )
    .await
    .expect("시작");
    let sandbox = anchored.relay(8787).await.expect("릴레이");
    let mut health = sandbox.health().unwrap();
    let first = anchor_name(&fake).await;
    sandbox
        .spawn(fake.spec())
        .await
        .expect("주소가 있는 동안은 연다");
    // 닻 프로세스는 살아 있는데 호스트의 게이트웨이 주소가 사라진다(vmnet이 망을 다시 만들었다): 닫고 닻을 다시 띄운다
    std::fs::write(&hidden, "").unwrap();
    until_health(&mut health, |h| h == Health::Down).await;
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        fake.read()["killed"]
            .as_array()
            .unwrap()
            .contains(&json!(first)),
        "주소를 잃은 닻은 끝낸다"
    );
    // 주소가 돌아오면 새 세대로 돌아와 다시 받는다
    std::fs::remove_file(&hidden).unwrap();
    let up_again = until_health(&mut health, |h| matches!(h, Health::Up { .. })).await;
    assert_eq!(up_again, Health::Up { generation: 2 });
    sandbox.spawn(fake.spec()).await.expect("다시 연다");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn every_spawn_itself_refuses_when_the_gateway_address_is_gone_even_before_the_watcher_notices()
 {
    // 감시 간격을 아주 길게 해서 감시가 아직 모르는 상태에서도 `spawn`이 직접 주소를 확인하는지 본다
    let fake = Fake::new("ready-address");
    let hidden = fake.dir.join("address-hidden");
    let (seen, flag) = (address_while_anchored(&fake), hidden.clone());
    let hooks = Hooks {
        host_has: Arc::new(move |address| seen(address) && !flag.exists()),
        relay_up: Arc::new(|_| true),
        ready_within: Duration::from_secs(5),
        reap_pause: Duration::from_millis(1),
        address_poll: Duration::from_secs(3600),
        ready_check: Duration::from_secs(5),
    };
    let anchored = ContainerSandbox::start_with(fake.config(), hooks)
        .await
        .expect("시작");
    let sandbox = anchored.relay(8787).await.expect("릴레이");
    sandbox
        .spawn(fake.spec())
        .await
        .expect("주소가 있는 동안은 연다");
    std::fs::write(&hidden, "").unwrap();
    assert_eq!(
        *sandbox.health().unwrap().borrow(),
        Health::Up { generation: 1 }
    );
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    std::fs::remove_file(&hidden).unwrap();
    sandbox
        .spawn(fake.spec())
        .await
        .expect("주소가 돌아오면 다시 연다");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_stuck_container_service_never_holds_a_spawn_beyond_the_readiness_window() {
    // `spawn`은 워커의 취소 신호 밖에서 기다린다: 컨테이너 서비스가 멈춰 점검이 끝나지 않아도 정해 둔 시간 안에 닫힌 채 실패해야 한다
    let fake = Fake::new("ready-stuck");
    let hooks = Hooks {
        host_has: Arc::new(address_while_anchored(&fake)),
        relay_up: Arc::new(|_| true),
        ready_within: Duration::from_secs(5),
        reap_pause: Duration::from_millis(1),
        address_poll: Duration::from_secs(3600),
        ready_check: Duration::from_millis(300),
    };
    let anchored = ContainerSandbox::start_with(fake.config(), hooks)
        .await
        .expect("시작");
    let sandbox = anchored.relay(8787).await.expect("릴레이");
    sandbox.spawn(fake.spec()).await.expect("멀쩡할 때는 연다");
    fake.set(json!({"inspect_hangs": true}));
    let started = std::time::Instant::now();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "점검이 멈춰도 {:?} 안에 끝난다",
        started.elapsed()
    );
    // 서비스가 돌아오면 다시 연다
    fake.set(json!({"inspect_hangs": false}));
    sandbox.spawn(fake.spec()).await.expect("다시 연다");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn dropping_the_handle_ends_the_anchor_and_closes_model_vms() {
    let fake = Fake::new("anchor-drop");
    let sandbox = up(&fake).await;
    let mut health = sandbox.health().unwrap();
    let name = anchor_name(&fake).await;
    // 소유권 핸들(`shutdown`)을 거치지 않고 어댑터를 그냥 놓는다
    let spec = fake.spec();
    drop(sandbox);
    until_health(&mut health, |h| h == Health::Down).await;
    for _ in 0..100 {
        if fake.read()["killed"]
            .as_array()
            .unwrap()
            .contains(&json!(name))
        {
            // 놓은 뒤에는 새로 시작해야 하고(소유권을 놓았다), 옛 어댑터로는 아무것도 못 띄운다
            let _ = spec;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("놓은 닻을 끝내지 않았다");
}
