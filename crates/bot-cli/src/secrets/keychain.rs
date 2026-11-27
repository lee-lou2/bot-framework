//! macOS 키체인 SecretStore. 서비스 `bot/<profile>`, 계정 `<name>`인 일반 암호 항목을 `security` 명령으로 읽는다.
//! 호출자가 프로필·전체 키를 넘길 수 없고(프로필은 핸들을 만들 때 고정), 명령 인자에는 이름만 있다 — 값은 파이프로만 온다.
use super::checked_profile;
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore, SecretValue},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

const SECURITY: &str = "/usr/bin/security";
/// `security find-generic-password`의 «항목 없음» 종료 코드
const NOT_FOUND: i32 = 44;
const MAX_VALUE: usize = 64 * 1024;

pub struct KeychainStore {
    program: PathBuf,
}

impl KeychainStore {
    /// macOS에서만 쓸 수 있다.
    pub fn new() -> Result<Self, String> {
        if cfg!(target_os = "macos") {
            Ok(Self {
                program: SECURITY.into(),
            })
        } else {
            Err("키체인 보관소는 macOS에서만 쓸 수 있어요".into())
        }
    }

    /// 시험용: `security` 대신 이 실행 파일을 쓴다. 운영 빌드에는 포함하지 않는다.
    #[cfg(any(test, feature = "testing"))]
    pub fn with_program(program: PathBuf) -> Self {
        Self { program }
    }

    /// macOS 키체인에 `bot/<profile>` / `<name>` 항목을 저장(또는 갱신)한다.
    pub async fn set_secret(
        &self,
        profile: &ProfileId,
        name: &SecretName,
        value: &str,
    ) -> Result<(), Error> {
        let profile = checked_profile(profile)?;
        if value.is_empty() || value.len() > MAX_VALUE || value.contains(['\0', '\r', '\n']) {
            return Err(Error::Invalid);
        }
        let service = format!("bot/{profile}");
        let account = name.as_str().to_owned();
        let secret = value.to_owned();
        let program = self.program.clone();
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(&program);
            command
                .args([
                    "add-generic-password",
                    "-U",
                    "-s",
                    &service,
                    "-a",
                    &account,
                    "-w",
                    &secret,
                ])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            let status = command.status().map_err(|_| Error::Unavailable)?;
            if status.success() {
                Ok(())
            } else {
                Err(Error::Unavailable)
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }

    /// macOS 키체인에서 `bot/<profile>` / `<name>` 항목을 삭제한다(이미 없으면 성공).
    pub async fn unset_secret(&self, profile: &ProfileId, name: &SecretName) -> Result<(), Error> {
        let profile = checked_profile(profile)?;
        let service = format!("bot/{profile}");
        let account = name.as_str().to_owned();
        let program = self.program.clone();
        tokio::task::spawn_blocking(move || {
            let mut command = Command::new(&program);
            command
                .args(["delete-generic-password", "-s", &service, "-a", &account])
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(home) = std::env::var_os("HOME") {
                command.env("HOME", home);
            }
            let status = command.status().map_err(|_| Error::Unavailable)?;
            match status.code() {
                Some(0) | Some(NOT_FOUND) => Ok(()),
                _ => Err(Error::Unavailable),
            }
        })
        .await
        .map_err(|_| Error::Unavailable)?
    }
}

struct Profile {
    program: PathBuf,
    service: String,
}

impl SecretStore for KeychainStore {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        let handle = checked_profile(&profile).map(|profile| {
            Box::new(Profile {
                program: self.program.clone(),
                service: format!("bot/{profile}"),
            }) as Box<dyn ProfileSecrets>
        });
        Box::pin(std::future::ready(handle))
    }
}

fn lookup(program: &Path, service: &str, name: &str) -> Result<Vec<u8>, Error> {
    let mut command = Command::new(program);
    command
        .args(["find-generic-password", "-s", service, "-a", name, "-w"])
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    // 로그인 키체인을 찾으려면 홈이 필요하다. 그 밖의 환경은 넘기지 않는다.
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    let output = command.output().map_err(|_| Error::Unavailable)?;
    match output.status.code() {
        Some(0) => {}
        Some(NOT_FOUND) => return Err(Error::Missing),
        _ => return Err(Error::Unavailable),
    }
    let mut value = output.stdout;
    if value.last() == Some(&b'\n') {
        value.pop();
    }
    if value.is_empty() {
        Err(Error::Missing)
    } else if value.len() > MAX_VALUE {
        Err(Error::Invalid)
    } else {
        Ok(value)
    }
}

impl ProfileSecrets for Profile {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        let (program, service) = (self.program.clone(), self.service.clone());
        Box::pin(async move {
            tokio::task::spawn_blocking(move || lookup(&program, &service, name.as_str()))
                .await
                .map_err(|_| Error::Unavailable)?
                .map(SecretValue)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// `security`를 흉내 내는 실행 파일: 서비스 bot/personal의 calendar-reader만 있다.
    fn fake(dir: &Path) -> PathBuf {
        let path = dir.join("security");
        std::fs::write(
            &path,
            "#!/bin/sh\n\
             if [ \"$*\" = \"find-generic-password -s bot/personal -a calendar-reader -w\" ]; then echo synthetic-keychain-value; exit 0; fi\n\
             if [ \"$*\" = \"find-generic-password -s bot/personal -a empty -w\" ]; then echo; exit 0; fi\n\
             if [ \"$1\" = \"boom\" ]; then exit 1; fi\n\
             exit 44\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn resolve(store: &KeychainStore, profile: &str, name: &str) -> Result<String, Error> {
        let handle = store.for_profile(ProfileId(profile.into())).await?;
        let value = handle.resolve(SecretName::parse(name).unwrap()).await?;
        Ok(String::from_utf8(value.0).unwrap())
    }

    #[tokio::test]
    async fn reads_the_profile_scoped_entry_and_strips_the_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeychainStore::with_program(fake(dir.path()));
        assert_eq!(
            resolve(&store, "personal", "calendar-reader")
                .await
                .unwrap(),
            "synthetic-keychain-value"
        );
        // 다른 프로필은 같은 이름이어도 자기 서비스만 본다
        assert!(matches!(
            resolve(&store, "work", "calendar-reader").await,
            Err(Error::Missing)
        ));
        assert!(matches!(
            resolve(&store, "personal", "absent").await,
            Err(Error::Missing)
        ));
        assert!(matches!(
            resolve(&store, "personal", "empty").await,
            Err(Error::Missing)
        ));
    }

    #[tokio::test]
    async fn refuses_unchecked_profiles_and_reports_backend_failures_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let store = KeychainStore::with_program(fake(dir.path()));
        for bad in ["", "Personal", "../x", "a/b", "a b"] {
            assert!(
                matches!(
                    store.for_profile(ProfileId(bad.into())).await,
                    Err(Error::Invalid)
                ),
                "{bad:?}"
            );
        }
        let missing_program = KeychainStore::with_program(dir.path().join("nope"));
        assert!(matches!(
            resolve(&missing_program, "personal", "calendar-reader").await,
            Err(Error::Unavailable)
        ));
    }

    #[test]
    fn only_macos_can_build_the_real_store() {
        assert_eq!(KeychainStore::new().is_ok(), cfg!(target_os = "macos"));
    }
}
