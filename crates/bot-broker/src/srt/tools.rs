//! Host process-group management (`kill`, `pgrep`, `ps`) for sandboxed tool teardown.
//!
//! A group whose remaining members are all zombies (`Z`) counts as `Gone`; probe failures return `Unknown`, never `Gone`.

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

    /// Probes `pgid` via `pgrep -g` and `ps`: returns `Gone` on exit 1 or when all listed PIDs are zombies (`Z`)
    /// (Linux `pgrep` includes un-reaped zombies), `Alive` if any non-zombie remains, and `Unknown` on probe error.
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

    /// Checks whether every PID listed by `pgrep` is a zombie; unparseable output keeps `Alive`.
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
            // Exit 1 with empty stdout means all listed PIDs exited before `ps` inspected them.
            (Some(1), true) => Group::Gone,
            (Some(0 | 1), false) if states.iter().any(|state| !state.starts_with('Z')) => {
                Group::Alive
            }
            (Some(0 | 1), false) => Group::Gone,
            _ => Group::Unknown,
        }
    }

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
