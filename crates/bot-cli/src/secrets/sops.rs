//! SOPS/age 파일 SecretStore. `sops --decrypt --output-type json <파일>`로 복호화한 JSON의 `bot.<profile>.<name>`을 읽는다.
//! age 키는 sops가 자기 방식(`SOPS_AGE_KEY_FILE` 등)으로 찾는다. 복호화는 프로필 핸들을 만들 때 하고, 핸들에는 그 프로필의
//! 이름 공간만 남긴다. 같은 프로필은 잠깐(30초) 기억해 호출마다 sops를 띄우지 않는다.
use super::{Namespace, checked_profile};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;

type Namespaces = Mutex<HashMap<String, (Instant, Arc<BTreeMap<String, Vec<u8>>>)>>;

pub struct SopsStore {
    file: PathBuf,
    program: PathBuf,
    cache: Namespaces,
}

impl SopsStore {
    pub fn new(file: PathBuf) -> Self {
        Self::with_program(file, "sops".into())
    }

    /// 시험용: 다른 실행 파일을 쓴다.
    pub fn with_program(file: PathBuf, program: PathBuf) -> Self {
        Self {
            file,
            program,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

/// sops가 키를 찾는 데 필요한 환경만 넘긴다(그 밖의 환경·비밀은 넘기지 않는다).
fn passthrough() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            matches!(key.as_ref(), "PATH" | "HOME" | "XDG_CONFIG_HOME")
                || key.starts_with("SOPS_")
                || key.starts_with("AGE")
                || key.starts_with("GNUPG")
        })
        .collect()
}

fn decrypt(program: &Path, file: &Path, profile: &str) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let output = Command::new(program)
        .args(["--decrypt", "--output-type", "json"])
        .arg(file)
        .env_clear()
        .envs(passthrough())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|_| Error::Unavailable)?;
    if !output.status.success() || output.stdout.len() > MAX_OUTPUT {
        return Err(Error::Unavailable);
    }
    let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(&output.stdout) else {
        return Err(Error::Invalid);
    };
    // 이 프로필의 이름 공간만 꺼낸다. 다른 프로필 값은 여기서 버린다.
    let own = root
        .get("bot")
        .and_then(|bot| bot.get(profile))
        .and_then(Value::as_object);
    Ok(own
        .into_iter()
        .flatten()
        .filter_map(|(name, value)| {
            SecretName::parse(name)?;
            Some((name.clone(), value.as_str()?.as_bytes().to_vec()))
        })
        .collect())
}

impl SecretStore for SopsStore {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        Box::pin(async move {
            let profile = checked_profile(&profile)?.to_owned();
            if let Some((at, values)) = self
                .cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&profile)
                && at.elapsed() < TTL
            {
                return Ok(Box::new(Namespace::new((**values).clone())) as Box<dyn ProfileSecrets>);
            }
            let (program, file, name) = (self.program.clone(), self.file.clone(), profile.clone());
            let values = tokio::task::spawn_blocking(move || decrypt(&program, &file, &name))
                .await
                .map_err(|_| Error::Unavailable)??;
            let values = Arc::new(values);
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(profile, (Instant::now(), values.clone()));
            Ok(Box::new(Namespace::new((*values).clone())) as Box<dyn ProfileSecrets>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const DECRYPTED: &str = r#"{"bot": {
        "personal": {"calendar-reader": "synthetic-sops-value", "not-text": 5, "Bad Name": "x"},
        "work": {"calendar-reader": "other-profile-value"}}}"#;

    /// `sops`를 흉내 내는 실행 파일: 인자를 확인하고 정해진 JSON을 내놓으며 호출 횟수를 센다.
    fn fake(dir: &Path, counter: &Path) -> PathBuf {
        let path = dir.join("sops");
        let script = format!(
            "#!/bin/sh\n\
             echo x >> '{}'\n\
             [ \"$1 $2 $3\" = \"--decrypt --output-type json\" ] || exit 2\n\
             cat <<'JSON'\n{DECRYPTED}\nJSON\n",
            counter.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn resolve(store: &SopsStore, profile: &str, name: &str) -> Result<String, Error> {
        let handle = store.for_profile(ProfileId(profile.into())).await?;
        let value = handle.resolve(SecretName::parse(name).unwrap()).await?;
        Ok(String::from_utf8(value.0).unwrap())
    }

    #[tokio::test]
    async fn each_profile_reads_only_its_own_subtree_and_decrypts_once_per_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        let store = SopsStore::with_program(
            dir.path().join("secrets.sops.json"),
            fake(dir.path(), &counter),
        );
        assert_eq!(
            resolve(&store, "personal", "calendar-reader")
                .await
                .unwrap(),
            "synthetic-sops-value"
        );
        assert_eq!(
            resolve(&store, "work", "calendar-reader").await.unwrap(),
            "other-profile-value"
        );
        assert!(matches!(
            resolve(&store, "personal", "absent").await,
            Err(Error::Missing)
        ));
        // 문자열이 아니거나 이름 규칙에 맞지 않는 항목은 없는 것이다
        assert!(matches!(
            resolve(&store, "personal", "not-text").await,
            Err(Error::Missing)
        ));
        assert!(matches!(
            resolve(&store, "nobody", "calendar-reader").await,
            Err(Error::Missing)
        ));
        // personal은 한 번만 복호화했다(work·nobody는 각각 한 번)
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 3);
    }

    #[tokio::test]
    async fn failures_and_unchecked_profiles_never_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("count");
        let broken = SopsStore::with_program(dir.path().join("x"), dir.path().join("missing-sops"));
        assert!(matches!(
            broken.for_profile(ProfileId("personal".into())).await,
            Err(Error::Unavailable)
        ));
        let store = SopsStore::with_program(dir.path().join("x"), fake(dir.path(), &counter));
        for bad in ["", "Personal", "../x", "a/b"] {
            assert!(
                matches!(
                    store.for_profile(ProfileId(bad.into())).await,
                    Err(Error::Invalid)
                ),
                "{bad:?}"
            );
        }
        assert!(!counter.exists(), "잘못된 프로필로는 sops를 띄우지 않는다");
        // 복호화 결과가 JSON 객체가 아니면 거절
        let bad = dir.path().join("bad-sops");
        std::fs::write(&bad, "#!/bin/sh\necho '[1]'\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store = SopsStore::with_program(dir.path().join("x"), bad);
        assert!(matches!(
            store.for_profile(ProfileId("personal".into())).await,
            Err(Error::Invalid)
        ));
    }
}
