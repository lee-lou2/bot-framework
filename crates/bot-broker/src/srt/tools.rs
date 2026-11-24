//! 호스트 도구(`kill`·`pgrep`·`ps`)로 프로세스 그룹을 다룬다. 조회가 실패하면 "사라짐"이 아니라 "모름"이다.

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

    /// `pgrep -g`: 0이면 있고, 1이면 없고, 그 밖(실행 실패·다른 종료 코드)은 모름이다.
    pub(super) async fn probe(&self, pgid: u32) -> Group {
        let status = Command::new(&self.pgrep)
            .args(["-g", &pgid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        match status.ok().and_then(|s| s.code()) {
            Some(0) => Group::Alive,
            Some(1) => Group::Gone,
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
