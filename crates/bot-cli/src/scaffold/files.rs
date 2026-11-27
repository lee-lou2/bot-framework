//! 새 파일·폴더를 만드는 쓰기 도구. 기존 것을 덮어쓰지 않고, 이미 있는 구성 요소가 링크이면 따라가지 않는다. 경로 조각은 호출자가 `is_name`으로
//! 검사한 값이거나 코드에 적힌 상수여야 한다.
//!
//! - 기준 폴더는 사용자가 준 경로를 **한 번만** 연다(경로 속 링크는 사용자가 고른 것이라 따라간다). 풀어 놓고 다시 여는 일은 없고, 그 뒤 모든 일은
//!   그 fd를 기준으로 한 칸씩(`openat`·`mkdirat`, 링크는 `O_NOFOLLOW`) 한다 — 검사한 경로를 이름으로 다시 열지 않으므로, 열고 난 뒤에 기준 폴더나
//!   중간 폴더를 링크로 바꿔 쳐도 바깥에 쓰지 않는다.
//! - 파일은 하나씩 `O_EXCL`로 만든다. 실패했을 때 되돌림(`undo`)은 **우리가 만든 빈 폴더를 `rmdir`하는 것뿐**이다: 파일을 이름으로 지우는 일은
//!   (확인한 뒤 지우기 전에 편집기가 새 파일로 바꿔 놓을 수 있어) 안전하게 할 수 없으므로 하지 않고, 남은 파일의 경로를 돌려준다.
//! - 새 폴더 통째로는 `Stage`가 맡는다: 기준 폴더 아래 임시 폴더에 다 만들어 `renameat2(NOREPLACE)`로 최종 이름에 원자적으로 옮긴다
//!   (이미 있으면 실패, 중간에 실패하면 최종 이름에는 아무것도 없다). 임시 폴더는 처음부터 최종 권한(`mkdir`이 주는 `0777 & !umask`)으로 만들고 권한을 나중에 바꾸지 않으며, 만든 뒤 열 때 정말 우리 것인지(소유자·그 권한·비어 있음) 확인하고,
//!   치울 때는 **우리가 만든 이름만** 잡고 있는 fd 기준으로 지운다(재귀로 쓸어 지우지 않는다).
//!
//! 위협 모델: 같은 부모 폴더에 쓸 수 있는 쪽은 그 이름공간을 이미 통제한다(어떤 이름에든 자기 내용을 둘 수 있다). 여기서 막는 것은 그 권한을 넘는
//! 일뿐이다 — 이 프로세스가 남이 건드릴 수 없는 데이터를 지우거나, 부모 밖에 쓰는 것.
use rustix::{
    fs::{
        AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, RenameFlags, Stat, fchmod, flock,
        fstat, mkdirat, open, openat, renameat, renameat_with, statat, unlinkat,
    },
    io::Errno,
    process::{geteuid, umask},
};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::OwnedFd,
    path::Path,
    sync::Mutex,
};

pub struct Writer {
    /// 기준 폴더. 열어 둔 fd라서 이름이 나중에 다른 곳을 가리켜도 이 폴더를 가리킨다.
    root: OwnedFd,
    /// 이번에 만든 폴더(기준 폴더 아래 경로 조각)
    dirs: Vec<Vec<String>>,
    /// 이번에 만든 파일(기준 폴더 아래 `/` 경로)
    files: Vec<String>,
}

/// 경로 조각 하나가 평범한 이름인가(빈 조각·`.`·`..`·구분자·NUL 거절)
fn plain(part: &str) -> bool {
    !part.is_empty() && !matches!(part, "." | "..") && !part.contains(['/', '\\', '\0'])
}

