//! 작업 폴더 밖으로 새지 않는 안전한 열기. 세션 파일은 VM이 쓸 수 있는 작업 폴더 안에 있어서, 경로를 바꿔치기(심볼릭 링크·FIFO·폴더)하면
//! 호스트가 폴더 밖의 파일을 읽거나 읽기에서 멈출 수 있다. 그래서 작업 폴더에서 한 칸씩 `openat(O_NOFOLLOW)`로 내려가고(중간 폴더 교체에도 안전하다),
//! 마지막은 `O_NOFOLLOW|O_NONBLOCK`으로 열어(FIFO가 열기에서 멈추지 않게) **열린 fd 자체**가 일반 파일인지 확인한다.

use rustix::{
    fs::{FileType, Mode, OFlags, fstat, open, openat},
    io::Errno,
};
use std::{fs::File, path::Path};

pub enum Opened {
    File(File),
    /// 아직 없다(CLI가 곧 만든다)
    Missing,
    /// 일반 파일이 아니거나(링크·FIFO·폴더) 경로가 작업 폴더 밖으로 샌다: 이 세션을 믿을 수 없다
    Unsafe,
}

/// `root`(호스트가 만든 작업 폴더) 밑의 `relative`(`a/b/c`)를 연다. 상위(`..`)·빈 칸·NUL이 있는 경로는 받지 않는다.
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
    let (last, folders) = parts.split_last().expect("비어 있지 않다");
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
        std::fs::write(root.join(rel), "줄\n").unwrap();
        assert_eq!(read(open_beneath(&root, rel)).as_deref(), Some("줄\n"));
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
        // 마지막 칸이 링크
        symlink(&outside, root.join(rel)).unwrap();
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(root.join(rel)).unwrap();
        // 중간 폴더를 폴더 밖으로 가는 링크로 바꿔치기
        let outside_dir = std::env::temp_dir().join(format!("bot-open-dir-{}", std::process::id()));
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("s.jsonl"), "host secret\n").unwrap();
        std::fs::remove_dir_all(root.join("claude/projects/-bot")).unwrap();
        symlink(&outside_dir, root.join("claude/projects/-bot")).unwrap();
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(root.join("claude/projects/-bot")).unwrap();
        std::fs::create_dir_all(root.join("claude/projects/-bot")).unwrap();
        // FIFO: 열기에서 멈추지 않고 거절한다
        let fifo = root.join(rel);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(matches!(open_beneath(&root, rel), Opened::Unsafe));
        std::fs::remove_file(&fifo).unwrap();
        // 폴더, 상위·빈 칸·점 칸
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
