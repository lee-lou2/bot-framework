//! 호출 간 상태 볼륨을 요구하는 서버(`state: required`): srt 어댑터는 영속 볼륨을 지원하지 않으므로 자격·설정·프로세스를 만들기 전에
//! 명시적으로 거절한다. 기본(`Ephemeral`)은 그대로 돈다.

use super::*;
use bot_core::install::ServerState;

fn required(dir: &Path) -> SrtConfig {
    let mut cfg = config(dir, fake_srt(dir, PINNED_VERSION));
    cfg.servers.get_mut("demo-agenda").unwrap().state = ServerState::Required;
    cfg
}

async fn open_required(dir: &Path) -> SrtSandbox {
    SrtSandbox::open(required(dir), Arc::new(CountingEntropy::default()))
        .await
        .unwrap()
}

#[tokio::test]
async fn a_server_that_needs_a_state_volume_is_refused_before_any_credential_or_process() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = open_required(dir.path()).await;
    // 미리 묻는 길: 자격을 풀기 전에 거절한다
    assert!(matches!(
        sandbox.admit("demo-agenda", "img1"),
        Err(Error::Denied)
    ));
    // 곧바로 띄우려 해도(자격이 든 명세로) 거절하고, 설정·pid 기록·작업 폴더·프로세스를 만들지 않는다
    let result = sandbox.spawn(spec("r1.1.c1", false)).await;
    assert!(matches!(result, Err(Error::Denied)));
    assert!(
        !dir.path().join("work/.srt").exists()
            || std::fs::read_dir(dir.path().join("work/.srt"))
                .unwrap()
                .count()
                == 0,
        "거절 전에 아무것도 만들지 않는다"
    );
    let work: Vec<_> = std::fs::read_dir(dir.path().join("work"))
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(
        work.iter().all(|name| name == ".srt"),
        "호출별 작업 폴더도 없다: {work:?}"
    );
}

#[tokio::test]
async fn the_default_state_is_ephemeral_and_still_runs_and_unknown_servers_stay_refused() {
    let dir = tempfile::tempdir().unwrap();
    let sandbox = open(dir.path()).await;
    assert!(sandbox.admit("demo-agenda", "img1").is_ok());
    // 설치하지 않은 서버·승인과 다른 판본은 기존처럼 거절한다
    assert!(matches!(sandbox.admit("other", "img1"), Err(Error::Denied)));
    assert!(matches!(
        sandbox.admit("demo-agenda", "img2"),
        Err(Error::Denied)
    ));
    let process = sandbox.spawn(spec("r1.1.c2", false)).await.unwrap();
    process.handle.terminate().await.unwrap();
}

#[test]
fn the_default_of_the_core_enum_is_ephemeral() {
    assert_eq!(ServerState::default(), ServerState::Ephemeral);
}
