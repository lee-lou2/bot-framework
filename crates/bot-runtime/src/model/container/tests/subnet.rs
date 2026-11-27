//! Host-only network subnet pinning tests: private IPv4 CIDR validation and create-only passing.

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
    assert!(fake.calls().is_empty(), "container must not be invoked");
}

#[tokio::test]
async fn the_subnet_is_passed_only_when_the_network_is_created() {
    let fake = Fake::new("subnet-create");
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config()).await.abort().await;
    let created = fake
        .argv("network")
        .into_iter()
        .find(|a| a[1] == "create")
        .expect("network created");
    assert!(
        !created.contains(&"--subnet".to_owned()),
        "omitted subnet should defer to container default: {created:?}"
    );
    fake.set(json!({"networks": {}}));
    start(&fake, fake.config().with_subnet("192.168.222.0/24"))
        .await
        .abort()
        .await;
    let created = fake
        .argv("network")
        .into_iter()
        .rfind(|a| a[1] == "create")
        .expect("network created");
    assert!(
        created
            .windows(2)
            .any(|w| w == ["--subnet", "192.168.222.0/24"]),
        "{created:?}"
    );
    assert_eq!(created.last().unwrap(), "botvm-net");
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
        "existing network must not be recreated"
    );
}
