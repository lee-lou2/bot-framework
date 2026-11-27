//! Directory scanner pinning profile bytes into memory once while rejecting links, special files, and oversized inputs.
use crate::{issue::Issues, limits::Limits, names, text};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// VCS/OS artifacts skipped before reading or hashing; all other unknown entries fail layout checks.
const IGNORED: &[&str] = &[
    ".git",
    ".DS_Store",
    ".gitkeep",
    ".gitignore",
    ".gitattributes",
];

#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub files: BTreeMap<String, Vec<u8>>,
    pub dirs: BTreeSet<String>,
}

impl Tree {
    #[cfg(test)]
    pub fn from_files<I, P>(files: I) -> Self
    where
        I: IntoIterator<Item = (P, Vec<u8>)>,
        P: Into<String>,
    {
        let mut tree = Tree::default();
        for (path, bytes) in files {
            let path: String = path.into();
            let mut prefix = String::new();
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_none() {
                    break;
                }
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                tree.dirs.insert(prefix.clone());
            }
            tree.files.insert(path, bytes);
        }
        tree
    }

    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files.get(path).map(Vec::as_slice)
    }

    pub fn child_dirs(&self, dir: &str) -> Vec<&str> {
        children(self.dirs.iter().map(String::as_str), dir)
    }

    pub fn child_files(&self, dir: &str) -> Vec<&str> {
        children(self.files.keys().map(String::as_str), dir)
    }

    pub fn files_under<'a>(&'a self, dir: &str) -> Vec<&'a str> {
        let prefix = format!("{dir}/");
        self.files
            .keys()
            .map(String::as_str)
            .filter(|p| p.starts_with(&prefix))
            .collect()
    }
}

fn children<'a>(paths: impl Iterator<Item = &'a str>, dir: &str) -> Vec<&'a str> {
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut out: Vec<&str> = paths
        .filter_map(|p| p.strip_prefix(prefix.as_str()))
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        .collect();
    out.sort_unstable();
    out
}

fn join(prefix: &str, rel: &str) -> String {
    match (prefix.is_empty(), rel.is_empty()) {
        (true, _) => rel.to_owned(),
        (false, true) => prefix.to_owned(),
        _ => format!("{prefix}/{rel}"),
    }
}

fn kind_name(ft: fs::FileType) -> &'static str {
    if ft.is_socket() {
        "socket"
    } else if ft.is_fifo() {
        "FIFO"
    } else if ft.is_block_device() {
        "block device"
    } else if ft.is_char_device() {
        "character device"
    } else {
        "special file"
    }
}

/// Lists sorted profile subdirectories under `BOT_PROFILES` (`root` itself may be an operator symlink).
pub fn list_profiles(root: &Path, limits: &Limits, issues: &mut Issues) -> Vec<(String, PathBuf)> {
    let shown = root.display().to_string();
    let canon = match fs::canonicalize(root) {
        Ok(p) if p.is_dir() => p,
        Ok(_) => {
            issues.error("root-not-dir", &shown, "BOT_PROFILES is not a directory");
            return Vec::new();
        }
        Err(e) => {
            issues.error(
                "root-missing",
                &shown,
                format!("Cannot open BOT_PROFILES: {e}"),
            );
            return Vec::new();
        }
    };
    if canon.join("profile.yaml").is_file() {
        issues.error(
            "root-is-profile",
            &shown,
            "BOT_PROFILES must point to the parent directory containing profile folders, not a single profile folder",
        );
        return Vec::new();
    }
    let rd = match fs::read_dir(&canon) {
        Ok(rd) => rd,
        Err(e) => {
            issues.error(
                "root-missing",
                &shown,
                format!("Cannot read BOT_PROFILES: {e}"),
            );
            return Vec::new();
        }
    };
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            issues.error("bad-name", &shown, "Entry name is not valid UTF-8");
            continue;
        };
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            issues.error("dir-unreadable", &name, "Cannot read file metadata");
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            issues.error("symlink", &name, "Symbolic links are not allowed");
        } else if ft.is_dir() {
            if names::is_name(&name) {
                names.push((name, entry.path()));
            } else {
                issues.error(
                    "bad-profile-name",
                    &name,
                    "Profile directory name must start with a lowercase letter and use only lowercase letters, digits, and `-` (max 63 chars)",
                );
            }
        } else if ft.is_file() {
            issues.error(
                "stray-file",
                &name,
                "Unexpected file in profiles root (expected profile directories)",
            );
        } else {
            issues.error(
                "special-file",
                &name,
                format!("{} is not allowed", kind_name(ft)),
            );
        }
    }
    names.sort();
    if names.len() > limits.max_profiles {
        issues.error(
            "too-many-profiles",
            &shown,
            format!("Too many profiles (max {})", limits.max_profiles),
        );
        names.truncate(limits.max_profiles);
    }
    names
}

struct Walk<'a> {
    limits: &'a Limits,
    issues: &'a mut Issues,
    prefix: &'a str,
    tree: Tree,
    bytes: u64,
    seen: HashSet<(u64, u64)>,
    aborted: bool,
}

/// Reads a profile directory once into memory; callers must discard the tree if `issues` records errors.
pub fn read_tree(dir: &Path, prefix: &str, limits: &Limits, issues: &mut Issues) -> Tree {
    let mut walk = Walk {
        limits,
        issues,
        prefix,
        tree: Tree::default(),
        bytes: 0,
        seen: HashSet::new(),
        aborted: false,
    };
    if let Ok(meta) = fs::symlink_metadata(dir) {
        walk.seen.insert((meta.dev(), meta.ino()));
    }
    let mut stack = vec![(dir.to_path_buf(), String::new())];
    while let Some((path, rel)) = stack.pop() {
        if walk.aborted {
            break;
        }
        walk.directory(&path, &rel, &mut stack);
    }
    walk.tree
}