/// 폴더를 여는 방법: 마지막 구성 요소가 링크이면 열지 않는다.
fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn kind(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

fn clone(fd: &OwnedFd) -> Result<OwnedFd, String> {
    fd.try_clone()
        .map_err(|_| "폴더를 다시 열지 못했어요".to_owned())
}

/// 폴더 fd에 잡은 배타 잠금(`flock`). 잠금 파일을 만들지 않으며, 값이 떨어질 때 잠금을 푼다.
pub struct DirLock(OwnedFd);

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

/// `replace_regular`가 스킬 폴더 잠금 안에서 쓰는 고정 임시 파일 이름.
const TOOL_TMP: &str = ".bot-add-tool.tmp";

/// 이전 강제 종료(`SIGKILL`)로 남은 `.bot-add-tool.tmp`가 우리 소유의 안전한 일반 파일(`nlink == 1`)일 때만 치운다.
fn clean_stale_tmp(dir: &OwnedFd, rel: &str) -> Result<(), String> {
    let fd = match openat(
        dir,
        TOOL_TMP,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(()),
        Err(Errno::LOOP) => {
            return Err(format!(
                "{rel}의 임시 파일({TOOL_TMP})이 링크라 덮어쓰지 않았어요"
            ));
        }
        Err(error) => {
            return Err(format!(
                "{rel}의 임시 파일({TOOL_TMP})을 확인하지 못했어요: {error}"
            ));
        }
    };
    let stat =
        fstat(&fd).map_err(|_| format!("{rel}의 임시 파일({TOOL_TMP})을 확인하지 못했어요"))?;
    let mine = u64::from(stat.st_uid) == u64::from(geteuid().as_raw());
    if kind(&stat) != FileType::RegularFile || stat.st_nlink != 1 || !mine {
        return Err(format!(
            "{rel}의 임시 파일({TOOL_TMP})이 안전한 일반 파일이 아니라 지우지 않았어요"
        ));
    }
    drop(fd);
    unlinkat(dir, TOOL_TMP, AtFlags::empty())
        .map_err(|error| format!("{rel}의 이전 임시 파일({TOOL_TMP})을 치우지 못했어요: {error}"))
}

impl Writer {
    /// `parts` 폴더에 배타 잠금(`flock`)을 건다. 가는 길이나 대상이 링크면 거절한다.
    pub fn lock_dir(&self, parts: &[&str]) -> Result<DirLock, String> {
        let (depth, dir) = self.present(parts)?;
        if depth < parts.len() {
            return Err(format!("{} 폴더가 없어요", parts.join("/")));
        }
        loop {
            match flock(&dir, FlockOperation::LockExclusive) {
                Ok(()) => return Ok(DirLock(dir)),
                Err(Errno::INTR) => continue,
                Err(error) => {
                    return Err(format!(
                        "{} 폴더 잠금을 잡지 못했어요: {error}",
                        parts.join("/")
                    ));
                }
            }
        }
    }

    /// `root`는 사용자가 지정한 기준 폴더로 이미 있는 폴더여야 한다. 그 경로를 여기서 한 번만 연다.
    pub fn new(root: &Path) -> Result<Self, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = open(root, flags, Mode::empty()).map_err(|errno| match errno {
            Errno::NOENT => format!("{} 폴더가 없어요 — 먼저 만들어 주세요", root.display()),
            Errno::NOTDIR => format!("{}은(는) 폴더가 아니에요", root.display()),
            _ => format!("{} 폴더를 열지 못했어요", root.display()),
        })?;
        Ok(Self::at(fd))
    }

    fn at(root: OwnedFd) -> Self {
        Self {
            root,
            dirs: Vec::new(),
            files: Vec::new(),
        }
    }

    /// `parts`를 따라 이어서 있는 폴더의 수와 그 마지막 폴더. 이미 있는 것이 링크이거나 폴더가 아니면 거절한다(링크는 따라가지 않는다).
    fn present(&self, parts: &[&str]) -> Result<(usize, OwnedFd), String> {
        let mut dir = clone(&self.root)?;
        for (index, part) in parts.iter().enumerate() {
            if !plain(part) {
                return Err(format!("경로 조각이 올바르지 않아요: {part:?}"));
            }
            match openat(&dir, *part, dir_flags(), Mode::empty()) {
                Ok(next) => dir = next,
                Err(Errno::NOENT) => return Ok((index, dir)),
                // 링크는 플랫폼에 따라 ELOOP 또는 ENOTDIR로 거절된다: 링크인지 아닌지 알려 준다
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    let shown = parts[..=index].join("/");
                    let link = statat(&dir, *part, AtFlags::SYMLINK_NOFOLLOW)
                        .is_ok_and(|stat| kind(&stat) == FileType::Symlink);
                    return Err(if link {
                        format!("{shown}은(는) 링크라 따라가지 않아요")
                    } else {
                        format!("{shown}이(가) 폴더가 아니에요")
                    });
                }
                Err(_) => {
                    return Err(format!(
                        "{}을(를) 확인하지 못했어요",
                        parts[..=index].join("/")
                    ));
                }
            }
        }
        Ok((parts.len(), dir))
    }

    /// `parts`가 (링크를 포함해) 무엇이든 이미 있는가. 가는 길에 링크가 있으면 거절한다.
    pub fn exists(&self, parts: &[&str]) -> Result<bool, String> {
        let Some((last, parents)) = parts.split_last() else {
            return Ok(false);
        };
        if !plain(last) {
            return Err(format!("경로 조각이 올바르지 않아요: {last:?}"));
        }
        let (depth, dir) = self.present(parents)?;
        if depth < parents.len() {
            return Ok(false);
        }
        match statat(&dir, *last, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(_) => Err(format!("{}을(를) 확인하지 못했어요", parts.join("/"))),
        }
    }

    /// `parts`가 가리키는 것이 아직 없어야 한다. 있으면(파일·링크 포함) 덮어쓰지 않고 거절한다.
    pub fn require_absent(&self, parts: &[&str]) -> Result<(), String> {
        if self.exists(parts)? {
            return Err(format!(
                "{}이(가) 이미 있어요 — 덮어쓰지 않아요",
                parts.join("/")
            ));
        }
        Ok(())
    }

    /// `parts`가 이미 있는 일반 파일인가(링크는 파일로 치지 않는다). 가는 길에 링크가 있으면 거절한다.
    pub fn file_exists(&self, parts: &[&str]) -> Result<bool, String> {
        let Some((last, parents)) = parts.split_last() else {
            return Ok(false);
        };
        let (depth, dir) = self.present(parents)?;
        if depth < parents.len() {
            return Ok(false);
        }
        Ok(statat(&dir, *last, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|stat| kind(&stat) == FileType::RegularFile))
    }

    /// `parts` 폴더 바로 아래의 폴더 이름(정렬). 링크는 폴더로 치지 않는다. 폴더가 없으면 빈 목록이다.
    pub fn child_dirs(&self, parts: &[&str]) -> Result<Vec<String>, String> {
        let (depth, dir) = self.present(parts)?;
        if depth < parts.len() {
            return Ok(Vec::new());
        }
        let entries =
            Dir::read_from(&dir).map_err(|_| format!("{}을(를) 읽지 못했어요", parts.join("/")))?;
        let mut names = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let is_dir = match entry.file_type() {
                FileType::Directory => true,
                FileType::Unknown => statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                    .is_ok_and(|stat| kind(&stat) == FileType::Directory),
                _ => false,
            };
            if let (true, Ok(text)) = (is_dir, name.to_str()) {
                names.push(text.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// `parts`가 이미 있는 일반 파일이면 링크·FIFO 대기 없이 `max_bytes` 이하로 읽는다. 링크·하드링크·특수 파일·크기 초과는 거절한다.
    pub fn read_regular(&self, parts: &[&str], max_bytes: u64) -> Result<Vec<u8>, String> {
        let Some((last, parents)) = parts.split_last() else {
            return Err("빈 경로예요".into());
        };
        if !plain(last) {
            return Err(format!("경로 조각이 올바르지 않아요: {last:?}"));
        }
        let shown = parts.join("/");
        let (depth, dir) = self.present(parents)?;
        if depth < parents.len() {
            return Err(format!("{shown}이(가) 없어요"));
        }
        let fd = openat(
            &dir,
            *last,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::NOENT => format!("{shown}이(가) 없어요"),
            Errno::LOOP => format!("{shown}은(는) 링크라 따라가지 않아요"),
            error => format!("{shown}을(를) 열지 못했어요: {error}"),
        })?;
        let stat = fstat(&fd).map_err(|_| format!("{shown}을(를) 확인하지 못했어요"))?;
        if kind(&stat) != FileType::RegularFile {
            return Err(format!("{shown}은(는) 일반 파일이 아니에요"));
        }
        if stat.st_nlink > 1 {
            return Err(format!("{shown}은(는) 하드링크라 수정하지 않아요"));
        }
        if stat.st_size < 0 || stat.st_size as u64 > max_bytes {
            return Err(format!(
                "{shown}이(가) 크기 한도({max_bytes} 바이트)를 넘어요"
            ));
        }
        let mut buf = Vec::new();
        File::from(fd)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|error| format!("{shown}을(를) 읽지 못했어요: {error}"))?;
        if buf.len() as u64 > max_bytes {
            return Err(format!(
                "{shown}이(가) 크기 한도({max_bytes} 바이트)를 넘어요"
            ));
        }
        Ok(buf)
    }

    /// 이미 있는 일반 파일 `rel`을 같은 폴더 안의 고정 임시 파일(`.bot-add-tool.tmp`)로 쓰고 기존 권한을 맞춘 뒤 원자적으로 바꾼다.
    /// 호출자가 대상 폴더의 `DirLock`을 쥐고 있어야 하며, 링크·하드링크·특수 파일은 거절한다.
    pub fn replace_regular(&self, rel: &str, content: &str) -> Result<(), String> {
        let parts: Vec<&str> = rel.split('/').collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Err("빈 경로예요".into());
        };
        if !parts.iter().all(|part| plain(part)) {
            return Err(format!("경로가 올바르지 않아요: {rel:?}"));
        }
        let (depth, dir) = self.present(dirs)?;
        if depth < dirs.len() {
            return Err(format!("{rel}의 상위 폴더가 없어요"));
        }
        let existing_fd = openat(
            &dir,
            *name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::LOOP => format!("{rel}은(는) 링크라 덮어쓰지 않아요"),
            Errno::NOENT => format!("{rel}이(가) 없어요"),
            _ => format!("{rel}을(를) 확인하지 못했어요"),
        })?;
        let stat = fstat(&existing_fd).map_err(|_| format!("{rel}을(를) 확인하지 못했어요"))?;
        if kind(&stat) != FileType::RegularFile {
            return Err(format!("{rel}은(는) 일반 파일이 아니에요"));
        }
        if stat.st_nlink > 1 {
            return Err(format!("{rel}은(는) 하드링크라 덮어쓰지 않아요"));
        }
        let existing_mode = Mode::from_raw_mode(stat.st_mode & 0o777);
        drop(existing_fd);

        clean_stale_tmp(&dir, rel)?;
        let fd = openat(
            &dir,
            TOOL_TMP,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|error| format!("{rel} 임시 파일을 만들지 못했어요: {error}"))?;
        let mut file = File::from(fd);
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            return Err(format!("{rel}을(를) 쓰지 못했어요: {error}"));
        }
        if let Err(error) = fchmod(&file, existing_mode) {
            drop(file);
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            return Err(format!("{rel}의 권한을 맞추지 못했어요: {error}"));
        }
        drop(file);
        renameat(&dir, TOOL_TMP, &dir, *name).map_err(|error| {
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            format!("{rel}을(를) 바꾸지 못했어요: {error}")
        })
    }

    /// 새 파일을 만든다(필요한 폴더도 새로 만든다). `rel`은 `/`로 구분한 기준 폴더 아래 상대 경로다.
    pub fn write(&mut self, rel: &str, content: &str) -> Result<(), String> {
        let parts: Vec<&str> = rel.split('/').collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Err("빈 경로예요".into());
        };
        if !parts.iter().all(|part| plain(part)) {
            return Err(format!("경로가 올바르지 않아요: {rel:?}"));
        }
        let (present, mut dir) = self.present(dirs)?;
        for (index, part) in dirs.iter().enumerate().skip(present) {
            let shown = dirs[..=index].join("/");
            mkdirat(&dir, *part, Mode::from_raw_mode(0o777))
                .map_err(|error| format!("{shown} 폴더를 만들지 못했어요: {error}"))?;
            self.dirs
                .push(dirs[..=index].iter().map(|p| (*p).to_owned()).collect());
            // 방금 만든 폴더도 링크를 따라가지 않고 다시 연다(그 사이 바꿔 쳤다면 여기서 멈춘다)
            dir = openat(&dir, *part, dir_flags(), Mode::empty())
                .map_err(|_| format!("{shown} 폴더를 열지 못했어요"))?;
        }
        // 배타 생성(O_EXCL)이라 이미 있는 파일은 물론 링크(끊어진 것 포함)도 따라가지 않고 거절한다
        let file = openat(
            &dir,
            *name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )
        .map_err(|errno| match errno {
            Errno::EXIST => format!("{rel}이(가) 이미 있어요 — 덮어쓰지 않아요"),
            error => format!("{rel} 파일을 만들지 못했어요: {error}"),
        })?;
        self.files.push(rel.to_owned());
        File::from(file)
            .write_all(content.as_bytes())
            .map_err(|error| format!("{rel}을(를) 쓰지 못했어요: {error}"))
    }

    /// 이번에 만든 폴더 중 **비어 있는 것만** 거꾸로 지운다(비어 있지 않으면 `rmdir`이 거절해 남는다). 만든 파일은 지우지 않는다 — 이름으로
    /// 지우면 그 사이 편집기가 새 파일로 바꿔 놓은 것을 지울 수 있다. 남은 파일의 경로(정렬)를 돌려준다.
    ///
    /// 마지막 `rmdir`은 이름으로 하므로 그 사이 그 자리에 새로 생긴 빈 폴더를 지울 수 있다. `rmdir`은 빈 폴더만 지우니 잃는 데이터는 없다.
    pub fn undo(&mut self) -> Vec<String> {
        for parts in std::mem::take(&mut self.dirs).iter().rev() {
            let Some((name, parents)) = parts.split_last() else {
                continue;
            };
            let parents: Vec<&str> = parents.iter().map(String::as_str).collect();
            if let Ok((depth, dir)) = self.present(&parents)
                && depth == parents.len()
            {
                let _ = unlinkat(&dir, name.as_str(), AtFlags::REMOVEDIR);
            }
        }
        self.created()
    }

    /// 이번에 만든 파일의 기준 폴더 상대 경로(정렬)
    pub fn created(&self) -> Vec<String> {
        let mut files = self.files.clone();
        files.sort();
        files
    }

    /// 기준 폴더 아래에 무작위 이름의 임시 폴더를 **최종 권한**(`mkdir`이 주는 `0777 & !umask`)으로 만든다. 거기에 다 만든 뒤 `Stage::publish`로
    /// 최종 이름에 옮긴다. 권한을 나중에 바꾸지 않으므로(템플릿에는 비밀이 없어 만드는 동안 읽혀도 된다) 이름이 다른 폴더로 바뀌어도 그 폴더의 권한이
    /// 달라지는 일이 없다. 만든 직후 열 때 그것이 정말 우리가 방금 만든 폴더인지 확인한다(아니면 손대지 않고 실패한다).
    pub fn stage(&self) -> Result<Stage, String> {
        self.stage_with(|_| {})
    }

    /// `between`은 폴더를 만든 뒤 열기 전에 불린다(시험이 그 사이의 바꿔치기를 흉내 낸다).
    fn stage_with(&self, between: impl Fn(&str)) -> Result<Stage, String> {
        for _ in 0..32u32 {
            let name = format!(
                ".bot-init-{}-{}",
                std::process::id(),
                bot_core::random::random_hex(8)
            );
            // 기대 권한을 알려면 umask를 읽어야 하는데 읽는 호출이 없어 잠깐 바꿨다 되돌린다: 같은 일을 하는 다른 스레드와 겹치면 umask가 엉키므로 잠그고,
            // 같은 잠금 안에서 만들어 «읽은 값 = 만들 때 값»을 지킨다.
            let (made, expected) = {
                let _guard = UMASK
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let expected = usual_mode();
                (
                    mkdirat(&self.root, name.as_str(), Mode::from_raw_mode(0o777)),
                    expected,
                )
            };
            match made {
                Ok(()) => {
                    between(&name);
                    // 열지 못하거나 우리 것이 아니면 그 이름의 것을 지우지 않는다: 우리가 만든 것이 아닐 수 있다
                    let fd = openat(&self.root, name.as_str(), dir_flags(), Mode::empty())
                        .map_err(|_| "임시 폴더를 열지 못했어요".to_owned())?;
                    check_ours(&fd, expected)?;
                    return Ok(Stage {
                        parent: clone(&self.root)?,
                        name,
                        writer: Writer::at(fd),
                    });
                }
                // 같은 이름이 이미 있으면 다른 이름을 쓴다
                Err(Errno::EXIST) => {}
                Err(error) => return Err(format!("임시 폴더를 만들지 못했어요: {error}")),
            }
        }
        Err("임시 폴더 이름을 정하지 못했어요".into())
    }

    /// 이번에 만든 파일을 이름으로 지운다(거꾸로, 열어 둔 폴더 기준, 링크는 따라가지 않는다). **우리만 쓰는 임시 폴더 안에서만** 쓴다 — 사용자의
    /// 폴더에서는 이름으로 파일을 지우지 않는다(`undo`).
    fn unlink_created_files(&self) {
        for rel in self.files.iter().rev() {
            let parts: Vec<&str> = rel.split('/').collect();
            let Some((name, parents)) = parts.split_last() else {
                continue;
            };
            if let Ok((depth, dir)) = self.present(parents)
                && depth == parents.len()
            {
                let _ = unlinkat(&dir, *name, AtFlags::empty());
            }
        }
    }
}

