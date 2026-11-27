//! Termination proof tests: tombstones, missing or corrupt records, lookup failures, and post-restart cleanup.

use super::*;

#[tokio::test]
async fn reap_ends_leftovers_even_after_a_broker_restart_and_confirms() {
    let tmp = tempfile::tempdir().unwrap();
    let first = open(tmp.path()).await;
    let process = first.spawn(spec("r1.1.c1", false)).await.unwrap();
    let work = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.file_name().unwrap() != ".srt")
        .unwrap();
    let background: u32 = wait_for(&work.join("bg.pid")).await.trim().parse().unwrap();
    // Simulate broker restart: fresh adapter discovers leftovers solely via on-disk PID metadata.
    let second = open(tmp.path()).await;
    second.reap("r1.1.c1").await.unwrap();
    let alive = std::process::Command::new("/bin/kill")
        .args(["-s", "0", &background.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(!alive);
    assert!(!work.exists() && leftovers(tmp.path()).is_empty());
    second.reap("r1.1.c1").await.unwrap();
    assert!(matches!(
        second.reap("r9.9.c9").await,
        Err(Error::Unavailable)
    ));
    drop(process);
}

#[tokio::test]
async fn reap_refuses_to_touch_a_group_whose_leader_is_not_ours() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    // Simulate PID reuse where the recorded process group leader belongs to an unrelated process.
    let mut stranger = std::process::Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let meta = tmp.path().join("work/.srt");
    std::fs::create_dir_all(&meta).unwrap();
    let stem = format!("{}-{}", owner_prefix("r1.1.c1"), "0".repeat(32));
    std::fs::write(
        meta.join(format!("{stem}.pid")),
        format!(
            "{}\n{}\n",
            stranger.id(),
            tmp.path().join("work").join(&stem).display()
        ),
    )
    .unwrap();
    let result = sandbox.reap("r1.1.c1").await;
    assert!(
        matches!(result, Err(Error::Unavailable)),
        "unverified process group must stay held"
    );
    assert!(
        stranger.try_wait().unwrap().is_none(),
        "unrelated process must remain alive"
    );
    stranger.kill().unwrap();
    let _ = stranger.wait();
}

#[tokio::test]
async fn terminating_leaves_a_tombstone_so_a_later_reap_of_the_same_owner_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("r1.1.c1", false)).await.unwrap();
    Box::new(process.handle).terminate().await.unwrap();
    let meta = tmp.path().join("work/.srt");
    assert_eq!(
        std::fs::read_dir(&meta)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "done"))
            .count(),
        1
    );
    let restarted = open(tmp.path()).await;
    restarted.reap("r1.1.c1").await.unwrap();
    restarted.reap("r1.1.c1").await.unwrap();
}

#[tokio::test]
async fn reap_refuses_to_vouch_without_a_record_or_with_a_broken_one() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let meta = tmp.path().join("work/.srt");
    assert!(matches!(
        sandbox.reap("r9.9.c9").await,
        Err(Error::Unavailable)
    ));
    let broken_pid = meta.join(format!(
        "{}-{}.pid",
        owner_prefix("r2.1.c1"),
        "a".repeat(32)
    ));
    for (text, case) in [
        ("pending\n/w\n", "pending"),
        ("not-a-number\n/w\n", "garbage"),
        ("123\n", "no work path"),
    ] {
        std::fs::write(&broken_pid, text).unwrap();
        assert!(
            matches!(sandbox.reap("r2.1.c1").await, Err(Error::Unavailable)),
            "{case}"
        );
        std::fs::remove_file(&broken_pid).unwrap();
    }
    std::fs::remove_dir_all(&meta).unwrap();
    assert!(matches!(
        sandbox.reap("r1.1.c1").await,
        Err(Error::Unavailable)
    ));
}

#[tokio::test]
async fn a_failing_group_lookup_is_unknown_not_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = fake_srt(tmp.path(), PINNED_VERSION);
    let real = HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: PathBuf::from("/usr/bin/pgrep"),
        ps: PathBuf::from("/bin/ps"),
    };
    // pgrep exit code 3 indicates an error, not an empty process group.
    let mut cfg = config(tmp.path(), binary.clone());
    cfg.terminate_wait = Duration::from_millis(300);
    cfg.host_tools = Some(HostTools {
        pgrep: fake_tool(tmp.path(), "pgrep-broken", 3),
        ..real.clone()
    });
    let sandbox = SrtSandbox::open(cfg, Arc::new(CountingEntropy::default()))
        .await
        .unwrap();
    let process = sandbox.spawn(spec("r1.1.c1", false)).await.unwrap();
    assert!(
        matches!(
            Box::new(process.handle).terminate().await,
            Err(Error::Unavailable)
        ),
        "failing lookup must not confirm termination"
    );
    assert!(matches!(
        sandbox.reap("r1.1.c1").await,
        Err(Error::Unavailable)
    ));
    // Only pgrep exit code 1 confirms no matching processes remain.
    let mut cfg = config(tmp.path(), binary);
    cfg.work_root = tmp.path().join("work2");
    cfg.host_tools = Some(HostTools {
        pgrep: fake_tool(tmp.path(), "pgrep-empty", 1),
        ..real
    });
    let sandbox = SrtSandbox::open(cfg, Arc::new(CountingEntropy::default()))
        .await
        .unwrap();
    let process = sandbox.spawn(spec("r2.1.c1", false)).await.unwrap();
    Box::new(process.handle).terminate().await.unwrap();
}

#[tokio::test]
async fn reap_never_matches_another_owner_with_a_shared_prefix_or_invalid_nonce() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let process = sandbox.spawn(spec("cli-1-0-extra", false)).await.unwrap();
    let work = work_dir(tmp.path());
    let bg: u32 = wait_for(&work.join("bg.pid")).await.trim().parse().unwrap();

    let restarted = open(tmp.path()).await;
    assert!(matches!(
        restarted.reap("cli-1-0").await,
        Err(Error::Unavailable)
    ));
    let still_alive = std::process::Command::new("/bin/kill")
        .args(["-s", "0", &bg.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(
        still_alive,
        "process belonging to a different owner with a shared prefix must stay alive"
    );

    Box::new(process.handle).terminate().await.unwrap();
    restarted.reap("cli-1-0-extra").await.unwrap();
    assert!(matches!(
        restarted.reap("cli-1-0").await,
        Err(Error::Unavailable)
    ));

    // Malformed tombstone nonces must not satisfy reap.
    let meta = tmp.path().join("work/.srt");
    let hash = owner_prefix("cli-1-0");
    for bogus in [
        format!("{hash}-abcd.done"),
        format!("{hash}-{}.done", "A".repeat(32)),
        format!("{hash}-{}-extra.done", "0".repeat(32)),
    ] {
        std::fs::write(meta.join(&bogus), "done\n").unwrap();
    }
    assert!(matches!(
        restarted.reap("cli-1-0").await,
        Err(Error::Unavailable)
    ));
}
