//! Atomic read-only installation and shared-lock lifecycle for local tool servers.
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

const STALE_STAGING: Duration = Duration::from_secs(3600);

/// Restores write permission on read-only installed directories before removing the tree.
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

/// Ensures the install base is an owner-only (`0700`) directory and sweeps stale staging dirs.
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
            "{}: install base must be an owner-only (0700) directory owned by the current user",
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

fn reusable(alias: &str, spec: &ServerSpec, dir: &Path, revision: &Revision, uid: u32) -> bool {
    walk(dir, None, Some(uid))
        .ok()
        .and_then(|(entries, _)| revision_of(alias, spec, &entries).ok())
        .is_some_and(|found| found == *revision)
}

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
        tracing::warn!(
            path = %aside.display(),
            "failed to remove corrupted install directory; delete it manually"
        );
    }
    Ok(())
}

/// Per-revision lock file (`<base>/<rev>.lock`): readers hold shared locks, recovery requires an exclusive lock.
fn use_lock(base: &Path, revision: &Revision, uid: u32) -> Result<File, String> {
    let path = base.join(format!("{}.lock", revision.0));
    match std::fs::symlink_metadata(&path) {
        Ok(old) if old.is_file() && old.uid() == uid => {}
        Ok(_) => {
            return Err(format!(
                "{}: must be a regular file (not a symlink) owned by the current user",
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

fn share(lock: &File, dir: &Path) -> Result<(), String> {
    crate::lock::retry(|| lock.try_lock_shared()).map_err(|e| match e {
        TryLockError::WouldBlock => format!(
            "{}: another process is repairing this install — retry shortly",
            dir.display()
        ),
        TryLockError::Error(e) => io(dir, e),
    })
}

/// Moves `staging` into `<base>/<rev>` and acquires a shared use lock.
/// Existing installs are reused only if their content, ownership, and read-only modes match; corrupted ones are replaced only under an exclusive lock.
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
    if let Err(error) = share(lock, &dir) {
        remove_tree(staging);
        return Err(error);
    }
    if reusable(alias, spec, &dir, revision, uid) {
        remove_tree(staging);
        return Ok(());
    }
    // Upgrade to exclusive lock so we never replace an install actively used by another process.
    let alone = lock.unlock().is_ok() && crate::lock::retry(|| lock.try_lock()).is_ok();
    if !alone {
        remove_tree(staging);
        return Err(format!(
            "servers.{alias}: install ({}) is corrupted while in use by another process — stop running servers and restart",
            dir.display()
        ));
    }
    replace(&dir, staging, base)?;
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
            // Lock directories bottom-up so child chmod succeeds before parent becomes read-only.
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
