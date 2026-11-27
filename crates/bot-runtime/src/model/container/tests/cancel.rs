//! Container cancellation and cleanup tests ensuring no VM survives late creation, drop, or inspect failure.

use super::*;

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
    // Simulates a late create landing 300ms after the first delete (within the 400ms absence-confirmation window).
    fake.set(json!({"late_after_delete_ms": 300, "hang": true, "lines": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    process.kill().await;
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
    assert_eq!(models(&fake).len(), 1, "VM should be registered");
    drop(process);
    for _ in 0..100 {
        if models(&fake).is_empty() {
            sandbox.shutdown().await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "dropped container remained: {:?}",
        fake.read()["containers"]
    );
}

#[tokio::test]
async fn a_start_dropped_during_the_readiness_check_launches_no_vm() {
    let fake = Fake::new("cancel-ready");
    let sandbox = up(&fake).await;
    let before = fake.argv("run").len();
    fake.set(json!({"inspect_hangs": true}));
    let started =
        tokio::time::timeout(Duration::from_millis(200), sandbox.spawn(fake.spec())).await;
    assert!(started.is_err(), "spawn should wait while inspect hangs");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fake.argv("run").len(),
        before,
        "dropped spawn must not launch a VM"
    );
    assert!(models(&fake).is_empty());
    fake.set(json!({"inspect_hangs": false}));
    sandbox.shutdown().await;
}

#[tokio::test]
async fn kill_gives_up_after_its_budget_when_the_container_cannot_be_confirmed_gone() {
    let fake = Fake::new("cancel-stuck");
    let sandbox = up(&fake).await;
    fake.set(json!({"register": true, "delete_stuck": true, "hang": true, "lines": []}));
    let mut process = sandbox.spawn(fake.spec()).await.unwrap();
    for _ in 0..40 {
        if !models(&fake).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(models(&fake).len(), 1, "VM should be registered");
    let began = std::time::Instant::now();
    process.kill().await;
    assert!(
        began.elapsed() < Duration::from_secs(8),
        "{:?}",
        began.elapsed()
    );
    assert!(!models(&fake).is_empty(), "container remained stuck");
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
    let alive = fake.read()["alive_at_remove"].clone();
    let alive = alive.as_array().unwrap();
    assert!(!alive.is_empty(), "expected remove commands");
    assert!(alive.iter().all(|a| a == false), "{alive:?}");
    sandbox.shutdown().await;
}

#[tokio::test]
async fn a_failed_inspect_is_not_proof_that_the_container_is_gone() {
    let fake = Fake::new("cancel-unknown");
    let sandbox = up(&fake).await;
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
        "returned before exhausting retry budget: {:?}",
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
