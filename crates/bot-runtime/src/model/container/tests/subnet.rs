//! 호스트 전용 망의 부분망 고정: 사설 IPv4 `주소/길이`(16~29)만 받고, 망을 새로 만들 때만 `--subnet`으로 넘긴다.

use super::*;

#[tokio::test]
async fn only_a_private_ipv4_subnet_is_accepted_before_any_command_runs() {
    let fake = Fake::new("subnet-invalid");
    for subnet in [
        "8.8.8.0/24",
        "192.168.1.0",
        "192.168.1.0/8",
        "192.168.1.0/30",
        "x/24",
        "",
    ] {
        let mut config = fake.config();
        config.subnet = Some(subnet.into());
        assert_eq!(
            ContainerSandbox::start(config).await.err(),
            Some(StartError("config")),
            "{subnet}"
        );
    }
    assert!(fake.calls().is_empty(), "container를 한 번도 부르지 않았다");
}

#[tokio::test]
async fn the_subnet_is_passed_only_when_the_network_is_created() {
    let fake = Fake::new("subnet-create");
    // 정하지 않으면 `container`가 정한다
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config()).await.abort().await;
    let created = fake
        .argv("network")
        .into_iter()
        .find(|a| a[1] == "create")
        .expect("만들었다");
    assert!(
        !created.contains(&"--subnet".to_owned()),
        "부분망을 정하지 않으면 container가 정한다: {created:?}"
    );
    // 정하면 새로 만들 때 그대로 넘긴다(이름 앞에)
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config().with_subnet("192.168.222.0/24"))
        .await
        .abort()
        .await;
    let created = fake
        .argv("network")
        .into_iter()
        .rfind(|a| a[1] == "create")
        .expect("만들었다");
    assert!(
        created
            .windows(2)
            .any(|w| w == ["--subnet", "192.168.222.0/24"]),
        "{created:?}"
    );
    assert_eq!(created.last().unwrap(), "botvm-net");
    // 이미 있는 망에는 쓰지 않는다
    let before = fake.argv("network").len();
    start(&fake, fake.config().with_subnet("192.168.223.0/24"))
        .await
        .abort()
        .await;
    assert!(
        fake.argv("network")
            .iter()
            .skip(before)
            .all(|a| a[1] != "create"),
        "있는 망은 다시 만들지 않는다"
    );
}
