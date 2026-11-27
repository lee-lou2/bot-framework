//! Test helper for writing executable scripts without Linux parallel-fork `ETXTBSY` races.
//!
//! When parallel test threads `fork()` while another thread holds a newly written script open for
//! writing, the child briefly inherits the write fd and triggers `ETXTBSY` even after the writer
//! closes it. `write_executable` probes `--version` until `ETXTBSY` clears before returning.

use std::{
    io::ErrorKind,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

const SETTLE: Duration = Duration::from_secs(10);

/// Writes `text` to `path` with `mode` and waits until the executable can be spawned without `ETXTBSY`.
pub fn write_executable(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    settle(path);
}

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
                "{}: failed to execute written script: {error}",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic Linux reproducer: macOS permits executing a file while a write fd is open.
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
            "file open for writing must not execute"
        );
        let release = std::thread::spawn(move || {
            sleep(Duration::from_millis(200));
            drop(writer);
        });
        let started = Instant::now();
        settle(&path);
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "must not return while still open for writing: {:?}",
            started.elapsed()
        );
        release.join().unwrap();
        assert!(Command::new(&path).status().unwrap().success());
    }

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
                .unwrap_or_else(|error| panic!("script #{n}: {error}"));
            assert_eq!(status.code(), Some(7), "{n}");
        }
        stop.store(true, Ordering::Relaxed);
        for forker in forkers {
            forker.join().unwrap();
        }
    }
}
