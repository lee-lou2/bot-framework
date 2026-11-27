//! Portable SHA-256 package revision hashing for installed tool servers.

use crate::{id::Revision, port::Error};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Whether an installed tool server requires a persistent state volume across calls.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerState {
    #[default]
    Ephemeral,
    Required,
}

/// Entry under the package root; file hashes are computed over installed bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstallEntry {
    Directory,
    File { executable: bool, sha256: [u8; 32] },
}

fn relative(path: &str) -> bool {
    !path.contains('\0') && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// `command[0]` must reference an executable file in `entries`; `entries` holds relative paths excluding the root.
pub fn server_revision(
    command: &[String],
    entries: &BTreeMap<String, InstallEntry>,
    state: ServerState,
) -> Result<Revision, Error> {
    let program = command.first().ok_or(Error::Invalid)?;
    if !matches!(
        entries.get(program),
        Some(InstallEntry::File {
            executable: true,
            ..
        })
    ) {
        return Err(Error::Invalid);
    }

    let mut hash = Sha256::new();
    hash.update(match state {
        ServerState::Ephemeral => b"bot-server-v1\0".as_slice(),
        ServerState::Required => b"bot-server-v1-state-required\0",
    });
    hash.update((command.len() as u64).to_be_bytes());
    for argument in command {
        if argument.contains('\0') {
            return Err(Error::Invalid);
        }
        field(&mut hash, argument.as_bytes());
    }

    hash.update((entries.len() as u64).to_be_bytes());
    for (path, entry) in entries {
        if !relative(path) {
            return Err(Error::Invalid);
        }
        if let Some((parent, _)) = path.rsplit_once('/')
            && entries.get(parent) != Some(&InstallEntry::Directory)
        {
            return Err(Error::Invalid);
        }
        field(&mut hash, path.as_bytes());
        match entry {
            InstallEntry::Directory => hash.update(b"d"),
            InstallEntry::File { executable, sha256 } => {
                hash.update(if *executable { b"x" } else { b"f" });
                hash.update(sha256);
            }
        }
    }
    Ok(Revision(format!("{:x}", hash.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(bytes: &[u8], executable: bool) -> InstallEntry {
        InstallEntry::File {
            executable,
            sha256: Sha256::digest(bytes).into(),
        }
    }

    fn package() -> BTreeMap<String, InstallEntry> {
        [
            ("bin".into(), InstallEntry::Directory),
            ("bin/server".into(), file(b"example\n", true)),
            ("data.txt".into(), file(b"help", false)),
        ]
        .into()
    }

    #[test]
    fn portable_package_revision_matches_the_wire_format() {
        let argv = ["bin/server".into(), "--stdio".into()];
        assert_eq!(
            server_revision(&argv, &package(), ServerState::default())
                .unwrap()
                .0,
            "160cbfbf525cf94649855f0ca561dccd2ff80aff4828f284a24a8caa1b6286c8"
        );
    }

    #[test]
    fn requiring_persistent_state_changes_the_approved_installation() {
        let argv = ["bin/server".into(), "--stdio".into()];
        let required = serde_json::from_str::<ServerState>(r#""required""#).unwrap();
        let ephemeral = serde_json::from_str::<ServerState>(r#""ephemeral""#).unwrap();
        assert_eq!(ephemeral, ServerState::default());
        assert_eq!(required, ServerState::Required);
        assert!(serde_json::from_str::<ServerState>(r#""optional""#).is_err());
        assert_ne!(
            server_revision(&argv, &package(), required).unwrap(),
            server_revision(&argv, &package(), ephemeral).unwrap()
        );
    }

    #[test]
    fn args_files_modes_and_empty_directories_bind_the_approval() {
        let argv = ["bin/server".into(), "a".into(), "b".into()];
        let original = server_revision(&argv, &package(), ServerState::Ephemeral).unwrap();
        for args in [
            vec!["bin/server".into(), "ab".into()],
            vec!["bin/server".into(), "b".into(), "a".into()],
        ] {
            assert_ne!(
                server_revision(&args, &package(), ServerState::Ephemeral).unwrap(),
                original
            );
        }
        for (path, entry) in [
            ("bin/server", file(b"new binary", true)),
            ("data.txt", file(b"new help", false)),
            ("data.txt", file(b"help", true)),
            ("empty", InstallEntry::Directory),
        ] {
            let mut entries = package();
            entries.insert(path.into(), entry);
            assert_ne!(
                server_revision(&argv, &entries, ServerState::Ephemeral).unwrap(),
                original
            );
        }
    }

    #[test]
    fn only_a_complete_tree_with_an_executable_entry_is_valid() {
        let argv = ["bin/server".into()];
        for candidate in ["absent", "data.txt", "bin"] {
            assert!(
                server_revision(&[candidate.into()], &package(), ServerState::Ephemeral).is_err()
            );
        }
        for path in [
            "/root",
            "../other",
            "bin//test",
            "./test",
            "missing/file",
            "data.txt/file",
        ] {
            let mut entries = package();
            entries.insert(path.into(), file(b"", false));
            assert!(server_revision(&argv, &entries, ServerState::Ephemeral).is_err());
        }
        assert!(server_revision(&[], &package(), ServerState::Ephemeral).is_err());
        assert!(
            server_revision(
                &["bin/server".into(), "a\0b".into()],
                &package(),
                ServerState::Ephemeral
            )
            .is_err()
        );
    }
}
