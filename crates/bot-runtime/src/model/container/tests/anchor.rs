//! Anchor VM lifecycle tests: gateway verification, failure cleanup, relaunch, and readiness checks.

use super::*;
use std::process::Command as StdCommand;

fn kill_vm(fake: &Fake, name: &str) {
    let status = StdCommand::new(fake.dir.join("container"))
        .args(["kill", name])
        .status()
        .unwrap();
    assert!(status.success());
}

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
    panic!("anchor VM was never started");
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
    .expect("anchor health should change")
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
        .expect("anchor launched");
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
    let sandbox = anchored.relay(8787).await.expect("relay");
    assert_eq!(sandbox.address(), Ipv4Addr::new(192, 168, 99, 1));
    sandbox.spawn(fake.spec()).await.expect("model VM");
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
    assert!(fake.argv("run").is_empty(), "no VM should have started");
}

#[tokio::test]
async fn an_anchor_that_never_gets_its_address_or_exits_early_fails_and_leaves_nothing() {
    let fake = Fake::new("anchor-fail");
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
    fake.set(json!({"anchor_fails": true, "killed": []}));
    let error =
        ContainerSandbox::start_probed(fake.config(), |_| false, |_| true, Duration::from_secs(20))
            .await
            .err();
    assert_eq!(error, Some(StartError("anchor_exited")));
    fake.set(json!({"anchor_fails": false}));
    start(&fake, fake.config()).await.abort().await;
}

#[tokio::test]
async fn model_vms_are_refused_while_the_anchor_is_down_and_allowed_again_once_it_is_relaunched() {
    let fake = Fake::new("anchor-relaunch");
    let sandbox = up(&fake).await;
    let mut health = sandbox.health().expect("anchor health");
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
        "must remain closed while relaunch fails"
    );
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    fake.set(json!({"anchor_fails": false}));
    let up_again = until_health(&mut health, |h| matches!(h, Health::Up { .. })).await;
    assert_eq!(up_again, Health::Up { generation: 2 });
    sandbox.spawn(fake.spec()).await.expect("reopened");
    assert_eq!(
        sandbox.address(),
        Ipv4Addr::new(192, 168, 99, 1),
        "gateway address stays unchanged"
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
    .expect("start");
    let sandbox = anchored.relay(8787).await.expect("relay");
    let mut health = sandbox.health().unwrap();
    let first = anchor_name(&fake).await;
    sandbox
        .spawn(fake.spec())
        .await
        .expect("spawn succeeds while address exists");
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
        "anchor whose gateway vanished must be killed"
    );
    std::fs::remove_file(&hidden).unwrap();
    let up_again = until_health(&mut health, |h| matches!(h, Health::Up { .. })).await;
    assert_eq!(up_again, Health::Up { generation: 2 });
    sandbox.spawn(fake.spec()).await.expect("reopened");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn every_spawn_itself_refuses_when_the_gateway_address_is_gone_even_before_the_watcher_notices()
 {
    let fake = Fake::new("ready-address");
    let hidden = fake.dir.join("address-hidden");
    let (seen, flag) = (address_while_anchored(&fake), hidden.clone());
    let hooks = Hooks {
        host_has: Arc::new(move |address| seen(address) && !flag.exists()),
        relay_up: Arc::new(|_| true),
        ready_within: Duration::from_secs(5),
        reap_pause: Duration::from_millis(1),
        reap_within: REAP_WITHIN,
        address_poll: Duration::from_secs(3600),
        ready_check: Duration::from_secs(5),
    };
    let anchored = ContainerSandbox::start_with(fake.config(), hooks)
        .await
        .expect("start");
    let sandbox = anchored.relay(8787).await.expect("relay");
    sandbox
        .spawn(fake.spec())
        .await
        .expect("spawn succeeds while address exists");
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
        .expect("spawn resumes once address returns");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_stuck_container_service_never_holds_a_spawn_beyond_the_readiness_window() {
    let fake = Fake::new("ready-stuck");
    let hooks = Hooks {
        host_has: Arc::new(address_while_anchored(&fake)),
        relay_up: Arc::new(|_| true),
        ready_within: Duration::from_secs(5),
        reap_pause: Duration::from_millis(1),
        reap_within: REAP_WITHIN,
        address_poll: Duration::from_secs(3600),
        ready_check: Duration::from_millis(300),
    };
    let anchored = ContainerSandbox::start_with(fake.config(), hooks)
        .await
        .expect("start");
    let sandbox = anchored.relay(8787).await.expect("relay");
    sandbox.spawn(fake.spec()).await.expect("healthy spawn");
    fake.set(json!({"inspect_hangs": true}));
    let started = std::time::Instant::now();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "stuck readiness check must time out quickly: {:?}",
        started.elapsed()
    );
    fake.set(json!({"inspect_hangs": false}));
    sandbox.spawn(fake.spec()).await.expect("reopened");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn dropping_the_handle_ends_the_anchor_and_closes_model_vms() {
    let fake = Fake::new("anchor-drop");
    let sandbox = up(&fake).await;
    let mut health = sandbox.health().unwrap();
    let name = anchor_name(&fake).await;
    let spec = fake.spec();
    drop(sandbox);
    until_health(&mut health, |h| h == Health::Down).await;
    for _ in 0..100 {
        if fake.read()["killed"]
            .as_array()
            .unwrap()
            .contains(&json!(name))
        {
            let _ = spec;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("dropped anchor was never killed");
}
