//! 설치본을 놓는 부분: 새 복사본을 `<기준>/<판본>/`에 놓거나 이미 있는 설치본을 다시 쓰고, 어긋난 설치본은 쓰는 프로세스가 없을 때만
//! 치우고 새로 설치한다(판본별 사용 잠금).
use super::{
    Installed, ServerSpec, revision_of,
    walk::{io, walk},
};
use bot_core::id::Revision;
use std::{
    fs::{DirBuilder, File, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{Duration, SystemTime},
};

/// 중단된 설치가 남긴 임시 폴더를 치우는 기준
const STALE_STAGING: Duration = Duration::from_secs(3600);

/// 폴더 아래 모든 폴더에 쓰기 권한을 돌려준 뒤 지운다(설치본은 읽기 전용이라 그대로는 지워지지 않는다).
pub(super) fn remove_tree(path: &Path) {
    fn unlock(path: &Path) {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return;
        };
        if meta.is_dir() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
            if let Ok(children) = std::fs::read_dir(path) {
                for child in children.flatten() {
                    unlock(&child.path());
                }
            }
        }
    }
    unlock(path);
    let _ = std::fs::remove_dir_all(path);
}

/// 설치 기준 폴더: 본인 소유의 비공개(0700) 폴더. 없으면 만든다. 중단된 설치의 임시 폴더는 치운다.
pub(super) fn base_dir(base: &Path, uid: u32) -> Result<(), String> {
    if let Some(parent) = base.parent() {
        std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
    }
    if std::fs::symlink_metadata(base).is_err() {
        DirBuilder::new()
            .mode(0o700)
            .create(base)
            .map_err(|e| io(base, e))?;
    }
    let meta = std::fs::symlink_metadata(base).map_err(|e| io(base, e))?;
    if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{}: 설치 기준 폴더는 본인 소유이고 소유자만 접근할 수 있는(0700) 폴더여야 해요",
            base.display()
        ));
    }
    if let Ok(children) = std::fs::read_dir(base) {
        for child in children.flatten() {
            let name = child.file_name().to_string_lossy().into_owned();
            let stale = (name.starts_with(".staging-") || name.starts_with(".stale-"))
                && child
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|at| SystemTime::now().duration_since(at).ok())
                    .is_some_and(|age| age > STALE_STAGING);
            if stale {
                remove_tree(&child.path());
            }
        }
    }
    Ok(())
}

/// 이미 있는 설치본을 다시 써도 되는가: 설치본의 바이트로 계산한 판본이 같고, 루트·모든 폴더·파일이 이 사용자 소유이며
/// 누구도 쓸 수 없다.
fn reusable(alias: &str, spec: &ServerSpec, dir: &Path, revision: &Revision, uid: u32) -> bool {
    walk(dir, None, Some(uid))
        .ok()
        .and_then(|(entries, _)| revision_of(alias, spec, &entries).ok())
        .is_some_and(|found| found == *revision)
}

/// 어긋난 설치본(`dir`)을 옆으로 치우고 새 복사본(`staging`)을 그 자리에 둔다. 치운 것은 지워 보고, 못 지우면 둔다(오래되면 다음 시작이 치운다).
fn replace(dir: &Path, staging: &Path, base: &Path) -> Result<(), String> {
    let aside = base.join(format!(".stale-{}", crate::nonce::hex()));
    if let Err(error) = std::fs::rename(dir, &aside) {
        remove_tree(staging);
        return Err(io(dir, error));
    }
    if let Err(error) = std::fs::rename(staging, dir) {
        remove_tree(staging);
        return Err(io(dir, error));
    }
    remove_tree(&aside);
    if aside.exists() {
        tracing::warn!(path = %aside.display(), "어긋난 설치본을 지우지 못했어요 — 수동으로 지워 주세요");
    }
    Ok(())
}

