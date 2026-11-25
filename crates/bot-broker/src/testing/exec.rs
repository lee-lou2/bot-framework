//! 시험용 실행 파일 쓰기.
//!
//! 스크립트를 막 쓰고 곧바로 실행하면 가끔 `ETXTBSY`(`Text file busy`)로 실패한다. 시험은 한 프로세스 안의 여러 스레드에서 병렬로 도는데, 다른 스레드가 그 사이
//! 프로세스를 띄우면(fork) 자식이 우리가 쓰려고 열어 둔 파일 기술자를 exec 전까지 물려받는다. 그동안 그 파일은 «쓰는 중»이라 실행할 수 없고, 쓴 스레드는 이미 파일을
//! 닫았는데도 실행이 거절된다(Linux 실측: 스레드를 많이 준 `srt` 시험 60번 중 14번 `Os { code: 26, kind: ExecutableFileBusy }`; CI의 한 시험도 이래서 `open`이
//! `Unavailable`로 실패했다). 파일을 닫은 뒤에는 새로 쓰는 사람이 없으니, 물려받은 복사본이 모두 닫히는 순간 이 경쟁은 영원히 끝난다. 그래서 쓴 직후 **실행에 성공할
//! 때까지 기다린 뒤에만** 돌아온다.
//!
//! 실행 파일을 시험에서 쓰고 곧바로 `Command`로 부르는 곳은 모두 이 함수로 만든다(다른 크레이트의 시험도 `testing` 기능으로 쓸 수 있다).

use std::{
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

/// 기다리는 최대 시간(물려받은 복사본은 자식이 exec하면 곧 닫히므로 보통 한두 번 쉬면 된다).
const SETTLE: Duration = Duration::from_secs(10);

/// 스크립트 `text`를 `path`에 쓰고 권한 `mode`(0o755·0o700 …)를 준다. 돌아올 때는 **이미 실행할 수 있다**(`ETXTBSY`가 다시 나지 않는다).
pub fn write_executable(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    settle(path);
}

/// 실행에 성공할 때까지 기다린다. 확인을 위해 한 번 실행해 본다: 인자 `--version`으로, 출력은 버리고 종료 코드는 보지 않는다(가짜 도구는 인자를 무시하거나 판본만 말한다).
/// `ETXTBSY`만 다시 해 본다(그 밖의 실패는 파일이 잘못된 것이라 기다려도 소용없다).
fn settle(path: &Path) {
    let deadline = Instant::now() + SETTLE;
    loop {
        match Command::new(path)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                let _ = child.wait();
                return;
            }
            Err(error)
                if error.kind() == ErrorKind::ExecutableFileBusy && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(2));
            }
            Err(error) => panic!(
                "{}: 쓴 실행 파일을 실행하지 못했다: {error}",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 결정적 재현(Linux만: macOS는 쓰는 중인 파일도 실행을 허락해 이 경쟁이 없다): 쓰는 쪽이 파일을 열어 둔 동안(다른 스레드의 fork가 물려받은 쓰기 기술자와 같은 상태)은 실행할 수 없고, `settle`은 그동안 돌아오지 않다가 놓는 순간 돌아온다.
    #[cfg(target_os = "linux")]
    #[test]
    fn settling_waits_while_someone_still_holds_the_file_open_for_writing() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held");
        let mut writer = std::fs::File::create(&path).unwrap();
        writer.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let busy = Command::new(&path).spawn().unwrap_err();
        assert_eq!(
            busy.kind(),
            ErrorKind::ExecutableFileBusy,
            "쓰는 중인 파일은 실행할 수 없다"
        );
        let release = std::thread::spawn(move || {
            sleep(Duration::from_millis(200));
            drop(writer);
        });
        let started = Instant::now();
        settle(&path);
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "쓰는 중에는 돌아오면 안 된다: {:?}",
            started.elapsed()
        );
        release.join().unwrap();
        assert!(Command::new(&path).status().unwrap().success());
    }

    /// `ETXTBSY`가 아닌 실패는 기다리지 않고 바로 드러난다.
    #[test]
    fn a_file_that_cannot_run_for_another_reason_fails_loudly_instead_of_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no-interpreter");
        let started = Instant::now();
        let result = std::panic::catch_unwind(|| {
            write_executable(&path, "#!/no/such/interpreter\n", 0o755);
        });
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// 실제 현상: 다른 스레드들이 쉬지 않고 프로세스를 띄우는 동안 스크립트를 쓰고 곧바로 실행해도 재시도 없이 한 번도 실패하지 않는다(Linux에서는 fork가 싸서 금방 끝난다).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_script_written_while_other_threads_keep_forking_runs_at_once() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let stop = Arc::new(AtomicBool::new(false));
        let forkers: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = Command::new("/bin/sh").args(["-c", "exit 0"]).status();
                    }
                })
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        for n in 0..100 {
            let path = dir.path().join(format!("script-{n}"));
            write_executable(&path, "#!/bin/sh\nexit 7\n", 0o755);
            let status = Command::new(&path)
                .status()
                .unwrap_or_else(|error| panic!("{n}번째 스크립트: {error}"));
            assert_eq!(status.code(), Some(7), "{n}");
        }
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            forker.join().unwrap();
        }
    }
}
