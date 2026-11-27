//! Single-pass directory tree walker that hashes server packages and optionally copies them read-only.
use super::relative_ok;
use bot_core::install::InstallEntry;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{DirBuilder, File, Metadata, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

// Safeguards against accidentally pointing a code-only server root at a large data directory.
const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: u64 = 1 << 30;
const MAX_DEPTH: usize = 32;
const MAX_PATH_BYTES: usize = 1024;

struct Walk {
    entries: BTreeMap<String, InstallEntry>,
    bytes: u64,
    // Created directories locked bottom-up after copying completes.
    dirs: Vec<PathBuf>,
    // When set, every entry must be owned by this UID and have no write bits (reuse audit).
    owner: Option<u32>,
}

fn audit(rel: &str, meta: &Metadata, uid: u32) -> Result<(), String> {
    if meta.uid() == uid && meta.mode() & 0o222 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{rel}: owner or write permissions violate install rules (must be owned by current user and read-only)"
        ))
    }
}

pub(super) fn io(path: &Path, error: std::io::Error) -> String {
    format!("{}: {error}", path.display())
}

impl Walk {
    fn visit(
        &mut self,
        dir: &Path,
        out: Option<&Path>,
        rel: &str,
        depth: usize,
    ) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err(format!(
                "{}: directory nesting too deep (max {MAX_DEPTH})",
                dir.display()
            ));
        }
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
            .map_err(|e| io(dir, e))?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| io(dir, e))?;
        names.sort();
        for name in names {
            let text = name.to_str().ok_or_else(|| {
                format!(
                    "{}: non-UTF-8 entry name cannot be installed",
                    dir.display()
                )
            })?;
            let rel_path = if rel.is_empty() {
                text.to_owned()
            } else {
                format!("{rel}/{text}")
            };
            if rel_path.len() > MAX_PATH_BYTES || !relative_ok(&rel_path) {
                return Err(format!("{rel_path}: invalid or overly long path"));
            }
            if self.entries.len() >= MAX_ENTRIES {
                return Err(format!(
                    "too many entries (max {MAX_ENTRIES}) — verify this is a code-only directory"
                ));
            }
            let path = dir.join(&name);
            let meta = std::fs::symlink_metadata(&path).map_err(|e| io(&path, e))?;
            let kind = meta.file_type();
            if let Some(uid) = self.owner
                && (kind.is_dir() || kind.is_file())
            {
                audit(&rel_path, &meta, uid)?;
            }
            if kind.is_dir() {
                self.entries
                    .insert(rel_path.clone(), InstallEntry::Directory);
                let child_out = match out {
                    Some(parent) => {
                        let child = parent.join(&name);
                        DirBuilder::new()
                            .mode(0o700)
                            .create(&child)
                            .map_err(|e| io(&child, e))?;
                        self.dirs.push(child.clone());
                        Some(child)
                    }
                    None => None,
                };
                self.visit(&path, child_out.as_deref(), &rel_path, depth + 1)?;
            } else if kind.is_file() {
                let entry = self.file(&path, &meta, out.map(|o| o.join(&name)).as_deref())?;
                self.entries.insert(rel_path, entry);
            } else {
                return Err(format!(
                    "{rel_path}: symlinks and special files cannot be installed"
                ));
            }
        }
        Ok(())
    }

    fn file(
        &mut self,
        path: &Path,
        seen: &Metadata,
        out: Option<&Path>,
    ) -> Result<InstallEntry, String> {
        let mut input = File::open(path).map_err(|e| io(path, e))?;
        // Verify the opened fd matches the lstat'd inode to prevent TOCTOU symlink swaps.
        let opened = input.metadata().map_err(|e| io(path, e))?;
        if !opened.is_file() || (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
            return Err(format!("{}: changed while reading", path.display()));
        }
        let executable = seen.mode() & 0o111 != 0;
        let mut output = match out {
            Some(target) => Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(target)
                    .map_err(|e| io(target, e))?,
            ),
            None => None,
        };
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let n = input.read(&mut buffer).map_err(|e| io(path, e))?;
            if n == 0 {
                break;
            }
            self.bytes += n as u64;
            if self.bytes > MAX_BYTES {
                return Err(format!(
                    "{}: package exceeds max install size ({} MiB) — verify this is a code-only directory",
                    path.display(),
                    MAX_BYTES >> 20
                ));
            }
            hash.update(&buffer[..n]);
            if let (Some(file), Some(target)) = (output.as_mut(), out) {
                file.write_all(&buffer[..n]).map_err(|e| io(target, e))?;
            }
        }
        if let (Some(file), Some(target)) = (output, out) {
            file.sync_all().map_err(|e| io(target, e))?;
            drop(file);
            let mode = if executable { 0o500 } else { 0o400 };
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))
                .map_err(|e| io(target, e))?;
        }
        Ok(InstallEntry::File {
            executable,
            sha256: hash.finalize().into(),
        })
    }
}

pub(super) fn walk(
    root: &Path,
    out: Option<&Path>,
    owner: Option<u32>,
) -> Result<(BTreeMap<String, InstallEntry>, Vec<PathBuf>), String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| io(root, e))?;
    if !meta.is_dir() {
        return Err(format!(
            "{}: must be a real directory, not a symlink",
            root.display()
        ));
    }
    if let Some(uid) = owner {
        audit("(install root)", &meta, uid)?;
    }
    let mut state = Walk {
        entries: BTreeMap::new(),
        bytes: 0,
        dirs: Vec::new(),
        owner,
    };
    state.visit(root, out, "", 0)?;
    Ok((state.entries, state.dirs))
}
