//! SOPS/age `SecretStore` decrypting `bot.<profile>.<name>` with 30s per-profile caching and encrypted updates.
//! Live updates use `sops set`/`unset` to preserve recipients and metadata without writing plaintext to disk.

use super::{Namespace, checked_profile};
use bot_core::{
    id::{ProfileId, SecretName},
    port::{Error, ProfileSecrets, Reply, SecretStore},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const TTL: Duration = Duration::from_secs(30);
const MAX_OUTPUT: usize = 4 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 64 * 1024;

type Namespaces = Mutex<HashMap<String, (Instant, Arc<BTreeMap<String, Vec<u8>>>)>>;

pub struct SopsStore {
    file: PathBuf,
    program: PathBuf,
    cache: Namespaces,
    /// Serializes `sops set`/`unset` calls targeting the same encrypted file.
    write_lock: Arc<Mutex<()>>,
}

impl SopsStore {
    pub fn new(file: PathBuf) -> Self {
        Self {
            file,
            program: "sops".into(),
            cache: Mutex::new(HashMap::new()),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_program(file: PathBuf, program: PathBuf) -> Self {
        Self {
            file,
            program,
            cache: Mutex::new(HashMap::new()),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn invalidate(&self, profile: &ProfileId) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&profile.0);
    }

    pub async fn list_names(&self, profile: &ProfileId) -> Result<BTreeSet<String>, Error> {
        let profile_name = checked_profile(profile)?.to_owned();
        let (program, file) = (self.program.clone(), self.file.clone());
        let values = tokio::task::spawn_blocking(move || decrypt(&program, &file, &profile_name))
            .await
            .map_err(|_| Error::Unavailable)??;
        Ok(values.into_keys().collect())
    }

    pub async fn set_secret(
        &self,
        profile: &ProfileId,
        name: &SecretName,
        value: &str,
    ) -> Result<(), Error> {
        let profile_name = checked_profile(profile)?.to_owned();
        if value.is_empty() || value.len() > MAX_SECRET_BYTES || value.contains(['\0', '\r', '\n'])
        {
            return Err(Error::Invalid);
        }
        let secret_name = name.as_str().to_owned();
        let secret_json = serde_json::to_string(value).map_err(|_| Error::Invalid)?;
        let (program, file, lock) = (
            self.program.clone(),
            self.file.clone(),
            self.write_lock.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            write_entry(&program, &file, &profile_name, &secret_name, &secret_json)
        })
        .await
        .map_err(|_| Error::Unavailable)??;
        self.invalidate(profile);
        Ok(())
    }

    pub async fn unset_secret(&self, profile: &ProfileId, name: &SecretName) -> Result<(), Error> {
        let profile_name = checked_profile(profile)?.to_owned();
        let secret_name = name.as_str().to_owned();
        let (program, file, lock) = (
            self.program.clone(),
            self.file.clone(),
            self.write_lock.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            remove_entry(&program, &file, &profile_name, &secret_name)
        })
        .await
        .map_err(|_| Error::Unavailable)??;
        self.invalidate(profile);
        Ok(())
    }
}

/// Passes only the environment variables `sops` needs to locate decryption keys.
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

fn decrypt_root(program: &Path, file: &Path) -> Result<serde_json::Map<String, Value>, Error> {
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
    Ok(root)
}

fn decrypt(program: &Path, file: &Path, profile: &str) -> Result<BTreeMap<String, Vec<u8>>, Error> {
    let root = decrypt_root(program, file)?;
    // Retain only this profile's namespace so foreign secrets are discarded immediately.
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

fn run_sops(program: &Path, args: &[&str], file: &Path) -> Result<(), Error> {
    let status = Command::new(program)
        .args(args)
        .arg(file)
        .env_clear()
        .envs(passthrough())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| Error::Unavailable)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Unavailable)
    }
}

