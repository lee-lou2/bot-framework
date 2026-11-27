//! Instance ownership lock, startup orphan cleanup, `reap`, and shutdown teardown tests.

use super::*;

fn label_of(fake: &Fake, key: &str) -> String {
    fake.argv("run")
        .into_iter()
        .flatten()
        .find_map(|a| a.strip_prefix(&format!("{key}=")).map(str::to_owned))
        .unwrap_or_else(|| panic!("missing label {key}"))
}

#[tokio::test]
async fn one_instance_has_one_owner_and_other_instances_coexist_without_touching_each_other() {
    let fake = Fake::new("own-lock");
    let first = start(&fake, fake.config()).await;
    fake.set(json!({"killed": [], "deleted": []}));
    let second = ContainerSandbox::start_probed(
        fake.config(),
        address_while_anchored(&fake),
        |_| true,
        Duration::from_secs(5),
    )
    .await
    .err();
    assert_eq!(second, Some(StartError("instance_in_use")));
    assert!(fake.read()["killed"].as_array().unwrap().is_empty());
    assert!(fake.read()["deleted"].as_array().unwrap().is_empty());
    let other = start(&fake, fake.config().named("botvm-secondary")).await;
    assert_eq!(*first.health().borrow(), Health::Up { generation: 1 });
    first.abort().await;
    assert_eq!(*other.health().borrow(), Health::Up { generation: 1 });
    let ids = fake.ids();
    assert!(
        ids.iter().any(|id| id.starts_with("botvm-secondary-")),
        "{ids:?}"
    );
    assert!(!ids.iter().any(|id| is_ours("botvm", id)), "{ids:?}");
    start(&fake, fake.config()).await.abort().await;
    other.abort().await;
}

#[tokio::test]
async fn a_lock_released_within_the_retry_window_does_not_refuse_the_start_but_a_held_one_does() {
    // A freshly forked child can briefly inherit an fd before exec; lock acquisition retries briefly.
    let fake = Fake::new("lock-retry");
    let config = fake.config();
    fake.locks();
    let path = fake.lock_file(&config.instance);
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    rustix::fs::flock(&holder, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(holder);
    });
    let anchored = start(&fake, config.clone()).await;
    release.join().unwrap();
    anchored.abort().await;
    let other = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    rustix::fs::flock(&other, rustix::fs::FlockOperation::LockExclusive).unwrap();
    let refused = ContainerSandbox::start(config).await.err();
    assert_eq!(refused, Some(StartError("instance_in_use")));
    drop(other);
}

#[tokio::test]
async fn startup_removes_only_this_instances_orphans_and_preserves_everything_it_cannot_prove_is_its_own()
 {
    let fake = Fake::new("own-orphans");
    let mine = |incarnation: Option<&str>| {
        let mut labels =
            json!({"bot.managed": "1", "bot.instance": "botvm", "bot.owner": "p.r.g1.c1"});
        if let Some(incarnation) = incarnation {
            labels["bot.incarnation"] = json!(incarnation);
        }
        labels
    };
    fake.set(json!({"containers": [
        fake.vm("botvm-aaaaaaaaaaaa-1", mine(Some("old-run"))),
        fake.vm("botvm-bbbbbbbbbbbb-2", mine(None)),
        fake.vm("botvm-secondary-cccccccccccc-1", json!({"bot.managed": "1", "bot.instance": "botvm-secondary", "bot.incarnation": "x"})),
        fake.vm("botvm2-dddddddddddd-1", json!({"bot.managed": "1", "bot.instance": "botvm2"})),
        fake.vm("botvm-eeeeeeeeeeee-3", json!({"bot.managed": "1"})),
        fake.vm("botvm-ffffffffffff-4", json!({"bot.instance": "botvm"})),
        fake.vm("botvm-weird", mine(Some("old-run"))),
        fake.vm("foreign-project", json!({})),
    ]}));
    start(&fake, fake.config()).await.abort().await;
    let deleted: Vec<String> = fake.read()["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        deleted[..2],
        ["botvm-aaaaaaaaaaaa-1", "botvm-bbbbbbbbbbbb-2"]
    );
    assert!(
        deleted[2..]
            .iter()
            .all(|id| !fake.ids().contains(id) && id.starts_with("botvm-") && is_ours("botvm", id)),
        "{deleted:?}"
    );
    assert_eq!(
        fake.ids(),
        [
            "botvm-secondary-cccccccccccc-1",
            "botvm2-dddddddddddd-1",
            "botvm-eeeeeeeeeeee-3",
            "botvm-ffffffffffff-4",
            "botvm-weird",
            "foreign-project",
        ]
    );
}