impl Walk<'_> {
    fn fail(&mut self, code: &'static str, rel: &str, message: impl Into<String>) {
        let path = join(self.prefix, rel);
        self.issues.error(code, &path, message);
    }

    fn directory(&mut self, path: &Path, rel: &str, stack: &mut Vec<(PathBuf, String)>) {
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(e) => {
                self.fail("dir-unreadable", rel, format!("Cannot read directory: {e}"));
                return;
            }
        };
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        for entry in rd {
            let Ok(entry) = entry else {
                self.fail("dir-unreadable", rel, "Cannot read directory entry");
                return;
            };
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_owned(),
                None => {
                    self.fail("bad-name", rel, "Entry name is not valid UTF-8");
                    continue;
                }
            };
            if entries.len() + self.tree.files.len() + self.tree.dirs.len() > self.limits.max_files
            {
                self.fail(
                    "too-many-files",
                    rel,
                    format!("Too many files (max {})", self.limits.max_files),
                );
                self.aborted = true;
                return;
            }
            entries.push((name, entry.path()));
        }
        entries.sort();
        let mut lower: HashSet<String> = HashSet::new();
        for (name, entry_path) in entries {
            if IGNORED.contains(&name.as_str()) {
                continue;
            }
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if !text::name_is_clean(&name) {
                self.fail("bad-name", &child, "Name contains disallowed characters");
                continue;
            }
            if !lower.insert(name.to_lowercase()) {
                self.fail(
                    "case-collision",
                    &child,
                    "Directory contains names differing only by case",
                );
                continue;
            }
            if child.split('/').count() > self.limits.max_depth {
                self.fail(
                    "too-deep",
                    &child,
                    format!(
                        "Directory hierarchy is too deep (max {} levels)",
                        self.limits.max_depth
                    ),
                );
                continue;
            }
            self.entry(&entry_path, child, stack);
            if self.aborted {
                return;
            }
        }
    }

    fn entry(&mut self, path: &Path, child: String, stack: &mut Vec<(PathBuf, String)>) {
        let Ok(meta) = fs::symlink_metadata(path) else {
            self.fail("dir-unreadable", &child, "Cannot read file metadata");
            return;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            let target = fs::read_link(path)
                .map(|t| t.display().to_string())
                .unwrap_or_default();
            self.fail(
                "symlink",
                &child,
                format!("Symbolic links are not allowed (-> {target})"),
            );
        } else if ft.is_dir() {
            if !self.seen.insert((meta.dev(), meta.ino())) {
                self.fail(
                    "dir-loop",
                    &child,
                    "Directory appears twice (loop or bind mount)",
                );
                return;
            }
            if self.tree.dirs.len() >= self.limits.max_dirs {
                self.fail(
                    "too-many-files",
                    &child,
                    format!("Too many directories (max {})", self.limits.max_dirs),
                );
                self.aborted = true;
                return;
            }
            self.tree.dirs.insert(child.clone());
            stack.push((path.to_path_buf(), child));
        } else if ft.is_file() {
            self.file(path, child, &meta);
        } else {
            self.fail(
                "special-file",
                &child,
                format!("{} is not allowed", kind_name(ft)),
            );
        }
    }

    fn file(&mut self, path: &Path, child: String, meta: &fs::Metadata) {
        if meta.nlink() > 1 {
            self.fail(
                "hardlink",
                &child,
                format!(
                    "Hard-linked files are not allowed (link count {}); copy the file instead",
                    meta.nlink()
                ),
            );
            return;
        }
        let limit = if child.ends_with(".yaml") {
            self.limits.max_yaml_bytes
        } else {
            self.limits.max_file_bytes
        };
        if self.tree.files.len() >= self.limits.max_files {
            self.fail(
                "too-many-files",
                &child,
                format!("Too many files (max {})", self.limits.max_files),
            );
            self.aborted = true;
            return;
        }
        match read_pinned(path, meta, limit) {
            Ok(bytes) => {
                self.bytes += bytes.len() as u64;
                if self.bytes > self.limits.max_profile_bytes {
                    self.fail(
                        "too-large-total",
                        &child,
                        format!(
                            "Profile exceeds total size limit (max {} bytes)",
                            self.limits.max_profile_bytes
                        ),
                    );
                    self.aborted = true;
                    return;
                }
                self.tree.files.insert(child, bytes);
            }
            Err(ReadError::TooLarge) => {
                self.fail(
                    "too-large",
                    &child,
                    format!("File exceeds size limit (max {limit} bytes)"),
                );
            }
            Err(ReadError::Changed) => {
                self.fail("changed", &child, "File changed while being read");
            }
            Err(ReadError::Io(e)) => {
                self.fail("file-unreadable", &child, format!("Cannot read file: {e}"));
            }
        }
    }
}

enum ReadError {
    TooLarge,
    Changed,
    Io(std::io::Error),
}

/// Verifies the opened fd still matches `(dev, ino)` of the inspected regular file before reading up to `limit`.
fn read_pinned(path: &Path, before: &fs::Metadata, limit: u64) -> Result<Vec<u8>, ReadError> {
    let file = fs::File::open(path).map_err(ReadError::Io)?;
    let after = file.metadata().map_err(ReadError::Io)?;
    if !after.is_file() || after.dev() != before.dev() || after.ino() != before.ino() {
        return Err(ReadError::Changed);
    }
    if after.len() > limit {
        return Err(ReadError::TooLarge);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(after.len()).unwrap_or(0));
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(ReadError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(ReadError::TooLarge);
    }
    Ok(bytes)
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;
