//! 유닉스 소켓의 위치와 안전한 바인드·접속. 소켓 폴더는 소유자만 들어갈 수 있어야 하고(0700), 소켓은 0600이다.
//! 서버는 접속자를 피어 자격(uid)으로 확인하고, 클라이언트도 **서버를** 확인한다: 접속하기 전에 폴더·소켓 파일을 점검하고(`trusted_socket`),
//! 접속한 뒤 피어 uid가 자기 uid인지 보고서야 요청을 보낸다. 공용 폴더에 다른 사용자가 먼저 만들어 둔 가짜 소켓에는 대화 글이 가지 않는다.
use std::{
    io,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};
use tokio::net::UnixListener;

pub const FILE_NAME: &str = "gateway.sock";
/// 런타임 폴더가 없을 때 홈 아래에 쓰는 폴더(공용 `/tmp`로 떨어지지 않는다)
const HOME_RUN_DIR: &str = ".local/state/bot/run";

/// 소켓 경로: `BOT_SOCKET` > 호스트 `cli.socket` > `<런타임 폴더>/bot-<uid>/gateway.sock`.
/// 런타임 폴더는 `XDG_RUNTIME_DIR`, 없으면 `TMPDIR`다. 둘 다 없으면(리눅스의 cron·su·컨테이너 등) 공용 `/tmp`가 아니라 홈 아래
/// `$HOME/.local/state/bot/run/gateway.sock`을 쓰고, 홈도 절대 경로로 알 수 없으면 위치를 정할 수 없다고 이름을 밝혀 거절한다.
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
        None => Err("소켓 위치를 정할 수 없어요 — XDG_RUNTIME_DIR·TMPDIR·HOME(절대 경로)이 모두 없어요. BOT_SOCKET(절대 경로)이나 호스트 설정의 gateway.cli.socket으로 지정하세요".to_owned()),
    }
}

/// 이 프로세스의 uid. libc 없이 `/usr/bin/id -u`로 묻는다.
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
        .ok_or_else(|| io::Error::other("uid를 확인하지 못했어요"))
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.to_owned())
}

fn socket_folder(path: &Path) -> io::Result<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| denied("소켓 경로에 폴더가 필요해요"))
}

/// 소켓 폴더가 본인(`uid`) 소유의 비공개(0700) 폴더이고 링크가 아닌가. 폴더가 없으면 `NotFound`, 어긋나면 `PermissionDenied`.
/// 서버의 `bind`와 클라이언트의 `trusted_socket`이 같은 규칙을 쓴다.
pub fn private_folder(parent: &Path, uid: u32) -> io::Result<()> {
    // `symlink_metadata`는 링크를 따라가지 않으므로 링크는 폴더(`is_dir`)가 아니다
    let meta = std::fs::symlink_metadata(parent)?;
    if !meta.is_dir() {
        return Err(denied("소켓 폴더는 링크가 아닌 폴더여야 해요"));
    }
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(denied(
            "소켓 폴더는 본인 소유이고 소유자만 접근할 수 있어야 해요(0700)",
        ));
    }
    Ok(())
}

/// 클라이언트가 접속하기 **전에** 하는 점검: 소켓 폴더가 본인 소유의 비공개 폴더이고(`private_folder`), 소켓 파일이 진짜 소켓(링크·일반 파일 아님)이다.
/// 소켓 파일의 주인은 따로 보지 않는다 — 내 비공개 폴더 안에는 나(또는 루트)만 만들 수 있고, 반대편이 정말 내 계정의 프로세스인지는 접속한 뒤 피어 uid로 확인한다.
/// 없으면 `NotFound`(서버가 떠 있지 않다), 어긋나면 `PermissionDenied` — 이때는 접속하지도 글을 보내지도 않는다.
pub fn trusted_socket(path: &Path, uid: u32) -> io::Result<()> {
    // 소켓이 아예 없으면 믿고 말고 할 것이 없다 — 폴더 권한이 어떻든 «서버가 안 떠 있다»(`NotFound`)가 먼저다. 접속하지 않으니 보낼 것도 없다
    if let Err(error) = std::fs::symlink_metadata(path)
        && error.kind() == io::ErrorKind::NotFound
    {
        return Err(error);
    }
    private_folder(socket_folder(path)?, uid)?;
    if !std::fs::symlink_metadata(path)?.file_type().is_socket() {
        return Err(denied("소켓 파일이 아니에요"));
    }
    Ok(())
}

