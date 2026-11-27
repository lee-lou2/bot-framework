//! Short retry loop for non-blocking `flock` acquisitions to absorb transient fork-before-exec fd inheritance.
use std::{fs::TryLockError, time::Duration};

// Child processes inherit open fds between `fork` and `exec`, briefly holding a just-released `flock`.
// Retrying for `WAIT` avoids false contention while genuine holders remain blocked.
const WAIT: Duration = Duration::from_millis(400);
const STEP: Duration = Duration::from_millis(25);

pub fn retry(mut attempt: impl FnMut() -> Result<(), TryLockError>) -> Result<(), TryLockError> {
    let mut waited = Duration::ZERO;
    loop {
        match attempt() {
            Err(TryLockError::WouldBlock) if waited < WAIT => {
                std::thread::sleep(STEP);
                waited += STEP;
            }
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn open(path: &std::path::Path) -> std::fs::File {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_lock_released_shortly_after_is_taken_but_a_held_one_stays_blocked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let holder = open(&path);
        holder.try_lock().unwrap();
        let other = open(&path);
        assert!(matches!(
            retry(|| other.try_lock()),
            Err(TryLockError::WouldBlock)
        ));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        assert!(retry(|| other.try_lock()).is_ok());
        release.join().unwrap();
    }
}