/// `mkdir(0o777)`이 이 프로세스의 umask로 만들어 내는 권한(`0777 & !umask`). 호출하는 쪽이 `UMASK`를 쥐고 있어야 한다(umask를 읽는 호출이 없어 잠깐
/// 바꿨다 되돌린다).
fn usual_mode() -> u64 {
    let current = umask(Mode::empty());
    umask(current);
    u64::from(Mode::from_raw_mode(0o777).difference(current).bits())
}

/// `usual_mode`를 읽고 폴더를 만드는 동안 쥐는 잠금(umask는 프로세스 전체가 하나다).
static UMASK: Mutex<()> = Mutex::new(());

/// 우리가 방금 만든 임시 폴더의 모양인가: 폴더이고, 소유자가 `me`이고, 권한이 만들 때 기대한 값(`expected`)이다. 값은 플랫폼마다 폭이 다른 정수라 `u64`로
/// 받는다.
fn made_by_us(kind: FileType, owner: u64, mode: u64, me: u64, expected: u64) -> bool {
    kind == FileType::Directory && owner == me && mode & 0o777 == expected
}

/// 방금 만든 임시 폴더를 열었을 때 그것이 정말 우리가 만든 것인지 본다: 소유자가 나이고, 권한이 기대한 값이고, 비어 있어야 한다. 이름을 만든 뒤 열기
/// 전에 다른 폴더(남의 것·내용이 든 것·권한이 다른 내 폴더)로 바뀌었다면 그것을 잡지 않고 실패한다 — 잡았다면 실패를 치우다 남의 내용을 지우거나 그
/// 폴더를 게시할 수 있다.
fn check_ours(fd: &OwnedFd, expected: u64) -> Result<(), String> {
    let refuse = || {
        "임시 폴더가 우리가 방금 만든 것이 아니에요(소유자·권한·내용이 달라요) — 아무것도 지우지 않고 멈췄어요".to_owned()
    };
    let stat = fstat(fd).map_err(|_| "임시 폴더를 확인하지 못했어요".to_owned())?;
    let me = u64::from(geteuid().as_raw());
    if !made_by_us(
        kind(&stat),
        u64::from(stat.st_uid),
        u64::from(stat.st_mode),
        me,
        expected,
    ) {
        return Err(refuse());
    }
    let entries = Dir::read_from(fd).map_err(|_| "임시 폴더를 읽지 못했어요".to_owned())?;
    for entry in entries {
        let entry = entry.map_err(|_| "임시 폴더를 읽지 못했어요".to_owned())?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Err(refuse());
        }
    }
    Ok(())
}