/// 이 판본의 설치본을 쓰는 동안 쥐는 잠금 파일(`<기준>/<판본>.lock`). 쓰는 쪽은 공유, 어긋난 설치본을 복구하는 쪽은 배타 잠금을
/// 잡아야 하므로 쓰는 프로세스가 하나라도 있으면 복구하지 않는다.
fn use_lock(base: &Path, revision: &Revision, uid: u32) -> Result<File, String> {
    let path = base.join(format!("{}.lock", revision.0));
    match std::fs::symlink_metadata(&path) {
        Ok(old) if old.is_file() && old.uid() == uid => {}
        Ok(_) => {
            return Err(format!(
                "{}: 본인 소유의 일반 파일(링크 아님)이어야 해요",
                path.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(io(&path, e)),
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| io(&path, e))
}

/// 설치본을 쓰는 쪽으로 합류한다(공유 잠금). 복구 중인 프로세스가 있으면(배타) 기다리지 않고 Err.
fn share(lock: &File, dir: &Path) -> Result<(), String> {
    crate::lock::retry(|| lock.try_lock_shared()).map_err(|e| match e {
        TryLockError::WouldBlock => format!(
            "{}: 다른 프로세스가 이 설치본을 복구하는 중이에요 — 잠시 뒤 다시 시작하세요",
            dir.display()
        ),
        TryLockError::Error(e) => io(dir, e),
    })
}

/// 검증을 마친 새 복사본(`staging`)을 `dir` 자리에 놓고 설치본을 쓰는 쪽(공유 잠금)이 된다. 같은 판본의 설치본이 이미 있으면
/// 바이트·소유자·쓰기 권한이 맞을 때만 다시 쓴다. 어긋났으면 쓰는 프로세스가 하나도 없을 때(배타 잠금)만 치우고 새로 설치하고,
/// 쓰는 프로세스가 있으면 지우지 않고 거절한다. 실패하면 `staging`은 여기서 지운다.
fn settle(
    alias: &str,
    spec: &ServerSpec,
    base: &Path,
    staging: &Path,
    revision: &Revision,
    lock: &File,
    uid: u32,
) -> Result<(), String> {
    let dir = base.join(&revision.0);
    match std::fs::rename(staging, &dir) {
        Ok(()) => return share(lock, &dir),
        Err(error) if !dir.exists() => {
            remove_tree(staging);
            return Err(io(&dir, error));
        }
        Err(_) => {}
    }
    // 같은 판본의 설치본이 이미 있다(이전 시작·다른 프로세스). 먼저 쓰는 쪽으로 합류한다.
    if let Err(error) = share(lock, &dir) {
        remove_tree(staging);
        return Err(error);
    }
    if reusable(alias, spec, &dir, revision, uid) {
        remove_tree(staging);
        return Ok(());
    }
    // 어긋났다. 복구하려면 아무도 쓰고 있지 않아야 한다 — 내 공유 잠금을 놓고 배타 잠금을 잡아 본다.
    let alone = lock.unlock().is_ok() && crate::lock::retry(|| lock.try_lock()).is_ok();
    if !alone {
        remove_tree(staging);
        return Err(format!(
            "servers.{alias}: 설치본({})이 어긋나 있는데 다른 프로세스가 쓰는 중이라 복구하지 않아요 — 실행 중인 서버를 멈춘 뒤 다시 시작하세요",
            dir.display()
        ));
    }
    replace(&dir, staging, base)?;
    // 복구를 마쳤으니 이제 나도 쓰는 쪽(공유)이다
    lock.unlock().map_err(|e| io(&dir, e))?;
    share(lock, &dir)
}

pub(super) fn install_one(
    alias: &str,
    spec: &ServerSpec,
    base: &Path,
    uid: u32,
) -> Result<(Installed, File), String> {
    let staging = base.join(format!(".staging-{}", crate::nonce::hex()));
    DirBuilder::new()
        .mode(0o700)
        .create(&staging)
        .map_err(|e| io(&staging, e))?;
    let copied = walk(&spec.root, Some(&staging), None)
        .map_err(|e| format!("servers.{alias}: {e}"))
        .and_then(|(entries, dirs)| {
            let revision = revision_of(alias, spec, &entries)?;
            // 폴더를 아래에서부터 읽기 전용으로 잠근다(파일은 쓰면서 이미 잠갔다)
            for dir in dirs.iter().rev() {
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500))
                    .map_err(|e| io(dir, e))?;
            }
            std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o500))
                .map_err(|e| io(&staging, e))?;
            Ok(revision)
        });
    let revision = match copied {
        Ok(revision) => revision,
        Err(error) => {
            remove_tree(&staging);
            return Err(error);
        }
    };
    let lock = match use_lock(base, &revision, uid) {
        Ok(lock) => lock,
        Err(error) => {
            remove_tree(&staging);
            return Err(error);
        }
    };
    settle(alias, spec, base, &staging, &revision, &lock, uid)?;
    let dir = base.join(&revision.0);
    let mut argv = spec.command.clone();
    argv[0] = dir.join(&spec.command[0]).to_string_lossy().into_owned();
    Ok((
        Installed {
            revision,
            state: spec.state,
            dir,
            argv,
        },
        lock,
    ))
}
