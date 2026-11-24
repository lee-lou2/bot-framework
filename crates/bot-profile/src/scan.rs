//! 폴더 스캔. 링크·특수 파일·하드링크·과대 입력을 거절하고, 파일 바이트를 한 번만 읽어
//! 메모리 트리에 고정한다. 이후 검증·판본·조립은 이 바이트만 쓴다(디스크를 다시 읽지 않는다).
use crate::{issue::Issues, limits::Limits, names, text};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fs,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// 읽지도 해시하지도 않는 이름(저장소·운영체제 부산물). 그 밖의 모르는 이름은 구조 검사가 거절한다.
const IGNORED: &[&str] = &[
    ".git",
    ".DS_Store",
    ".gitkeep",
    ".gitignore",
    ".gitattributes",
];

#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// 프로필 폴더 기준 `/` 경로 → 바이트
    pub files: BTreeMap<String, Vec<u8>>,
    /// 모든 하위 폴더 경로(빈 폴더 포함)
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

    /// `dir/` 바로 아래 하위 폴더 이름들(정렬)
    pub fn child_dirs(&self, dir: &str) -> Vec<&str> {
        children(self.dirs.iter().map(String::as_str), dir)
    }

    /// `dir/` 바로 아래 파일 이름들(정렬)
    pub fn child_files(&self, dir: &str) -> Vec<&str> {
        children(self.files.keys().map(String::as_str), dir)
    }

    /// `dir/` 아래 모든 파일 경로(정렬)
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
        "소켓"
    } else if ft.is_fifo() {
        "FIFO"
    } else if ft.is_block_device() {
        "블록 장치"
    } else if ft.is_char_device() {
        "문자 장치"
    } else {
        "특수 파일"
    }
}

/// BOT_PROFILES 바로 아래의 프로필 폴더들(이름순). 루트 자체는 링크여도 된다(운영자가 정한 위치).
pub fn list_profiles(root: &Path, limits: &Limits, issues: &mut Issues) -> Vec<(String, PathBuf)> {
    let shown = root.display().to_string();
    let canon = match fs::canonicalize(root) {
        Ok(p) if p.is_dir() => p,
        Ok(_) => {
            issues.error("root-not-dir", &shown, "BOT_PROFILES가 폴더가 아니에요");
            return Vec::new();
        }
        Err(e) => {
            issues.error(
                "root-missing",
                &shown,
                format!("BOT_PROFILES를 열 수 없어요: {e}"),
            );
            return Vec::new();
        }
    };
    if canon.join("profile.yaml").is_file() {
        issues.error(
            "root-is-profile",
            &shown,
            "BOT_PROFILES는 프로필 폴더들을 담는 폴더예요. 프로필 폴더 하나를 가리키고 있어요",
        );
        return Vec::new();
    }
    let rd = match fs::read_dir(&canon) {
        Ok(rd) => rd,
        Err(e) => {
            issues.error(
                "root-missing",
                &shown,
                format!("BOT_PROFILES를 읽을 수 없어요: {e}"),
            );
            return Vec::new();
        }
    };
    let mut names: Vec<(String, PathBuf)> = Vec::new();
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            issues.error("bad-name", &shown, "UTF-8이 아닌 이름이 있어요");
            continue;
        };
        if IGNORED.contains(&name.as_str()) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            issues.error("dir-unreadable", &name, "상태를 읽을 수 없어요");
            continue;
        };
        let ft = meta.file_type();
        if ft.is_symlink() {
            issues.error("symlink", &name, "링크는 허용하지 않아요");
        } else if ft.is_dir() {
            if names::is_name(&name) {
                names.push((name, entry.path()));
            } else {
                issues.error(
                    "bad-profile-name",
                    &name,
                    "프로필 폴더 이름은 소문자로 시작하는 소문자·숫자·`-` 63자 이하예요",
                );
            }
        } else if ft.is_file() {
            issues.error("stray-file", &name, "프로필 폴더가 아닌 파일이에요");
        } else {
            issues.error(
                "special-file",
                &name,
                format!("{}은 허용하지 않아요", kind_name(ft)),
            );
        }
    }
    names.sort();
    if names.len() > limits.max_profiles {
        issues.error(
            "too-many-profiles",
            &shown,
            format!("프로필이 너무 많아요(최대 {})", limits.max_profiles),
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

/// 프로필 폴더 하나를 읽는다. 오류가 하나라도 있으면(issues에 쌓임) 호출자는 트리를 쓰지 않는다.
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
                self.fail("dir-unreadable", rel, format!("폴더를 읽을 수 없어요: {e}"));
                return;
            }
        };
        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        for entry in rd {
            let Ok(entry) = entry else {
                self.fail("dir-unreadable", rel, "폴더 항목을 읽을 수 없어요");
                return;
            };
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_owned(),
                None => {
                    self.fail("bad-name", rel, "UTF-8이 아닌 이름이 있어요");
                    continue;
                }
            };
            if entries.len() + self.tree.files.len() + self.tree.dirs.len() > self.limits.max_files
            {
                self.fail(
                    "too-many-files",
                    rel,
                    format!("파일이 너무 많아요(최대 {})", self.limits.max_files),
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
                self.fail("bad-name", &child, "이름에 허용하지 않는 문자가 있어요");
                continue;
            }
            if !lower.insert(name.to_lowercase()) {
                self.fail(
                    "case-collision",
                    &child,
                    "같은 폴더에 대소문자만 다른 이름이 있어요",
                );
                continue;
            }
            if child.split('/').count() > self.limits.max_depth {
                self.fail(
                    "too-deep",
                    &child,
                    format!("폴더가 너무 깊어요(최대 {}단계)", self.limits.max_depth),
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
            self.fail("dir-unreadable", &child, "상태를 읽을 수 없어요");
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
                format!("링크는 허용하지 않아요(→ {target})"),
            );
        } else if ft.is_dir() {
            if !self.seen.insert((meta.dev(), meta.ino())) {
                self.fail(
                    "dir-loop",
                    &child,
                    "같은 폴더가 다시 나타나요(순환 또는 마운트)",
                );
                return;
            }
            if self.tree.dirs.len() >= self.limits.max_dirs {
                self.fail(
                    "too-many-files",
                    &child,
                    format!("폴더가 너무 많아요(최대 {})", self.limits.max_dirs),
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
                format!("{}은 허용하지 않아요", kind_name(ft)),
            );
        }
    }

    fn file(&mut self, path: &Path, child: String, meta: &fs::Metadata) {
        if meta.nlink() > 1 {
            self.fail(
                "hardlink",
                &child,
                format!(
                    "하드링크 파일이에요(링크 수 {}). 복사해서 두세요",
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
                format!("파일이 너무 많아요(최대 {})", self.limits.max_files),
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
                            "프로필이 너무 커요(최대 {} 바이트)",
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
                    format!("파일이 너무 커요(최대 {limit} 바이트)"),
                );
            }
            Err(ReadError::Changed) => {
                self.fail("changed", &child, "읽는 도중 파일이 바뀌었어요");
            }
            Err(ReadError::Io(e)) => {
                self.fail(
                    "file-unreadable",
                    &child,
                    format!("파일을 읽을 수 없어요: {e}"),
                );
            }
        }
    }
}

enum ReadError {
    TooLarge,
    Changed,
    Io(std::io::Error),
}

/// 연 파일이 방금 상태를 읽은 그 일반 파일인지(장치·i-node) 확인하고 상한까지만 읽는다.
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
