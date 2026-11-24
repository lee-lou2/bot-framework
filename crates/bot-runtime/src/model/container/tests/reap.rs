//! 소유와 정리: 인스턴스 소유권 잠금·시작 때의 고아 정리(정확한 라벨·이름 모양만)·`reap`·종료 정리. 확인하지 못하면 지우지 않는다.

use super::*;

/// 이 어댑터가 띄운 VM이 단 라벨(가짜 CLI의 `run` 인자에서 읽는다).
fn label_of(fake: &Fake, key: &str) -> String {
    fake.argv("run")
        .into_iter()
        .flatten()
        .find_map(|a| a.strip_prefix(&format!("{key}=")).map(str::to_owned))
        .unwrap_or_else(|| panic!("{key} 라벨이 없다"))
}

#[tokio::test]
async fn one_instance_has_one_owner_and_other_instances_coexist_without_touching_each_other() {
    let fake = Fake::new("own-lock");
    let first = start(&fake, fake.config()).await;
    // 같은 인스턴스 이름의 둘째 어댑터는 시작 때 거절된다: 서로의 닻·VM을 정리하지 않는다
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
    // 다른 인스턴스(접두사가 이쪽을 포함해도)는 함께 쓴다
    let other = start(&fake, fake.config().named("botvm-secondary")).await;
    assert_eq!(*first.health().borrow(), Health::Up { generation: 1 });
    first.abort().await;
    // 한쪽이 끝나도 다른 쪽의 닻과 VM은 그대로다
    assert_eq!(*other.health().borrow(), Health::Up { generation: 1 });
    let ids = fake.ids();
    assert!(
        ids.iter().any(|id| id.starts_with("botvm-secondary-")),
        "{ids:?}"
    );
    assert!(!ids.iter().any(|id| is_ours("botvm", id)), "{ids:?}");
    // 소유권을 놓았으니 같은 이름으로 다시 시작할 수 있다
    start(&fake, fake.config()).await.abort().await;
    other.abort().await;
}

#[tokio::test]
async fn a_lock_released_within_the_retry_window_does_not_refuse_the_start_but_a_held_one_does() {
    // 같은 프로세스가 방금 `fork`한 자식은 exec 전까지 (이미 닫은) 잠금 기술자를 쥘 수 있다: 잠깐 쥐었다 놓는 쪽은 시작을 거절하지 않는다
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
    // 계속 쥐고 있으면(진짜 다른 소유자) 재시도가 끝난 뒤에 거절한다
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
        // 이 인스턴스의 이전 실행이 남긴 고아(실행 표지가 없는 옛 VM도 이 인스턴스의 것이다)
        fake.vm("botvm-aaaaaaaaaaaa-1", mine(Some("old-run"))),
        fake.vm("botvm-bbbbbbbbbbbb-2", mine(None)),
        // 같은 기계의 다른 인스턴스: 접두사가 이쪽을 포함해도, 라벨이 같은 모양이어도 건드리지 않는다
        fake.vm("botvm-secondary-cccccccccccc-1", json!({"bot.managed": "1", "bot.instance": "botvm-secondary", "bot.incarnation": "x"})),
        fake.vm("botvm2-dddddddddddd-1", json!({"bot.managed": "1", "bot.instance": "botvm2"})),
        // 소유를 확인할 수 없는 것: 인스턴스 라벨이 없다·관리 라벨이 없다·이름이 우리 모양이 아니다·라벨이 하나도 없다
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
    // 시작이 치운 고아가 먼저이고(그 뒤는 `abort`가 치운 이번 실행의 닻이다), 다른 인스턴스·확인 못 한 VM은 하나도 지우지 않는다
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
    // 지워지지 않으면(조회로 확인) 시작하지 않는다
    fake.set(json!({"containers": [orphan.clone()], "delete_stuck": true}));
    assert_eq!(
        attempt(&fake).await.err(),
        Some(StartError("cleanup_failed"))
    );
    // 조회를 못 하면 모른다: 지우지도 시작하지도 않는다
    fake.set(
        json!({"containers": [orphan], "delete_stuck": false, "ls_fails": true, "deleted": []}),
    );
    assert_eq!(
        attempt(&fake).await.err(),
        Some(StartError("cleanup_failed"))
    );
    assert!(fake.read()["deleted"].as_array().unwrap().is_empty());
    assert!(fake.argv("run").is_empty(), "닻도 모델도 띄우지 않았다");
    // 실패한 시작은 소유권을 쥐고 있지 않다
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
        // 같은 소유 표지: 이번 실행의 것과 이전 실행의 것(둘 다 이 인스턴스의 것이다)
        fake.vm("botvm-111111111111-5", label(owner, "botvm", &incarnation)),
        fake.vm("botvm-222222222222-6", label(owner, "botvm", "older-run")),
        // 다른 소유 표지·다른 인스턴스·소유 표지만 같은 남의 VM
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
    // 종료는 이번 실행이 띄운 것(닻 포함)만 치운다: 이전 실행의 고아는 다음 시작이 치운다
    fake.set(json!({"deleted": [], "killed": []}));
    sandbox.shutdown().await;
    assert_eq!(fake.ids(), ["botvm-secondary-444444444444-1", "foreign"]);
    assert!(
        fake.read()["deleted"]
            .as_array()
            .unwrap()
            .contains(&json!("botvm-333333333333-7")),
        "이번 실행이 띄운 VM은 치운다"
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
