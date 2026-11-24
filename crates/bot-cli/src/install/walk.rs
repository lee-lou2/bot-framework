//! 설치 폴더를 읽는 부분: 폴더를 한 번 훑으며 항목 표(상대 경로 → 폴더·파일[실행 비트·해시])를 만들고, 설치본이 있으면 같은 바이트를
//! 복사한다. 링크·특수 파일·UTF-8이 아닌 이름은 거절하고, 항목 수·바이트·깊이·경로 길이에 상한을 둔다.
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

/// 한 설치본이 가질 수 있는 항목 수·바이트·깊이·경로 길이의 상한(코드 전용 폴더가 데이터 폴더로 잘못 지정된 경우를 막는다)
const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: u64 = 1 << 30;
const MAX_DEPTH: usize = 32;
const MAX_PATH_BYTES: usize = 1024;

/// 읽는 중 쌓이는 상태
struct Walk {
    entries: BTreeMap<String, InstallEntry>,
    bytes: u64,
    /// 설치본에 만든 폴더(권한을 잠글 때 아래에서부터 쓴다)
    dirs: Vec<PathBuf>,
    /// 있으면 모든 폴더·파일이 이 사용자 소유이고 누구도 쓸 수 없어야 한다(이미 있는 설치본을 다시 쓰기 전의 확인)
    owner: Option<u32>,
}

/// 설치본의 폴더·파일은 설치한 사용자 소유이고 읽기 전용(쓰기 비트 없음)이어야 다시 쓸 수 있다.
fn audit(rel: &str, meta: &Metadata, uid: u32) -> Result<(), String> {
    if meta.uid() == uid && meta.mode() & 0o222 == 0 {
        Ok(())
    } else {
        Err(format!(
            "{rel}: 소유자·쓰기 권한이 설치본 규칙(본인 소유·읽기 전용)과 달라요"
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
                "{}: 폴더가 너무 깊어요(최대 {MAX_DEPTH})",
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
                format!("{}: UTF-8이 아닌 이름은 설치할 수 없어요", dir.display())
            })?;
            let rel_path = if rel.is_empty() {
                text.to_owned()
            } else {
                format!("{rel}/{text}")
            };
            if rel_path.len() > MAX_PATH_BYTES || !relative_ok(&rel_path) {
                return Err(format!("{rel_path}: 경로가 올바르지 않거나 너무 길어요"));
            }
            if self.entries.len() >= MAX_ENTRIES {
                return Err(format!(
                    "항목이 너무 많아요(최대 {MAX_ENTRIES}) — 코드 전용 폴더인지 확인하세요"
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
                return Err(format!("{rel_path}: 링크·특수 파일은 설치할 수 없어요"));
            }
        }
        Ok(())
    }

    /// 일반 파일 하나를 읽어 해시를 계산하고, 설치본이 있으면 같은 바이트를 쓴다.
    fn file(
        &mut self,
        path: &Path,
        seen: &Metadata,
        out: Option<&Path>,
    ) -> Result<InstallEntry, String> {
        let mut input = File::open(path).map_err(|e| io(path, e))?;
        // 열기 전에 본 그 파일이어야 한다(그 사이 링크·다른 파일로 바뀌지 않았는지)
        let opened = input.metadata().map_err(|e| io(path, e))?;
        if !opened.is_file() || (opened.dev(), opened.ino()) != (seen.dev(), seen.ino()) {
            return Err(format!("{}: 읽는 사이에 바뀌었어요", path.display()));
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
                    "{}: 설치 크기가 너무 커요(최대 {} MiB) — 코드 전용 폴더인지 확인하세요",
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

/// 폴더 아래를 모두 읽어 항목 표를 만든다. `out`이 있으면 같은 바이트를 그 폴더에 복사한다.
pub(super) fn walk(
    root: &Path,
    out: Option<&Path>,
    owner: Option<u32>,
) -> Result<(BTreeMap<String, InstallEntry>, Vec<PathBuf>), String> {
    let meta = std::fs::symlink_metadata(root).map_err(|e| io(root, e))?;
    if !meta.is_dir() {
        return Err(format!("{}: 링크가 아닌 폴더여야 해요", root.display()));
    }
    if let Some(uid) = owner {
        audit("(설치본 루트)", &meta, uid)?;
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