fn write_entry(
    program: &Path,
    file: &Path,
    profile: &str,
    name: &str,
    json_value: &str,
) -> Result<(), Error> {
    if !file.is_file() {
        return Err(Error::Missing);
    }
    let root = decrypt_root(program, file)?;
    let has_bot = root.get("bot").is_some_and(Value::is_object);
    let has_profile = root
        .get("bot")
        .and_then(|bot| bot.get(profile))
        .is_some_and(Value::is_object);
    if !has_bot {
        run_sops(program, &["set", "[\"bot\"]", "{}"], file)?;
    }
    if !has_profile {
        let path = format!("[\"bot\"][\"{profile}\"]");
        run_sops(program, &["set", &path, "{}"], file)?;
    }
    let path = format!("[\"bot\"][\"{profile}\"][\"{name}\"]");
    run_sops(program, &["set", &path, json_value], file)
}

fn remove_entry(program: &Path, file: &Path, profile: &str, name: &str) -> Result<(), Error> {
    if !file.is_file() {
        return Err(Error::Missing);
    }
    let root = decrypt_root(program, file)?;
    let exists = root
        .get("bot")
        .and_then(|bot| bot.get(profile))
        .and_then(Value::as_object)
        .is_some_and(|obj| obj.contains_key(name));
    if !exists {
        return Ok(());
    }
    let path = format!("[\"bot\"][\"{profile}\"][\"{name}\"]");
    run_sops(program, &["unset", &path], file)
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

    fn fake_rw(dir: &Path) -> PathBuf {
        let path = dir.join("sops-rw");
        let script = r#"#!/usr/bin/env python3
import json, re, sys

args = sys.argv[1:]
if args[:3] == ["--decrypt", "--output-type", "json"]:
    with open(args[3], "r", encoding="utf-8") as f:
        print(f.read())
    sys.exit(0)
if args[0] == "set":
    keys = re.findall(r'\["([^"]+)"\]', args[1])
    val = json.loads(args[2])
    path = args[3]
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    cur = data
    for k in keys[:-1]:
        cur = cur[k]
    cur[keys[-1]] = val
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f)
    sys.exit(0)
if args[0] == "unset":
    keys = re.findall(r'\["([^"]+)"\]', args[1])
    path = args[2]
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    cur = data
    for k in keys[:-1]:
        cur = cur[k]
    cur.pop(keys[-1], None)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f)
    sys.exit(0)
sys.exit(2)
"#;
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
        assert!(matches!(
            resolve(&store, "personal", "not-text").await,
            Err(Error::Missing)
        ));
        assert!(matches!(
            resolve(&store, "nobody", "calendar-reader").await,
            Err(Error::Missing)
        ));
        let runs = std::fs::read_to_string(&counter).unwrap().lines().count();
        assert_eq!(runs, 3);
    }

    #[tokio::test]
    async fn set_unset_and_list_names_update_subtree_and_invalidate_cache() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secrets.sops.json");
        std::fs::write(&file, "{}").unwrap();
        let store = SopsStore::with_program(file, fake_rw(dir.path()));
        let team = ProfileId("team".into());
        let token = SecretName::parse("github-token").unwrap();

        assert!(matches!(
            resolve(&store, "team", "github-token").await,
            Err(Error::Missing)
        ));
        store
            .set_secret(&team, &token, "synthetic-secret-1")
            .await
            .unwrap();
        assert_eq!(
            resolve(&store, "team", "github-token").await.unwrap(),
            "synthetic-secret-1"
        );
        assert_eq!(
            store.list_names(&team).await.unwrap(),
            BTreeSet::from(["github-token".to_owned()])
        );
        store
            .set_secret(&team, &token, "synthetic-secret-2")
            .await
            .unwrap();
        assert_eq!(
            resolve(&store, "team", "github-token").await.unwrap(),
            "synthetic-secret-2"
        );
        assert!(matches!(
            store.set_secret(&team, &token, "").await,
            Err(Error::Invalid)
        ));
        assert!(matches!(
            store.set_secret(&team, &token, "a\nb").await,
            Err(Error::Invalid)
        ));
        store.unset_secret(&team, &token).await.unwrap();
        assert!(matches!(
            resolve(&store, "team", "github-token").await,
            Err(Error::Missing)
        ));
        assert!(store.list_names(&team).await.unwrap().is_empty());
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
        assert!(!counter.exists(), "invalid profiles must not invoke sops");
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
