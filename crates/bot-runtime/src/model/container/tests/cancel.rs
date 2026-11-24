//! 취소: 뜨는 중인 VM을 끊을 때 컨테이너가 남지 않는다. 점검 중에 버려진 시작은 아무것도 띄우지 않고, 끝내라는 요청은 클라이언트를 먼저 끝낸 뒤
//! 컨테이너가 사라진 것을 확인할 때까지 지운다(클라이언트가 죽기 직전에 보낸 «만들기» 요청이 늦게 컨테이너를 만들어도 남지 않는다).

use super::*;

/// 목록에 있는 모델 VM(닻이 아닌 것)의 ID들.
fn models(fake: &Fake) -> Vec<String> {
    fake.read()["containers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c.get("anchor").is_none())
        .map(|c| c["configuration"]["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn killing_a_vm_whose_creation_lands_late_still_leaves_nothing_behind() {
    let fake = Fake::new("cancel-late");
    let sandbox = up(&fake).await;
    // 첫 지우기가 지나고 300ms 뒤에 «만들기» 요청이 컨테이너를 만든다(첫 지우기 때는 없었고, «없음»이 400ms 이어지기 전에 나타난다)
    fake.set(json!({"late_after_delete_ms": 300, "hang": true, "lines": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    process.kill().await;
    // 돌아온 뒤에도, 늦게 도착하는 시각을 지나도록 기다려도 컨테이너가 없다
    for _ in 0..8 {
        assert!(models(&fake).is_empty(), "{:?}", fake.read()["containers"]);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!fake.read()["deleted"].as_array().unwrap().is_empty());
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_process_dropped_without_a_kill_is_removed_in_the_background() {
    let fake = Fake::new("cancel-drop");
    let sandbox = up(&fake).await;
    fake.set(json!({"register": true, "hang": true, "lines": []}));
    let process = sandbox.spawn(fake.spec()).await.unwrap();
    for _ in 0..40 {
        if !models(&fake).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(models(&fake).len(), 1, "VM이 목록에 올랐다");
    // 끝났다는 확인 없이 놓으면(취소·패닉) 컨테이너까지 지운다(끝내라고만 하면 `stopped`로 남는다)
    drop(process);
    for _ in 0..100 {
        if models(&fake).is_empty() {
            sandbox.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("놓은 컨테이너가 남았다: {:?}", fake.read()["containers"]);
}

#[tokio::test]
async fn a_start_dropped_during_the_readiness_check_launches_no_vm() {
    let fake = Fake::new("cancel-ready");
    let sandbox = up(&fake).await;
    let before = fake.argv("run").len();
    // 망 점검이 멈춘다: 점검 중에 시작을 버린다(호출자가 취소했다)
    fake.set(json!({"inspect_hangs": true}));
    let started =
        tokio::time::timeout(Duration::from_millis(200), sandbox.spawn(fake.spec())).await;
    assert!(started.is_err(), "점검이 멈췄으니 끝나지 않는다");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.argv("run").len(),
        before,
        "점검 중에 버려진 시작은 VM을 띄우지 않았다"
    );
    assert!(models(&fake).is_empty());
    fake.set(json!({"inspect_hangs": false}));
    sandbox.shutdown().await;
}

#[tokio::test]
async fn kill_gives_up_after_its_budget_when_the_container_cannot_be_confirmed_gone() {
    let fake = Fake::new("cancel-stuck");
    let sandbox = up(&fake).await;
    // 지우기가 먹히지 않고 컨테이너가 계속 보인다: 무한정 기다리지 않는다(정리 확인 단계가 한 번 더 본다)
    fake.set(json!({"register": true, "delete_stuck": true, "hang": true, "lines": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    for _ in 0..40 {
        if !models(&fake).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(models(&fake).len(), 1, "VM이 목록에 올랐다");
    let began = std::time::Instant::now();
    process.kill().await;
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    assert!(!models(&fake).is_empty(), "지워지지 않았다");
    fake.set(json!({"delete_stuck": false}));
    sandbox.shutdown().await;
}

#[tokio::test]
async fn the_client_is_ended_before_any_command_that_removes_the_container() {
    let fake = Fake::new("cancel-order");
    let sandbox = up(&fake).await;
    fake.set(json!({"register": true, "hang": true, "lines": [], "alive_at_remove": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    for _ in 0..40 {
        if !models(&fake).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    process.kill().await;
    // 클라이언트가 살아 있는 동안 지우면 클라이언트가 보낸 «만들기» 요청이 지운 뒤에 컨테이너를 만든다: 모든 지우기 명령 앞에 클라이언트는 이미 죽어 있다
    let alive = fake.read()["alive_at_remove"].clone();
    let alive = alive.as_array().unwrap();
    assert!(!alive.is_empty(), "지우기 명령이 있었다");
    assert!(alive.iter().all(|a| a == false), "{alive:?}");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_failed_inspect_is_not_proof_that_the_container_is_gone() {
    let fake = Fake::new("cancel-unknown");
    let sandbox = up(&fake).await;
    // 서비스가 응답하지 않아 있는지 없는지 모른다: 사라졌다고 믿지 않고 예산이 끝날 때까지 다시 시도한다
    fake.set(json!({"register": true, "hang": true, "lines": [], "inspect_unknown": true}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    for _ in 0..40 {
        if !models(&fake).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let began = std::time::Instant::now();
    process.kill().await;
    assert!(
        began.elapsed() >= Duration::from_secs(3),
        "확인하지 못했는데 일찍 돌아왔다: {:?}",
        began.elapsed()
    );
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    fake.set(json!({"inspect_unknown": false}));
    sandbox.shutdown().await;
}
