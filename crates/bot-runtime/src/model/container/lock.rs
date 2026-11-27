//! Per-user exclusive flock ensuring only one container adapter owns a given instance name.

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

/// Retry budget (0.5s total) to tolerate a forked child that has not yet called exec to drop inherited FDs.
const LOCK_TRIES: u32 = 25;
const LOCK_PAUSE: Duration = Duration::from_millis(20);
const REOPEN_TRIES: u32 = 5;

fn failed() -> StartError {
    StartError("instance_lock_failed")
}

pub(super) struct InstanceLock {
    _file: File,
    identity: Identity,
}

impl InstanceLock {
    pub(super) fn identity(&self) -> Identity {
        self.identity.clone()
    }
}

/// Inode identity checked before every model spawn so deletion or replacement of the lock file fails closed.
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

/// Uses a fixed user-private path in production (independent of workdir/mount_root) so two processes with the same instance name always collide.
pub(super) fn lock_dir(config: &ContainerConfig) -> Result<PathBuf, StartError> {
    match config.lock_dir_override() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => default_lock_dir(std::env::var_os("HOME").map(PathBuf::from)),
    }
}

pub(super) fn default_lock_dir(home: Option<PathBuf>) -> Result<PathBuf, StartError> {
    let home = home.filter(|h| h.is_absolute()).ok_or_else(failed)?;
    Ok(home.join(".local/state/bot-framework/locks"))
}

pub(super) fn lock_path(dir: &Path, instance: &str) -> PathBuf {
    dir.join(format!(".botvm-{instance}.lock"))
}

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
        // Verify the locked FD still matches the directory entry and directory owner (reopen if unlinked/replaced).
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
