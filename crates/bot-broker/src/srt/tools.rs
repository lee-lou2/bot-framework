//! 호스트 도구(`kill`·`pgrep`·`ps`)로 프로세스 그룹을 다룬다. 좀비(죽었지만 부모가 아직 거두지 않은 프로세스)만 남은 그룹은 사라진 것이고, 조회가 실패하면
//! "사라짐"이 아니라 "모름"이다.

use super::HostTools;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{process::Command, time::sleep};

pub(super) type Tools = HostTools;

pub(super) fn find_tool(candidates: &[&str]) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// 프로세스 그룹 조회의 세 가지 답. 조회 자체가 실패하면 `Unknown`이다(사라졌다고 보지 않는다).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Group {
    Alive,
    Gone,
    Unknown,
}

pub(super) fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub(super) fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

impl Tools {
    pub(super) async fn kill_group(&self, pgid: u32) {
        let _ = Command::new(&self.kill)
            .args(["-s", "KILL", "--", &format!("-{pgid}")])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }

    /// 그룹 조회: `pgrep -g`가 아무도 없다(종료 코드 1)고 하거나, 있다는 구성원이 **모두 좀비**이면 사라짐이다. 좀비(상태 `Z`)는 더는 코드를 돌리지 않고
    /// 부모가 거두기를 기다릴 뿐이다(운영에서도 부모가 아직 거두지 않은 순간이 있고, Linux의 `pgrep`은 좀비도 센다). 좀비가 아닌 구성원이 하나라도 있으면
    /// 있다. 조회 자체가 실패하면(실행 실패·그 밖의 종료 코드·읽을 수 없는 답) 사라짐이 아니라 모름이다.
    pub(super) async fn probe(&self, pgid: u32) -> Group {
        let listed = Command::new(&self.pgrep)
            .args(["-g", &pgid.to_string()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;
        let Ok(listed) = listed else {
            return Group::Unknown;
        };
        match listed.status.code() {
            Some(1) => Group::Gone,
            Some(0) => self.living(&String::from_utf8_lossy(&listed.stdout)).await,
            _ => Group::Unknown,
        }
    }

    /// `pgrep`이 있다고 한 구성원(줄마다 pid)이 좀비뿐인가. 구성원 목록을 읽지 못하면 «있다»는 답을 그대로 믿는다(사라짐으로 바꾸지 않는다).
    async fn living(&self, listed: &str) -> Group {
        let pids: Vec<&str> = listed
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        if pids.is_empty() {
            return Group::Alive;
        }
        let states = Command::new(&self.ps)
            .args(["-o", "stat=", "-p", &pids.join(",")])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await;
        let Ok(output) = states else {
            return Group::Unknown;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let states: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        match (output.status.code(), states.is_empty()) {
            // 종료 코드 1·빈 답: 그사이 구성원이 모두 사라졌다
            (Some(1), true) => Group::Gone,
            (Some(0 | 1), false) if states.iter().any(|state| !state.starts_with('Z')) => {
                Group::Alive
            }
            (Some(0 | 1), false) => Group::Gone,
            _ => Group::Unknown,
        }
    }

    /// 사라질 때까지 기다린다. 끝내 `Gone`이 확인되지 않으면 false다.
    pub(super) async fn gone_within(&self, pgid: u32, wait: Duration) -> bool {
        let started = tokio::time::Instant::now();
        loop {
            if self.probe(pgid).await == Group::Gone {
                return true;
            }
            if started.elapsed() >= wait {
                return false;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    /// 그룹 선두의 명령줄. 조회가 실패하면 `None`이다.
    pub(super) async fn leader_command(&self, pgid: u32) -> Option<String> {
        let output = Command::new(&self.ps)
            .args(["-o", "command=", "-p", &pgid.to_string()])
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        (output.status.success() || output.stdout.is_empty())
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }
}