/// 소켓을 안전하게 연다. 폴더가 없으면 0700으로 만들고(없는 상위 폴더도 0700), 있으면 소유자 본인의 비공개 폴더(링크 아님)여야 한다.
/// 이미 떠 있는 서버가 있으면 `AddrInUse`, 죽은 소켓 파일은 지우고 다시 연다. 소켓이 아닌 파일은 건드리지 않는다.
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
                    "이미 실행 중인 서버가 이 소켓을 쓰고 있어요",
                ));
            }
            std::fs::remove_file(path)?;
        }
        Ok(_) => return Err(denied("소켓이 아닌 파일이 이미 있어요")),
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
        // 런타임 폴더도 홈도 없으면 위치를 정할 수 없다 — 공용 `/tmp`로 떨어지지 않는다
        let none = env(&[]);
        let refused = socket_path(None, &none, 501).unwrap_err();
        assert!(
            refused.contains("BOT_SOCKET") && refused.contains("HOME"),
            "{refused}"
        );
        // 홈만 있으면 홈 아래 개인 폴더다(공용 폴더가 아니다)
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
        // 빈 값은 없는 것으로 본다
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
        // 홈이 비었거나 절대 경로가 아니면 쓰지 않는다(현재 폴더에 따라 달라지는 위치는 믿을 수 없다)
        for odd in ["", "home/me", "."] {
            let only_home = |name: &str| (name == "HOME").then(|| odd.to_owned());
            assert!(socket_path(None, &only_home, 501).is_err(), "{odd:?}");
        }
    }

    fn scratch() -> tempfile::TempDir {
        // 유닉스 소켓 주소는 104바이트 안이어야 해서 짧은 위치에 만든다
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
        // 같은 소켓에 두 번째 서버는 거절
        let second = bind(&path, uid).err().unwrap();
        assert_eq!(second.kind(), io::ErrorKind::AddrInUse);
    }

    #[tokio::test]
    async fn refuses_shared_folders_links_foreign_owners_and_regular_files() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        // 그룹·다른 사용자가 접근할 수 있는 폴더
        let open = dir.path().join("open");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&open)
            .unwrap();
        assert!(bind(&open.join(FILE_NAME), uid).is_err());
        // 다른 소유자로 보이는 폴더
        let mine = dir.path().join("mine");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&mine)
            .unwrap();
        assert!(bind(&mine.join(FILE_NAME), uid + 1).is_err());
        // 폴더가 링크
        let link = dir.path().join("link");
        symlink(&mine, &link).unwrap();
        assert!(bind(&link.join(FILE_NAME), uid).is_err());
        // 소켓이 아닌 파일은 지우지 않는다
        let file = mine.join("plain");
        std::fs::write(&file, b"keep").unwrap();
        assert!(bind(&file, uid).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    }

    fn make_dir(path: &Path, mode: u32) {
        std::fs::DirBuilder::new().mode(mode).create(path).unwrap();
        // umask가 깎은 권한을 시험이 정한 값으로 되돌린다
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn a_client_only_trusts_a_socket_in_my_private_folder() {
        let dir = scratch();
        let uid = current_uid().unwrap();
        let path = dir.path().join("run").join(FILE_NAME);
        // 서버가 아직 없다: `NotFound`(폴더도, 소켓 파일도)
        assert_eq!(
            trusted_socket(&path, uid).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        // 폴더 권한이 어떻든 소켓이 없으면 «서버가 없다»(`NotFound`)다
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
        // 다른 사용자의 것으로 보이는 폴더·소켓
        assert_eq!(
            trusted_socket(&path, uid + 1).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        // 소켓이 아닌 파일·링크
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
        // 다른 사용자가 만들어 둘 수 있는 폴더(0777·0755·0750·0705) 안의 소켓
        for mode in [0o777, 0o755, 0o750, 0o705] {
            let shared = dir.path().join(format!("shared-{mode:o}"));
            make_dir(&shared, mode);
            let path = shared.join(FILE_NAME);
            let _fake = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let refused = trusted_socket(&path, uid).unwrap_err();
            assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied, "{mode:o}");
            assert!(refused.to_string().contains("0700"), "{mode:o}: {refused}");
        }
        // 폴더가 링크다(내 비공개 폴더를 가리켜도)
        let mine = dir.path().join("mine");
        make_dir(&mine, 0o700);
        let _server = std::os::unix::net::UnixListener::bind(mine.join(FILE_NAME)).unwrap();
        assert!(trusted_socket(&mine.join(FILE_NAME), uid).is_ok());
        let link = dir.path().join("link");
        symlink(&mine, &link).unwrap();
        let refused = trusted_socket(&link.join(FILE_NAME), uid).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        // 링크라서 거절한다고 밝힌다(권한 문제로 오해하지 않게)
        assert!(refused.to_string().contains("링크"), "{refused}");
        // 폴더 자리에 일반 파일이 있어도(내 소유·0600) 소켓 폴더로 믿지 않는다
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let refused = trusted_socket(&file.join(FILE_NAME), uid).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            refused.to_string().contains("링크가 아닌 폴더"),
            "{refused}"
        );
        // 공용 `/tmp` 바로 아래(다른 사용자 소유·스티키 폴더)에 있는 소켓
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
        drop(bind(&path, uid).unwrap()); // 서버가 죽어 소켓 파일만 남은 상태
        assert!(path.exists());
        let _again = bind(&path, uid).unwrap();
    }
}
