//! 파일 잠금(`flock`) 보조. 잠금은 «열린 파일»에 걸리는데 `fork`한 자식은 `exec`하기 전까지 부모의 열린 파일을 같이 쥔다.
//! 여러 스레드·여러 역할이 동시에 프로세스를 띄우면(병렬 시험·`/usr/bin/id` 호출·srt 실행) 방금 놓은 잠금이 자식의 `exec`가 끝날
//! 때까지 잠깐 풀리지 않아, 곧바로 다시 잡으려는 시도가 일시적으로 막힌다. 이를 «다른 프로세스가 쓰는 중»으로 오인하지 않도록
//! 아주 짧게(총 `WAIT`) 다시 시도한다 — 진짜로 쥐고 있는 쪽이 있으면 이 시간이 지나도 풀리지 않아 그대로 막힌다.
use std::{fs::TryLockError, time::Duration};

const WAIT: Duration = Duration::from_millis(400);
const STEP: Duration = Duration::from_millis(25);

/// 잠금 시도를 `WouldBlock`인 동안 잠깐 되풀이한다. 다른 오류는 바로 돌려준다.
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
        // 진짜로 쥐고 있으면 대기가 끝나도 막힌다
        assert!(matches!(
            retry(|| other.try_lock()),
            Err(TryLockError::WouldBlock)
        ));
        // 잠깐 뒤 풀리면(자식이 exec하며 놓는 것처럼) 다시 시도해 잡는다
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            drop(holder);
        });
        assert!(retry(|| other.try_lock()).is_ok());
        release.join().unwrap();
    }
}
