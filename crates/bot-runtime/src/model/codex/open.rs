//! Confined `openat`-based discovery and opening of Codex rollout JSONL files under the scratch directory.

use rustix::{
    fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, open, openat, statat},
    io::Errno,
};
use std::{fs::File, os::fd::OwnedFd, path::Path};

const MAX_DEPTH: usize = 3;
const MAX_ENTRIES: usize = 128;

pub(crate) enum Opened {
    File(File),
    Missing,
    /// Target is not a regular file (symlink, FIFO, directory) or escapes `root`.
    Unsafe,
}

pub(crate) enum Discovery {
    Found(String),
    Missing,
    /// Encountered a symlink, non-regular rollout entry, excessive tree, or multiple rollout files.
    Unsafe,
}

/// Traverses `relative` component by component under `root` with `O_NOFOLLOW`, opens the leaf with `O_NONBLOCK`, and verifies `fstat` is a regular file.
pub(crate) fn open_beneath(root: &Path, relative: &str) -> Opened {
    let parts: Vec<&str> = relative.split('/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == "." || *p == ".." || p.contains('\0'))
    {
        return Opened::Unsafe;
    }
    let Ok(mut dir) = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Opened::Missing;
    };
    let (last, folders) = parts.split_last().expect("parts is non-empty");
    for part in folders {
        dir = match openat(
            &dir,
            *part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => next,
            Err(Errno::NOENT) => return Opened::Missing,
            Err(_) => return Opened::Unsafe,
        };
    }
    let file = match openat(
        &dir,
        *last,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Opened::Missing,
        Err(_) => return Opened::Unsafe,
    };
    match fstat(&file) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
            Opened::File(File::from(file))
        }
        _ => Opened::Unsafe,
    }
}

/// Finds the unique `rollout-*.jsonl` regular file under `codex/sessions` without following symlinks.
pub(crate) fn discover_rollout(root: &Path) -> Discovery {
    let Ok(mut dir) = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) else {
        return Discovery::Missing;
    };
    for part in ["codex", "sessions"] {
        dir = match openat(
            &dir,
            part,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(next) => next,
            Err(Errno::NOENT) => return Discovery::Missing,
            Err(_) => return Discovery::Unsafe,
        };
    }
    let mut found = None;
    let mut seen = 0usize;
    if !walk(&dir, "codex/sessions", 0, &mut seen, &mut found) {
        return Discovery::Unsafe;
    }
    match found {
        Some(path) => Discovery::Found(path),
        None => Discovery::Missing,
    }
}

fn walk(
    dir: &OwnedFd,
    prefix: &str,
    depth: usize,
    seen: &mut usize,
    found: &mut Option<String>,
) -> bool {
    let Ok(reader) = Dir::read_from(dir) else {
        return false;
    };
    let mut names = Vec::new();
    for entry in reader {
        let Ok(entry) = entry else {
            return false;
        };
        let bytes = entry.file_name().to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return false;
        }
        let Ok(name) = std::str::from_utf8(bytes) else {
            return false;
        };
        if name.is_empty() || name.contains('/') || name.contains('\0') {
            return false;
        }
        names.push(name.to_owned());
    }
    names.sort();
    for name in names {
        let Ok(stat) = statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) else {
            return false;
        };
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Symlink => return false,
            FileType::Directory => {
                if depth >= MAX_DEPTH {
                    return false;
                }
                let Ok(child) = openat(
                    dir,
                    name.as_str(),
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                ) else {
                    return false;
                };
                let next_prefix = format!("{prefix}/{name}");
                if !walk(&child, &next_prefix, depth + 1, seen, found) {
                    return false;
                }
            }
            FileType::RegularFile => {
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    if found.is_some() {
                        return false;
                    }
                    *found = Some(format!("{prefix}/{name}"));
                }
            }
            _ => {
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    return false;
                }
            }
        }
    }
    true
}
