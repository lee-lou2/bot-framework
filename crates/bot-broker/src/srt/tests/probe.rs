//! 프로세스 그룹 조회(`Tools::probe`): 좀비(죽었지만 부모가 아직 거두지 않은 프로세스)만 남은 그룹은 사라짐이고, 좀비가 아닌 구성원이 하나라도 있으면 있다.
//! 조회가 실패하면 사라짐이 아니라 모름이다. 진짜 프로세스로는 Linux(`pgrep`이 좀비도 센다)와 macOS에서 같은 답이어야 한다.

use super::*;
use crate::srt::tools::Group;

/// 정해진 줄을 찍고 정해진 코드로 끝나는 가짜 도구.
fn fake_out(dir: &Path, name: &str, code: i32, out: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!("#!/bin/sh\ncat <<'EOF'\n{out}EOF\nexit {code}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

async fn probe_with(pgrep: (i32, &str), ps: (i32, &str)) -> Group {
    let tmp = tempfile::tempdir().unwrap();
    let tools = HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: fake_out(tmp.path(), "pgrep", pgrep.0, pgrep.1),
        ps: fake_out(tmp.path(), "ps", ps.0, ps.1),
    };
    tools.probe(4242).await
}

#[tokio::test]
async fn a_group_is_gone_when_nobody_is_listed_or_every_listed_member_is_a_zombie() {
    // `pgrep`이 아무도 없다고 한다
    assert_eq!(probe_with((1, ""), (9, "")).await, Group::Gone);
    // 있다고 한 구성원이 모두 좀비다(`Z`·`Zs`·`Z+`)
    assert_eq!(
        probe_with((0, "10\n11\n12\n"), (0, "Z\nZs\nZ+\n")).await,
        Group::Gone
    );
    // 목록을 읽는 사이 구성원이 모두 사라졌다(`ps -p`는 아무도 못 찾으면 1이다)
    assert_eq!(probe_with((0, "10\n"), (1, "")).await, Group::Gone);
}

#[tokio::test]
async fn one_member_that_is_not_a_zombie_keeps_the_group_alive() {
    for states in ["Z\nS\n", "S\nZ\n", "R+\n", "Ss\nZs\nZ\n", "D\n", "T\nZ\n"] {
        assert_eq!(
            probe_with((0, "10\n11\n"), (0, states)).await,
            Group::Alive,
            "{states:?}"
        );
    }
}

#[tokio::test]
async fn an_unreadable_member_list_is_trusted_as_alive_and_a_failed_lookup_is_unknown_never_gone() {
    // 있다는 답은 맞는데 구성원 목록을 읽지 못하면 있다고 본다(사라짐으로 바꾸지 않는다)
    for listed in ["", "\n", "not-a-pid\n", "12 13\n"] {
        assert_eq!(
            probe_with((0, listed), (1, "")).await,
            Group::Alive,
            "{listed:?}"
        );
    }
    // `pgrep`이 알 수 없는 코드로 끝난다
    for code in [2, 3, 127] {
        assert_eq!(probe_with((code, "10\n"), (0, "S\n")).await, Group::Unknown);
    }
    // `ps`가 실패한다: 알 수 없는 코드, 정상 코드인데 빈 답, 실패 코드인데 답이 있다
    for ps in [(2, ""), (3, "Z\n"), (0, ""), (0, "\n\n")] {
        assert_eq!(probe_with((0, "10\n"), ps).await, Group::Unknown, "{ps:?}");
    }
}

#[tokio::test]
async fn a_tool_that_cannot_even_start_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let missing = tmp.path().join("absent");
    let working = fake_out(tmp.path(), "ok", 0, "10\n");
    let tools = HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: missing.clone(),
        ps: working.clone(),
    };
    assert_eq!(tools.probe(1).await, Group::Unknown);
    let tools = HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: working,
        ps: missing,
    };
    assert_eq!(tools.probe(1).await, Group::Unknown);
}

fn real_tools() -> HostTools {
    HostTools {
        kill: PathBuf::from("/bin/kill"),
        pgrep: PathBuf::from("/usr/bin/pgrep"),
        ps: PathBuf::from("/bin/ps"),
    }
}

/// 조건이 될 때까지 기다린다(시간 추정 대신 짧게 다시 본다).
async fn until(what: &str, mut ready: impl AsyncFnMut() -> bool) {
    for _ in 0..200 {
        if ready().await {
            return;
        }
        sleep(Duration::from_millis(25)).await;
    }
    panic!("{what}");
}

#[tokio::test]
async fn a_dead_leader_nobody_has_reaped_yet_does_not_keep_its_group_alive() {
    let tools = real_tools();
    // 바로 끝나는 프로세스: 우리가 `wait`하기 전까지 죽었지만 거두지 않은(좀비) 그룹 선두다
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    until(
        "좀비만 남은 그룹이 사라짐으로 보이지 않았다",
        async || tools.probe(pgid).await == Group::Gone,
    )
    .await;
    assert!(tools.gone_within(pgid, Duration::from_secs(2)).await);
    leader.wait().unwrap();
}

#[tokio::test]
async fn a_live_member_keeps_the_group_alive_even_though_its_leader_is_a_zombie() {
    let tools = real_tools();
    // 선두는 곧 끝나 좀비가 되지만 백그라운드 `sleep`이 같은 그룹에 남는다
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 60 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    // 선두가 좀비가 될 때까지 기다린다
    until("선두가 끝나지 않았다", async || {
        let output = tokio::process::Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", &pgid.to_string()])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .starts_with('Z')
    })
    .await;
    assert_eq!(tools.probe(pgid).await, Group::Alive);
    // 그룹을 끝내면 모두 사라진다(남은 `sleep`이 아직 거두어지지 않았더라도 좀비일 뿐이다)
    tools.kill_group(pgid).await;
    assert!(tools.gone_within(pgid, Duration::from_secs(5)).await);
    leader.wait().unwrap();
}
