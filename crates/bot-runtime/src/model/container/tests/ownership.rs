//! 인스턴스 소유권: 같은 인스턴스 이름의 어댑터는 호스트 사용자에게 하나뿐이다. 잠금은 작업 폴더·마운트 루트 같은 설정이 아니라 이름과 호스트 사용자로만 정해지는
//! 고정 위치에 있고(`lock.rs`), 두 번째 시작은 VM을 하나도 건드리기 전에 `instance_in_use`로 끝난다. 안전하지 않은 잠금 폴더는 거절하고,
//! 잠금 파일이 지워지거나 바뀌면 모델 VM을 거절한다.

use super::*;

#[tokio::test]
async fn the_lock_belongs_to_the_instance_name_not_to_the_work_folder() {
    let fake = Fake::new("own-root");
    let first = start(&fake, fake.config()).await;
    let before = fake.calls().len();
    // 같은 인스턴스 이름, 작업 폴더(마운트 루트)만 다르다: 같은 잠금이다
    let elsewhere_root = fake.dir.join("mounts-b");
    std::fs::create_dir_all(&elsewhere_root).unwrap();
    let mut elsewhere = fake.config();
    elsewhere.mount_root = elsewhere_root;
    let refused = ContainerSandbox::start(elsewhere).await.err();
    assert_eq!(refused, Some(StartError("instance_in_use")));
    // VM을 하나도 건드리지 않았다: `container`를 한 번도 부르지 않았고 첫 어댑터의 닻은 그대로다
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
    // 홈을 알 수 없거나 절대 경로가 아니면 정하지 못한다(시작하지 않는다)
    assert_eq!(
        lock::default_lock_dir(None),
        Err(StartError("instance_lock_failed"))
    );
    assert_eq!(
        lock::default_lock_dir(Some("relative".into())),
        Err(StartError("instance_lock_failed"))
    );
    // 이름이 같으면 같은 파일, 다르면 다른 파일이다. 작업 폴더·이미지·망 같은 설정은 경로에 끼지 않는다
    assert_eq!(lock::lock_path(&place, "a"), lock::lock_path(&place, "a"));
    assert_ne!(lock::lock_path(&place, "a"), lock::lock_path(&place, "b"));
    let (one, two) = (
        ContainerConfig::new("img-1", "net-1", "/work/a"),
        ContainerConfig::new("img-2", "net-2", "/work/b"),
    );
    assert_eq!(lock::lock_dir(&one), lock::lock_dir(&two));
    // 시험만 폴더를 바꾼다
    assert_eq!(
        lock::lock_dir(&one.with_lock_dir("/tmp/x")),
        Ok(PathBuf::from("/tmp/x"))
    );
}

#[tokio::test]
async fn an_unsafe_lock_folder_is_refused_before_any_command_runs() {
    let fake = Fake::new("own-unsafe");
    // 그룹·다른 사용자에게 열린 폴더
    let open = fake.locks();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        ContainerSandbox::start(fake.config()).await.err(),
        Some(StartError("instance_lock_failed"))
    );
    // 진짜 폴더가 아니라 심볼릭 링크
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = fake.dir.join("locks-link");
    std::os::unix::fs::symlink(&open, &link).unwrap();
    let mut through_link = fake.config();
    through_link.lock_dir = Some(link);
    assert_eq!(
        ContainerSandbox::start(through_link).await.err(),
        Some(StartError("instance_lock_failed"))
    );
    // 상대 경로는 설정부터 거절한다
    let mut relative = fake.config();
    relative.lock_dir = Some("locks".into());
    assert_eq!(
        ContainerSandbox::start(relative).await.err(),
        Some(StartError("config"))
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    // 사용자만 쓰는 폴더이면 시작한다
    start(&fake, fake.config()).await.abort().await;
}

#[tokio::test]
async fn a_lock_file_that_is_deleted_or_replaced_closes_model_runs() {
    let fake = Fake::new("own-vanish");
    let sandbox = up(&fake).await;
    sandbox
        .spawn(fake.spec())
        .await
        .expect("잠금을 쥐고 있는 동안은 연다");
    // 누가 잠금 파일을 지웠다(정리 도구·사람): 소유를 보증할 수 없으니 닫힌 채 실패한다
    std::fs::remove_file(fake.lock_file("botvm")).unwrap();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    // 같은 이름으로 다시 만들어도 처음 잠근 그 파일이 아니다
    std::fs::write(fake.lock_file("botvm"), "").unwrap();
    assert!(matches!(
        sandbox.spawn(fake.spec()).await,
        Err(Error::Unavailable)
    ));
    sandbox.shutdown().await;
}