#[tokio::test]
async fn a_startup_that_cannot_confirm_the_cleanup_starts_nothing() {
    let fake = Fake::new("own-cleanup-fail");
    let orphan = fake.vm(
        "botvm-aaaaaaaaaaaa-1",
        json!({"bot.managed": "1", "bot.instance": "botvm", "bot.incarnation": "old"}),
    );
    async fn attempt(fake: &Fake) -> Result<Anchored, StartError> {
        ContainerSandbox::start_probed(
            fake.config(),
            address_while_anchored(fake),
            |_| true,
            Duration::from_secs(1),
        )
        .await
    }
    fake.set(json!({"containers": [orphan.clone()], "delete_stuck": true}));
    assert_eq!(
        attempt(&fake).await.err(),
        Some(StartError("cleanup_failed"))
    );
    fake.set(
        json!({"containers": [orphan], "delete_stuck": false, "ls_fails": true, "deleted": []}),
    );
    assert_eq!(
        attempt(&fake).await.err(),
        Some(StartError("cleanup_failed"))
    );
    assert!(fake.read()["deleted"].as_array().unwrap().is_empty());
    assert!(fake.argv("run").is_empty(), "no VM should have started");
    fake.set(json!({"ls_fails": false}));
    attempt(&fake).await.unwrap().abort().await;
}

#[tokio::test]
async fn reap_and_shutdown_end_only_this_instances_matching_vms() {
    let fake = Fake::new("own-reap");
    let sandbox = up(&fake).await;
    let incarnation = label_of(&fake, "bot.incarnation");
    let owner = "personal.run-1.g2.c3";
    let label = |owner: &str, instance: &str, incarnation: &str| json!({"bot.managed": "1", "bot.instance": instance, "bot.owner": label_value(owner), "bot.incarnation": incarnation});
    let mut containers = fake.read()["containers"].as_array().unwrap().clone();
    containers.extend([
        fake.vm("botvm-111111111111-5", label(owner, "botvm", &incarnation)),
        fake.vm("botvm-222222222222-6", label(owner, "botvm", "older-run")),
        fake.vm(
            "botvm-333333333333-7",
            label("personal.run-9.g1.c1", "botvm", &incarnation),
        ),
        fake.vm(
            "botvm-secondary-444444444444-1",
            label(owner, "botvm-secondary", &incarnation),
        ),
        fake.vm("foreign", label(owner, "other", &incarnation)),
    ]);
    fake.set(json!({"containers": containers, "killed": [], "deleted": []}));
    sandbox.reap(owner).await.unwrap();
    assert_eq!(
        fake.read()["deleted"],
        json!(["botvm-111111111111-5", "botvm-222222222222-6"])
    );
    fake.set(json!({"deleted": [], "killed": []}));
    sandbox.shutdown().await;
    assert_eq!(fake.ids(), ["botvm-secondary-444444444444-1", "foreign"]);
    assert!(
        fake.read()["deleted"]
            .as_array()
            .unwrap()
            .contains(&json!("botvm-333333333333-7")),
        "VMs launched by this incarnation must be removed on shutdown"
    );
    assert!(
        fake.read()["killed"]
            .as_array()
            .unwrap()
            .iter()
            .all(|id| is_ours("botvm", id.as_str().unwrap())),
        "{:?}",
        fake.read()["killed"]
    );
}

#[tokio::test]
async fn reap_clamps_stuck_subcommands_to_the_shared_deadline_budget() {
    let fake = Fake::new("reap-budget");
    let hooks = Hooks {
        host_has: Arc::new(address_while_anchored(&fake)),
        relay_up: Arc::new(|_| true),
        ready_within: Duration::from_secs(5),
        reap_pause: Duration::from_millis(10),
        reap_within: Duration::from_millis(350),
        address_poll: Duration::from_secs(3600),
        ready_check: Duration::from_secs(5),
    };
    let sandbox = ContainerSandbox::start_with(fake.config(), hooks)
        .await
        .expect("start")
        .relay(8787)
        .await
        .expect("relay");
    let owner = "personal.run-budget.g1.c1";
    let mut containers = fake.read()["containers"].as_array().unwrap().clone();
    containers.push(fake.vm(
        "botvm-999999999999-1",
        json!({
            "bot.managed": "1",
            "bot.instance": "botvm",
            "bot.owner": label_value(owner),
            "bot.incarnation": "other"
        }),
    ));
    fake.set(json!({"containers": containers, "kill_hangs": true}));
    let started = std::time::Instant::now();
    let result = sandbox.reap(owner).await;
    let elapsed = started.elapsed();
    assert!(matches!(result, Err(Error::Unavailable)), "{result:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "must complete within shared 350ms deadline: {elapsed:?}"
    );
    fake.set(json!({"kill_hangs": false}));
    sandbox.shutdown().await;
}
