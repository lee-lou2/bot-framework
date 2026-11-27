//! 종료 증명: 표식, 기록 없음·깨진 기록, 조회 실패, 재시작 뒤 정리.

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
    // 브로커가 다시 시작했다: 새 어댑터는 기록을 모르고 디스크의 pid 기록으로만 찾는다
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
    // 같은 소유자를 다시 확인하면 표식으로 증명된다. 기록이 하나도 없는 소유자는 증명할 수 없다.
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
    // pid가 재사용된 상황: 기록한 그룹 선두가 우리 srt가 아니라 다른 프로세스다
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
        "확인할 수 없으면 건드리지 않고 보류를 유지한다"
    );
    assert!(
        stranger.try_wait().unwrap().is_none(),
        "남의 프로세스는 살아 있다"
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
    // 브로커가 결과를 적기 전에 죽었다가 다시 시작해 같은 소유자를 확인해도 종료가 증명된다
    let restarted = open(tmp.path()).await;
    restarted.reap("r1.1.c1").await.unwrap();
    restarted.reap("r1.1.c1").await.unwrap();
}

#[tokio::test]
async fn reap_refuses_to_vouch_without_a_record_or_with_a_broken_one() {
    let tmp = tempfile::tempdir().unwrap();
    let sandbox = open(tmp.path()).await;
    let meta = tmp.path().join("work/.srt");
    // 이 소유자의 기록이 하나도 없다(다른 호스트이거나 지워졌다): 종료를 증명할 수 없다
    assert!(matches!(
        sandbox.reap("r9.9.c9").await,
        Err(Error::Unavailable)
    ));
    // 띄우다 죽은 기록(그룹 번호가 없다), 깨진 기록
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
    // 메타 폴더를 읽지 못한다
    std::fs::remove_dir_all(&meta).unwrap();
    assert!(matches!(
        sandbox.reap("r1.1.c1").await,
        Err(Error::Unavailable)
    ));
}

/// `pgrep`을 대신하는 가짜: 종료 코드만 돌려준다.

#[tokio::test]
async fn a_failing_group_lookup_is_unknown_not_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let binary = fake_srt(tmp.path(), PINNED_VERSION);
    let real = HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: PathBuf::from("/usr/bin/pgrep"),
        ps: PathBuf::from("/bin/ps"),
    };
    // pgrep이 알 수 없는 오류(종료 코드 3)로 끝난다: 사라졌다고 보지 않는다
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
        "조회가 실패하면 종료를 확인하지 못한 것이다"
    );
    assert!(matches!(
        sandbox.reap("r1.1.c1").await,
        Err(Error::Unavailable)
    ));
    // 종료 코드 1(그룹에 아무도 없음)만 사라짐이다
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

    // 다른 소유자(`cli-1-0`)의 reap은 `cli-1-0-extra`의 살아 있는 프로세스를 건드리지 않는다.
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
        "접두어가 같은 다른 소유자의 프로세스가 살아 있어야 한다"
    );

    // `cli-1-0-extra`를 정상 종료해 `.done` 표식을 남겨도 `cli-1-0`은 그 표식을 읽지 않는다.
    Box::new(process.handle).terminate().await.unwrap();
    restarted.reap("cli-1-0-extra").await.unwrap();
    assert!(matches!(
        restarted.reap("cli-1-0").await,
        Err(Error::Unavailable)
    ));

    // nonce 길이나 대소문자 모양이 틀린 위조 `.done` 파일도 표식으로 인정하지 않는다.
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
