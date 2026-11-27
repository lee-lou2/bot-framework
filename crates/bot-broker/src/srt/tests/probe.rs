//! Process-group probing tests (`Tools::probe`), including zombie-only groups and lookup failures.

use super::*;
use crate::{srt::tools::Group, testing::write_executable};

fn fake_out(dir: &Path, name: &str, code: i32, out: &str) -> PathBuf {
    let path = dir.join(name);
    write_executable(
        &path,
        &format!("#!/bin/sh\ncat <<'EOF'\n{out}EOF\nexit {code}\n"),
        0o755,
    );
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
    assert_eq!(probe_with((1, ""), (9, "")).await, Group::Gone);
    assert_eq!(
        probe_with((0, "10\n11\n12\n"), (0, "Z\nZs\nZ+\n")).await,
        Group::Gone
    );
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
    for listed in ["", "\n", "not-a-pid\n", "12 13\n"] {
        assert_eq!(
            probe_with((0, listed), (1, "")).await,
            Group::Alive,
            "{listed:?}"
        );
    }
    for code in [2, 3, 127] {
        assert_eq!(probe_with((code, "10\n"), (0, "S\n")).await, Group::Unknown);
    }
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
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    until("zombie-only group was not reported as gone", async || {
        tools.probe(pgid).await == Group::Gone
    })
    .await;
    assert!(tools.gone_within(pgid, Duration::from_secs(2)).await);
    leader.wait().unwrap();
}

#[tokio::test]
async fn a_live_member_keeps_the_group_alive_even_though_its_leader_is_a_zombie() {
    let tools = real_tools();
    let mut leader = std::process::Command::new("/bin/sh")
        .args(["-c", "sleep 60 & exit 0"])
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = leader.id();
    until("group leader did not exit", async || {
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
    tools.kill_group(pgid).await;
    assert!(tools.gone_within(pgid, Duration::from_secs(5)).await);
    leader.wait().unwrap();
}
