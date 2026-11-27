//! `container run` 클라이언트 프로세스를 `ModelProcess`로 감싼다: 표준 출력은 줄 단위(줄 하나의 상한을 둔다), 종료는 클라이언트를 먼저 끝내고
//! 컨테이너가 사라진 것을 확인할 때까지 지우는 것이다.
//!
//! 왜 «클라이언트를 먼저, 사라진 것을 확인하며» 지우는가(실제 VM으로 확인): 뜨는 중인 컨테이너를 곧바로 `kill`·`delete --force`하면 아직 만들어지기 전이라 지울 것이 없고,
//! 그 사이 클라이언트가 보낸 «만들기» 요청이 컨테이너를 만들어 `stopped`로 남는다(`--rm`을 지켜 줄 클라이언트는 이미 죽었다). 그래서 클라이언트를 먼저 끝내
//! 더는 요청이 나가지 않게 하고, 컨테이너가 보이면 지우기를 되풀이해 «없음»이 한동안 이어질 때까지 본다.

use bot_core::sandbox::{Exit, ExitFuture, LineFuture, ModelProcess, UnitFuture};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::{Child, ChildStdout, Command},
    time::{Instant, timeout},
};

/// 출력 줄 하나의 상한. CLI의 JSONL 한 줄이 이보다 길면 읽기를 끝내고 컨테이너를 끝낸다(무한히 쌓지 않는다).
pub const MAX_LINE: usize = 8 << 20;
/// 명령 하나(`kill`·`delete`·`inspect`)를 기다리는 시간
const COMMAND_WITHIN: Duration = Duration::from_secs(3);
/// 컨테이너를 지우고 사라진 것을 확인하는 데 쓰는 전체 시간. 넘으면 확인하지 못한 채 돌아온다(종료 정리 단계의 `reap`이 한 번 더 본다).
const REMOVE_WITHIN: Duration = Duration::from_secs(4);
/// «없음»이 이만큼 이어져야 사라진 것으로 본다: 클라이언트가 죽기 직전에 보낸 «만들기» 요청이 늦게 컨테이너를 만들 수 있다.
const ABSENT_FOR: Duration = Duration::from_millis(400);
const POLL: Duration = Duration::from_millis(100);

/// 공유 마감(`deadline`)까지 남은 시간을 `cap`으로 자른다. 이미 마감이 지났으면 `None`.
pub(super) fn budget(deadline: Instant, cap: Duration) -> Option<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    (!remaining.is_zero()).then(|| remaining.min(cap))
}

/// 컨테이너 하나를 가리키는 값: 클라이언트 프로그램과 컨테이너 이름.
#[derive(Clone)]
struct Target {
    program: String,
    name: String,
}

/// `inspect`로 본 컨테이너 상태.
enum Seen {
    Present,
    Absent,
    /// 서비스가 응답하지 않는 등 있는지 없는지 모른다
    Unknown,
}

impl Target {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.stdin(Stdio::null()).kill_on_drop(true);
        command
    }

    /// 명령 하나를 돌려 끝나기를 기다린다(결과는 보지 않는다). 남은 마감이 없으면 띄우지 않는다.
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

    /// 컨테이너가 있는지 `inspect`로 본다: 성공이면 있고, «not found»면 없다. 그 밖(서비스 오류·시간 초과)은 모른다.
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

    /// 컨테이너를 끝내고 지운 뒤 «없음»이 `ABSENT_FOR` 동안 이어질 때까지 본다. 보이면 다시 지운다. 사라진 것을 확인했으면 `true`.
    /// 처음에는 `inspect`와 상관없이 한 번 끝내라고 한다(목록·조회가 늦어도 이미 떠 있는 컨테이너는 바로 멈춘다). 뜨는 중이라 `kill`이 거절돼도 `delete --force`가 멈추고 지운다.
    async fn remove(&self) -> bool {
        self.remove_within(REMOVE_WITHIN).await
    }

    /// 주어진 전체 마감(`within`) 안에서 지우고 확인한다. 각 하위 명령과 대기의 한도를 남은 마감에 맞춰 잘라 전체 시간을 넘기지 않는다.
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
    /// 컨테이너가 이미 끝났거나 끝내라고 했다
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

    /// 클라이언트를 먼저 끝내(더는 만들라는 요청이 나가지 않는다) 컨테이너가 사라진 것을 확인할 때까지 지운다.
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
                // 개행 없이 상한을 넘었다: 더 읽지 않고 컨테이너를 끝낸다(마지막 줄이 짧게 끝난 정상 종료는 아래로 간다)
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
    /// 끝났다는 확인 없이 놓이면(취소·패닉) 컨테이너가 남지 않게 최선으로 지운다: 클라이언트는 이 값이 놓이며 죽고(`kill_on_drop`), 지우기는 런타임에서 따로 돈다.
    /// 런타임이 없으면 끝내라는 명령만 보낸다.
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
        // kill·delete·inspect 모두 30초 멈춘다: 공유 마감 없이 COMMAND_WITHIN(3초)을 그대로 쓰면 9초 이상 걸린다
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
            "공유 마감(300ms) 안에 끝나야 한다: {elapsed:?}"
        );
    }
}
