//! Unix socket path resolution and owner-only bind/connect verification.
//!
//! Enforces a non-symlink 0700 parent directory and 0600 socket file so sockets pre-created by other users in
//! shared directories cannot be trusted by either server or client.
use std::{
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;

pub const FILE_NAME: &str = "gateway.sock";
const HOME_RUN_DIR: &str = ".local/state/bot/run";

/// Precedence: `BOT_SOCKET` > `cli.socket` > `XDG_RUNTIME_DIR`/`TMPDIR` > `$HOME/.local/state/bot/run`.
/// Never falls back to shared `/tmp`.
pub fn socket_path(
    configured: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
    uid: u32,
) -> Result<PathBuf, String> {
    let pick = |name: &str| env(name).filter(|value| !value.is_empty());
    if let Some(explicit) = pick("BOT_SOCKET") {
        return Ok(PathBuf::from(explicit));
    }
    if let Some(configured) = configured {
        return Ok(PathBuf::from(configured));
    }
    if let Some(runtime) = pick("XDG_RUNTIME_DIR").or_else(|| pick("TMPDIR")) {
        return Ok(PathBuf::from(runtime)
            .join(format!("bot-{uid}"))
            .join(FILE_NAME));
    }
    match pick("HOME").filter(|home| Path::new(home).is_absolute()) {
        Some(home) => Ok(PathBuf::from(home).join(HOME_RUN_DIR).join(FILE_NAME)),
        None => Err("cannot determine socket path — XDG_RUNTIME_DIR, TMPDIR, and HOME (absolute path) are all unset; set BOT_SOCKET or gateway.cli.socket".to_owned()),
    }
}

/// Resolves the current UID via `/usr/bin/id -u` without pulling in `libc`.
pub fn current_uid() -> io::Result<u32> {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()?;
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|_| output.status.success())
        .ok_or_else(|| io::Error::other("failed to determine uid"))
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.to_owned())
}

fn socket_folder(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| denied("socket path requires a parent directory"))
}

/// Shared by server `bind` and client `trusted_socket`: rejects symlinks, wrong owner, or group/world bits.
pub fn private_folder(parent: &Path, uid: u32) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(parent)?;
    if !meta.is_dir() {
        return Err(denied(
            "socket directory must be a real directory, not a symlink",
        ));
    }
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(denied(
            "socket directory must be owned by you and accessible only to the owner (0700)",
        ));
    }
    Ok(())
}

/// Pre-connect client verification; returns `NotFound` before checking parent permissions if no socket exists.
pub fn trusted_socket(path: &Path, uid: u32) -> io::Result<()> {
    if let Err(error) = std::fs::symlink_metadata(path)
        && error.kind() == io::ErrorKind::NotFound
    {
        return Err(error);
    }
    private_folder(socket_folder(path)?, uid)?;
    if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
        return Err(denied("path is not a socket file"));
    }
    Ok(())
}

