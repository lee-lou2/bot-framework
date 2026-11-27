//! `ModelProcess` wrapper around `container run` with bounded line reads and verified VM teardown.
//!
//! Teardown kills the CLI client first, then loops `kill`/`delete --force` until `inspect` reports
//! absence continuously for `ABSENT_FOR`, preventing a late in-flight create from stranding a VM.

use bot_core::sandbox::{Exit, ExitFuture, LineFuture, ModelProcess, UnitFuture};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdout, Command},
    time::{Instant, timeout},
};

pub const MAX_LINE: usize = 8 << 20;
const COMMAND_WITHIN: Duration = Duration::from_secs(3);
const REMOVE_WITHIN: Duration = Duration::from_secs(4);
/// Require sustained absence because an in-flight create from a dying client can materialize late.
const ABSENT_FOR: Duration = Duration::from_millis(400);
const POLL: Duration = Duration::from_millis(100);

pub(super) fn budget(deadline: Instant, cap: Duration) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then(|| remaining.min(cap))
}

#[derive(Clone)]
struct Target {
    program: String,
    name: String,
}

enum Seen {
    Present,
    Absent,
    Unknown,
}

impl Target {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }

    async fn run(&self, args: &[&str], deadline: Instant) {
        let Some(within) = budget(deadline, COMMAND_WITHIN) else {
            return;
        };
        let mut command = self.command();
        command
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut child) = command.spawn() {
            let _ = timeout(within, child.wait()).await;
        }
    }

    async fn seen(&self, deadline: Instant) -> Seen {
        let Some(within) = budget(deadline, COMMAND_WITHIN) else {
            return Seen::Unknown;
        };
        let mut command = self.command();
        command
            .args(["inspect", &self.name])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        match timeout(within, command.output()).await {
            Ok(Ok(output)) if output.status.success() => Seen::Present,
            Ok(Ok(output)) if String::from_utf8_lossy(&output.stderr).contains("not found") => {
                Seen::Absent
            }
            _ => Seen::Unknown,
        }
    }

    async fn remove(&self) -> bool {
        self.remove_within(REMOVE_WITHIN).await
    }

    async fn remove_within(&self, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        self.run(&["kill", &self.name], deadline).await;
        self.run(&["delete", "--force", &self.name], deadline).await;
        let mut absent_since: Option<Instant> = None;
        loop {
            match self.seen(deadline).await {
                Seen::Present => {
                    absent_since = None;
                    self.run(&["kill", &self.name], deadline).await;
                    self.run(&["delete", "--force", &self.name], deadline).await;
                }
                Seen::Absent => {
                    let since = *absent_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= ABSENT_FOR {
                        return true;
                    }
                }
                Seen::Unknown => absent_since = None,
            }
            let Some(sleep_for) = budget(deadline, POLL) else {
                return false;
            };
            tokio::time::sleep(sleep_for).await;
        }
    }
}

pub struct ContainerProcess {
    child: Child,
    lines: BufReader<ChildStdout>,
    stderr: Arc<std::sync::Mutex<String>>,
    program: String,
    name: String,
    gone: bool,
}

impl ContainerProcess {
    pub(super) fn new(mut child: Child, program: String, name: String) -> Option<Self> {
        let stdout = child.stdout.take()?;
        let stderr = Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let sink = stderr.clone();
            tokio::spawn(async move {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut tail) = sink.lock() {
                        tail.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if tail.len() > 4096 {
                            let cut = (tail.len() - 2048..tail.len())
                                .find(|i| tail.is_char_boundary(*i))
                                .unwrap_or(0);
                            *tail = tail[cut..].to_owned();
                        }
                    }
                }
            });
        }
        Some(Self {
            child,
            lines: BufReader::new(stdout),
            stderr,
            program,
            name,
            gone: false,
        })
    }

    fn target(&self) -> Target {
        Target {
            program: self.program.clone(),
            name: self.name.clone(),
        }
    }

    async fn remove_container(&mut self) {
        self.gone = true;
        let _ = self.child.kill().await;
        self.target().remove().await;
    }
}

impl ModelProcess for ContainerProcess {
    fn next_line(&mut self) -> LineFuture<'_> {
        Box::pin(async move {
            let mut buffer = Vec::new();
            let read = (&mut self.lines)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut buffer)
                .await
                .ok()?;
            if read == 0 {
                return None;
            }
            if buffer.last() == Some(&b'\n') {
                buffer.pop();
                if buffer.last() == Some(&b'\r') {
                    buffer.pop();
                }
            } else if buffer.len() > MAX_LINE {
                self.remove_container().await;
                return None;
            }
            Some(String::from_utf8_lossy(&buffer).into_owned())
        })
    }

    fn kill(&mut self) -> UnitFuture<'_> {
        Box::pin(async move {
            self.remove_container().await;
        })
    }

    fn wait(&mut self) -> ExitFuture<'_> {
        Box::pin(async move {
            let status = self.child.wait().await.ok();
            self.gone = true;
            let tail = self.stderr.lock().map(|s| s.clone()).unwrap_or_default();
            Exit {
                code: status.and_then(|s| s.code()),
                stderr_tail: tail,
            }
        })
    }
}

impl Drop for ContainerProcess {
    fn drop(&mut self) {
        if self.gone {
            return;
        }
        let target = self.target();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    target.remove().await;
                });
            }
            Err(_) => {
                for args in [
                    vec!["kill", &target.name],
                    vec!["delete", "--force", &target.name],
                ] {
                    let _ = std::process::Command::new(&target.program)
                        .args(args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn budget_clamps_to_cap_and_returns_none_after_deadline() {
        let now = Instant::now();
        assert_eq!(
            budget(now + Duration::from_secs(10), Duration::from_secs(3)),
            Some(Duration::from_secs(3))
        );
        let short = budget(now + Duration::from_millis(200), Duration::from_secs(3)).unwrap();
        assert!(short <= Duration::from_millis(200), "{short:?}");
        assert_eq!(budget(now, Duration::from_secs(3)), None);
    }

    #[tokio::test]
    async fn remove_clamps_every_subcommand_to_the_shared_deadline() {
        let dir = std::env::temp_dir().join(format!(
            "bot-process-budget-{}-{}",
            std::process::id(),
            bot_core::random::random_hex(4)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("hang-container");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let target = Target {
            program: script.to_str().unwrap().to_owned(),
            name: "botvm-test-1".to_owned(),
        };
        let started = Instant::now();
        let removed = target.remove_within(Duration::from_millis(300)).await;
        let elapsed = started.elapsed();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!removed);
        assert!(
            elapsed < Duration::from_secs(2),
            "must complete within shared 300ms deadline: {elapsed:?}"
        );
    }
}
