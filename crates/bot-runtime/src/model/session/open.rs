//! Confined `openat`-based file opening for VM-writable session files, refusing symlinks, FIFOs, and directory escapes.

use rustix::{
    fs::{FileType, Mode, OFlags, fstat, open, openat},
    io::Errno,
};
use std::{fs::File, path::Path};

pub enum Opened {
    File(File),
    Missing,
    /// Target is not a regular file (symlink, FIFO, directory) or escapes `root`.
    Unsafe,
}

/// Traverses `relative` component by component under `root` with `O_NOFOLLOW`, opens the leaf with `O_NONBLOCK`, and verifies `fstat` is a regular file.
pub fn open_beneath(root: &Path, relative: &str) -> Opened {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::fs::symlink, path::PathBuf};

    fn root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bot-open-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("claude/projects/-bot")).unwrap();
        dir
    }

    fn read(opened: Opened) -> Option<String> {
        let Opened::File(mut file) = opened else {
            return None;
        };
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        Some(text)
    }

    #[test]
    fn a_regular_file_inside_the_root_opens_and_a_missing_one_is_not_yet() {
        let root = root("plain");
        let rel = "claude/projects/-bot/s.jsonl";
        assert!(matches!(open_beneath(&root, rel), Opened::Missing));
        std::fs::write(root.join(rel), "line\n").unwrap();
        assert_eq!(read(open_beneath(&root, rel)).as_deref(), Some("line\n"));
        assert!(matches!(
            open_beneath(&root.join("nope"), rel),
            Opened::Missing
        ));
    }

    #[test]
    fn links_fifos_folders_and_odd_paths_are_refused_without_blocking() {
        let root = root("hostile");
        let outside = std::env::temp_dir().join(format!("bot-open-secret-{}", std::process::id()));
        std::fs::write(&outside, "host secret\n").unwrap();
        let rel = "claude/projects/-bot/s.jsonl";
        symlink(&outside, root.join(rel)).unwrap();
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(root.join(rel)).unwrap();
        let outside_dir = std::env::temp_dir().join(format!("bot-open-dir-{}", std::process::id()));
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("s.jsonl"), "host secret\n").unwrap();
        std::fs::remove_dir_all(root.join("claude/projects/-bot")).unwrap();
        symlink(&outside_dir, root.join("claude/projects/-bot")).unwrap();
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(root.join("claude/projects/-bot")).unwrap();
        std::fs::create_dir_all(root.join("claude/projects/-bot")).unwrap();
        let fifo = root.join(rel);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(&fifo).unwrap();
        std::fs::create_dir_all(root.join(rel)).unwrap();
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        for odd in ["../x", "claude//s", "./s", "claude/../s", ""] {
            assert!(
                matches!(open_beneath(&root, odd), Opened::Unsafe),
                "{odd:?}"
            );
        }
        let _ = std::fs::remove_file(outside);
        let _ = std::fs::remove_dir_all(outside_dir);
    }
}
