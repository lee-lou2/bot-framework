//! Instance ownership lock tests covering user-scoped lock paths, permission checks, and lock replacement.

use super::*;

#[tokio::test]
async fn the_lock_belongs_to_the_instance_name_not_to_the_work_folder() {
    let fake = Fake::new("own-root");
    let first = start(&fake, fake.config()).await;
    let before = fake.calls().len();
    let elsewhere_root = fake.dir.join("mounts-b");
    std::fs::create_dir_all(&elsewhere_root).unwrap();
    let mut elsewhere = fake.config();
    elsewhere.mount_root = elsewhere_root;
    let refused = ContainerSandbox::start(elsewhere).await.err();
    assert_eq!(refused, Some(StartError("instance_in_use")));
    assert_eq!(fake.calls().len(), before, "{:?}", fake.calls());
    assert!(fake.read()["killed"].as_array().unwrap().is_empty());
    assert!(fake.read()["deleted"].as_array().unwrap().is_empty());
    assert_eq!(*first.health().borrow(), Health::Up { generation: 1 });
    first.abort().await;
}

#[test]
fn without_a_configured_folder_the_lock_place_depends_only_on_the_host_user() {
    let home = PathBuf::from("/Users/someone");
    let place = lock::default_lock_dir(Some(home.clone())).unwrap();
    assert_eq!(place, home.join(".local/state/bot-framework/locks"));
    assert_eq!(
        lock::default_lock_dir(None),
        Err(StartError("instance_lock_failed"))
    );
    assert_eq!(
        lock::default_lock_dir(Some("relative".into())),
        Err(StartError("instance_lock_failed"))
    );
    assert_eq!(lock::lock_path(&place, "a"), lock::lock_path(&place, "a"));
    assert_ne!(lock::lock_path(&place, "a"), lock::lock_path(&place, "b"));
    let (one, two) = (
        ContainerConfig::new("img-1", "net-1", "/work/a"),
        ContainerConfig::new("img-2", "net-2", "/work/b"),
    );
    assert_eq!(lock::lock_dir(&one), lock::lock_dir(&two));
    assert_eq!(
        lock::lock_dir(&one.with_lock_dir("/tmp/x")),
        Ok(PathBuf::from("/tmp/x"))
    );
}

#[tokio::test]
async fn an_unsafe_lock_folder_is_refused_before_any_command_runs() {
    let fake = Fake::new("own-unsafe");
    let open = fake.locks();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        ContainerSandbox::start(fake.config()).await.err(),
        Some(StartError("instance_lock_failed"))
    );
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = fake.dir.join("locks-link");
    std::os::unix::fs::symlink(&open, &link).unwrap();
    let mut through_link = fake.config();
    through_link.lock_dir = Some(link);
    assert_eq!(
        ContainerSandbox::start(through_link).await.err(),
        Some(StartError("instance_lock_failed"))
    );
    let mut relative = fake.config();
    relative.lock_dir = Some("locks".into());
    assert_eq!(
        ContainerSandbox::start(relative).await.err(),
        Some(StartError("config"))
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    start(&fake, fake.config()).await.abort().await;
}

#[tokio::test]
async fn a_lock_file_that_is_deleted_or_replaced_closes_model_runs() {
    let fake = Fake::new("own-vanish");
    let sandbox = up(&fake).await;
    sandbox
        .spawn(fake.spec())
        .await
        .expect("spawn succeeds while lock is held");
    std::fs::remove_file(fake.lock_file("botvm")).unwrap();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    std::fs::write(fake.lock_file("botvm"), "").unwrap();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    sandbox.shutdown().await;
}