/// Creates missing parent directories with mode 0700, replaces stale sockets, refuses live servers or non-socket files.
pub fn bind(path: &Path, uid: u32) -> io::Result<UnixListener> {
    let parent = socket_folder(path)?;
    if std::fs::symlink_metadata(parent).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
    }
    private_folder(parent, uid)?;
    match std::fs::symlink_metadata(path) {
        Ok(existing) if existing.file_type().is_socket() => {
            if std::os::unix::net::UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another running server is already using this socket",
                ));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => return Err(denied("a non-socket file already exists at this path")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn path_precedence_is_env_then_host_then_runtime_dir_then_home() {
        let home = env(&[("HOME", "/home/me")]);
        let none = env(&[]);
        let refused = socket_path(None, &none, 501).unwrap_err();
        assert!(
            refused.contains("BOT_SOCKET") && refused.contains("HOME"),
            "{refused}"
        );
        assert_eq!(
            socket_path(None, &home, 501).unwrap(),
            PathBuf::from("/home/me/.local/state/bot/run/gateway.sock")
        );
        let tmp = env(&[("TMPDIR", "/var/t/"), ("HOME", "/home/me")]);
        assert_eq!(
            socket_path(None, &tmp, 501).unwrap(),
            PathBuf::from("/var/t/bot-501/gateway.sock")
        );
        let both = env(&[("TMPDIR", "/var/t"), ("XDG_RUNTIME_DIR", "/run/user/501")]);
        assert_eq!(
            socket_path(None, &both, 501).unwrap(),
            PathBuf::from("/run/user/501/bot-501/gateway.sock")
        );
        assert_eq!(
            socket_path(Some("/srv/b/s.sock"), &both, 501).unwrap(),
            PathBuf::from("/srv/b/s.sock")
        );
        let forced = env(&[("BOT_SOCKET", "/x/y.sock"), ("TMPDIR", "/var/t")]);
        assert_eq!(
            socket_path(Some("/srv/b/s.sock"), &forced, 501).unwrap(),
            PathBuf::from("/x/y.sock")
        );
        let empty = env(&[
            ("BOT_SOCKET", ""),
            ("XDG_RUNTIME_DIR", ""),
            ("TMPDIR", ""),
            ("HOME", "/home/me"),
        ]);
        assert_eq!(
            socket_path(None, &empty, 7).unwrap(),
            PathBuf::from("/home/me/.local/state/bot/run/gateway.sock")
        );
        for odd in ["", "home/me", "."] {
            let only_home = |name: &str| (name == "HOME").then(|| odd.to_owned());
            assert!(socket_path(None, &only_home, 501).is_err(), "{odd:?}");
        }
    }

    fn scratch() -> tempfile::TempDir {
        // Keep socket paths under the 104-byte Unix domain socket limit.
        tempfile::Builder::new()
            .prefix("g")
            .tempdir_in("/tmp")
            .unwrap()
    }

    #[tokio::test]
    async fn creates_a_private_folder_and_a_0600_socket() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let path = dir.path().join("run").join(FILE_NAME);
        let _listener = bind(&path, uid).unwrap();
        let folder = std::fs::metadata(path.parent().unwrap()).unwrap();
        assert_eq!(folder.mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let second = bind(&path, uid).err().unwrap();
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn refuses_shared_folders_links_foreign_owners_and_regular_files() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let open = dir.path().join("open");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&open)
            .unwrap();
        assert!(bind(&open.join(FILE_NAME), uid).is_err());
        let mine = dir.path().join("mine");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&mine)
            .unwrap();
        assert!(bind(&mine.join(FILE_NAME), uid + 1).is_err());
        let link = dir.path().join("link");
        symlink(&mine, &link).unwrap();
        assert!(bind(&link.join(FILE_NAME), uid).is_err());
        let file = mine.join("plain");
        std::fs::write(&file, b"keep").unwrap();
        assert!(bind(&file, uid).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    fn make_dir(path: &Path, mode: u32) {
        std::fs::DirBuilder::new().mode(mode).create(path).unwrap();
        // Restore exact mode after umask masking.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn a_client_only_trusts_a_socket_in_my_private_folder() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let path = dir.path().join("run").join(FILE_NAME);
        assert_eq!(
            trusted_socket(&path, uid).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        let open = dir.path().join("open");
        make_dir(&open, 0o755);
        assert_eq!(
            trusted_socket(&open.join(FILE_NAME), uid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
        let _listener = bind(&path, uid).unwrap();
        assert!(trusted_socket(&path, uid).is_ok());
        assert_eq!(
            trusted_socket(&path, uid + 1).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let plain = path.parent().unwrap().join("plain");
        std::fs::write(&plain, b"x").unwrap();
        assert_eq!(
            trusted_socket(&plain, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        let link = path.parent().unwrap().join("link.sock");
        symlink(&path, &link).unwrap();
        assert_eq!(
            trusted_socket(&link, uid).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn a_client_refuses_a_socket_in_a_shared_folder_or_behind_a_link() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        for mode in [0o777, 0o755, 0o750, 0o705] {
            let shared = dir.path().join(format!("shared-{mode:o}"));
            make_dir(&shared, mode);
            let path = shared.join(FILE_NAME);
            let _fake = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let refused = trusted_socket(&path, uid).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
            assert!(refused.to_string().contains("0700"), "{mode:o}: {refused}");
        }
        let mine = dir.path().join("mine");
        make_dir(&mine, 0o700);
        let _server = std::os::unix::net::UnixListener::bind(mine.join(FILE_NAME)).unwrap();
        assert!(trusted_socket(&mine.join(FILE_NAME), uid).is_ok());
        let link = dir.path().join("link");
        symlink(&mine, &link).unwrap();
        let refused = trusted_socket(&link.join(FILE_NAME), uid).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(refused.to_string().contains("symlink"), "{refused}");
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let refused = trusted_socket(&file.join(FILE_NAME), uid).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            refused
                .to_string()
                .contains("real directory, not a symlink"),
            "{refused}"
        );
        let shared = PathBuf::from(format!("/tmp/gwtest-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&shared);
        let _fake = std::os::unix::net::UnixListener::bind(&shared).unwrap();
        let refused = trusted_socket(&shared, uid).unwrap_err().kind();
        let _ = std::fs::remove_file(&shared);
        assert_eq!(refused, io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn bind_creates_missing_parents_privately() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let path = dir.path().join("a").join("b").join(FILE_NAME);
        let _listener = bind(&path, uid).unwrap();
        for folder in [dir.path().join("a"), dir.path().join("a").join("b")] {
            assert_eq!(
                std::fs::metadata(&folder).unwrap().mode() & 0o777,
                0o700,
                "{}",
                folder.display()
            );
        }
        assert!(trusted_socket(&path, uid).is_ok());
    }

    #[tokio::test]
    async fn replaces_a_dead_socket_file() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let path = dir.path().join("run").join(FILE_NAME);
        drop(bind(&path, uid).unwrap());
        assert!(path.exists());
        let _again = bind(&path, uid).unwrap();
    }
}