/// 기준 폴더 아래 임시 폴더 하나. 통째로 최종 이름에 옮기거나(`publish`) 비운다(`discard`).
pub struct Stage {
    parent: OwnedFd,
    name: String,
    /// 임시 폴더를 기준으로 쓰는 쓰기 도구
    pub writer: Writer,
}

impl Stage {
    /// 임시 폴더의 이름(기준 폴더 아래)
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 임시 폴더를 치운다: **우리가 만든 이름만**(파일은 이름으로, 폴더는 빈 것만) 잡고 있는 fd 기준으로 지운다. 재귀로 쓸어 지우지 않는다 —
    /// 우리가 만들지 않은 것이 들어 있으면 그대로 두고 끝낸다(그 폴더도 남는다).
    pub fn discard(mut self) {
        self.writer.unlink_created_files();
        self.writer.undo();
        // 이름으로 지우므로 그 자리에 새로 생긴 빈 폴더를 지울 수 있지만 `rmdir`은 빈 폴더만 지운다(잃는 데이터 없음)
        let _ = unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
    }

    /// 임시 폴더를 `name`으로 원자적으로 옮긴다. 이미 있으면 덮어쓰지 않고 실패한다(실패하면 임시 폴더는 치운다). 권한은 바꾸지 않는다 — 만들 때
    /// 이미 최종 값(`0777 & !umask`)이다.
    ///
    /// 옮기기는 이름으로 하므로, 열어서 확인한 뒤에 임시 폴더 이름이 다른 것으로 바뀌었다면 바뀐 것이 `name`으로 게시된다. 같은 부모에 쓸 수 있는
    /// 쪽은 원래 그 이름(`name`)에 무엇이든 둘 수 있었으므로 권한이 늘어나는 일이 아니다.
    pub fn publish(self, name: &str) -> Result<(), String> {
        if !plain(name) {
            self.discard();
            return Err(format!("경로 조각이 올바르지 않아요: {name:?}"));
        }
        let moved = renameat_with(
            &self.parent,
            self.name.as_str(),
            &self.parent,
            name,
            RenameFlags::NOREPLACE,
        )
        .map_err(|errno| match errno {
            Errno::EXIST | Errno::NOTEMPTY => {
                format!("{name}이(가) 이미 있어요 — 덮어쓰지 않아요")
            }
            Errno::INVAL | Errno::NOSYS | Errno::NOTSUP => {
                "이 파일 시스템은 이름이 겹치면 거절하는 이름 바꾸기를 지원하지 않아요".to_owned()
            }
            error => format!("{name}(으)로 옮기지 못했어요: {error}"),
        });
        if moved.is_err() {
            self.discard();
        }
        moved
    }
}

#[cfg(test)]
mod tests;
