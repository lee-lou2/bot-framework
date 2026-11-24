//! 인스턴스 소유권 잠금: 같은 인스턴스 이름의 어댑터는 이 호스트 사용자에게 하나뿐이다.
//!
//! 잠금 파일의 위치는 **호스트 사용자 전용 고정 경로**(`~/.local/state/bot-framework/locks/`, 0700)이고 작업 폴더·마운트 루트 같은 설정과 무관하다:
//! 이름이 같으면 어떤 설정으로 시작하든 같은 파일을 잠근다. (설정마다 다른 폴더에 잠금을 두면 같은 인스턴스를 둘이 시작해 한쪽이 다른 쪽의 살아 있는 닻과
//! VM을 «고아»로 오인해 지운다.) 잠금은 시작 맨 앞(VM을 하나도 건드리기 전)에 잡는다. 이미 잡혀 있으면 `instance_in_use`로 끝난다.
//!
//! - 폴더는 사용자만 쓴다: 심볼릭 링크가 아닌 진짜 폴더이고 그룹·다른 사용자 권한이 없으며 잠금 파일과 소유자가 같다. 아니면 시작하지 않는다.
//! - 잠근 파일이 지금 경로의 파일이어야 한다(잠근 뒤 경로를 다시 본다). 어댑터가 사는 동안 모델 VM을 띄울 때마다(`Identity::holds`) 같은 파일인지 다시 본다:
//!   누가 지웠거나 바꿨다면 소유를 보증할 수 없으니 모델 실행을 거절한다(닫힌 채 실패).
//! - 프로세스가 죽으면 OS가 잠금을 푼다. 그래서 같은 인스턴스의 다른 실행(incarnation)이 남긴 VM은 죽은 프로세스의 고아다.
//!
//! 시험만 폴더를 바꾼다(`ContainerConfig::lock_dir`): 운영은 비워 둔다.

use super::{ContainerConfig, StartError};
use rustix::{
    fs::{FlockOperation, OFlags, flock},
    io::Errno,
};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::time::sleep;

/// 잠금을 다시 해 보는 횟수와 간격(합쳐 0.5초): 같은 프로세스가 방금 `fork`한 자식은 `exec`로 파일 기술자를 닫기 전까지 (이미 닫은) 잠금을 잠깐 쥐고 있을 수 있다.
/// 진짜 다른 소유자는 이 사이에 사라지지 않는다.
const LOCK_TRIES: u32 = 25;
const LOCK_PAUSE: Duration = Duration::from_millis(20);
/// 잠근 뒤 경로가 다른 파일을 가리키면(지워졌다 다시 만들어졌다) 처음부터 다시 연다.
const REOPEN_TRIES: u32 = 5;

fn failed() -> StartError {
    StartError("instance_lock_failed")
}

/// 쥐고 있는 소유권. 파일을 닫으면(또는 프로세스가 죽으면) 풀린다.
pub(super) struct InstanceLock {
    _file: File,
    identity: Identity,
}

impl InstanceLock {
    pub(super) fn identity(&self) -> Identity {
        self.identity.clone()
    }
}

/// 잠근 파일의 신원: 경로가 아직 그 파일을 가리키는가.
#[derive(Clone, Debug)]
pub(super) struct Identity {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Identity {
    pub(super) fn holds(&self) -> bool {
        fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.is_file() && m.dev() == self.dev && m.ino() == self.ino)
    }
}

/// 잠금 폴더: 설정이 정했으면(시험) 그것, 아니면 호스트 사용자 전용 고정 경로다. 홈을 알 수 없으면 정할 수 없다(시작하지 않는다).
pub(super) fn lock_dir(config: &ContainerConfig) -> Result<PathBuf, StartError> {
    match &config.lock_dir {
        Some(dir) => Ok(dir.clone()),
        None => default_lock_dir(std::env::var_os("HOME").map(PathBuf::from)),
    }
}

/// 호스트 사용자 전용 고정 경로. 절대 경로인 홈에서만 정한다.
pub(super) fn default_lock_dir(home: Option<PathBuf>) -> Result<PathBuf, StartError> {
    let home = home.filter(|h| h.is_absolute()).ok_or_else(failed)?;
    Ok(home.join(".local/state/bot-framework/locks"))
}

/// 잠금 파일 경로(폴더와 인스턴스 이름만으로 정해진다).
pub(super) fn lock_path(dir: &Path, instance: &str) -> PathBuf {
    dir.join(format!(".botvm-{instance}.lock"))
}

/// 폴더를 0700으로 만들고(없을 때) 사용자만 쓰는 진짜 폴더인지 본다. 폴더의 메타데이터를 돌려준다.
fn private_dir(dir: &Path) -> Result<fs::Metadata, StartError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|_| failed())?;
    let meta = fs::symlink_metadata(dir).map_err(|_| failed())?;
    if !meta.is_dir() || meta.mode() & 0o077 != 0 {
        return Err(failed());
    }
    Ok(meta)
}

pub(super) async fn lock_instance(config: &ContainerConfig) -> Result<InstanceLock, StartError> {
    let dir = lock_dir(config)?;
    let dir_meta = private_dir(&dir)?;
    let path = lock_path(&dir, &config.instance);
    for _ in 0..REOPEN_TRIES {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(OFlags::NOFOLLOW.bits() as i32)
            .open(&path)
            .map_err(|_| failed())?;
        let mut tries = 0;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(Errno::WOULDBLOCK) if tries < LOCK_TRIES => {
                    tries += 1;
                    sleep(LOCK_PAUSE).await;
                }
                Err(_) => return Err(StartError("instance_in_use")),
            }
        }
        // 잠근 파일이 지금 경로의 파일이고 폴더와 같은 소유자의 것이어야 한다(그사이 지워졌다 다시 만들어졌다면 처음부터 다시 연다)
        let held = file.metadata().map_err(|_| failed())?;
        let named = fs::symlink_metadata(&path).map_err(|_| failed())?;
        if !named.is_file() || held.dev() != named.dev() || held.ino() != named.ino() {
            continue;
        }
        if held.uid() != dir_meta.uid() {
            return Err(failed());
        }
        let identity = Identity {
            path,
            dev: held.dev(),
            ino: held.ino(),
        };
        return Ok(InstanceLock {
            _file: file,
            identity,
        });
    }
    Err(failed())
}
