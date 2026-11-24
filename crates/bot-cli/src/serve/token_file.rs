//! 관리 화면 운영자 비밀 파일. 경로 하나를 한 프로세스가 독점하고(잠금), 활성 판본 확정까지 성공했을 때만 확정한다.
//!
//! 경로 옆의 `<경로>.lock`에 배타 잠금(`flock`)을 걸어 한 프로세스만 이 경로로 발행한다(프로세스가 죽으면 잠금은 저절로 풀려 남은
//! 잠금 파일이 막지 않는다). 수명은 세 단계다:
//! 1. `claim` — 독점한다. 먼저 **이전 시작이 남긴 미완료 발행을 복구**한다(아래). 비밀 파일은 건드리지 않는다.
//! 2. `publish` — 비밀을 쓰기 **전에** 복구 기록(`.<이름>.journal`)에 이 발행이 쓰는 정확한 이름(빈 임시 파일·보관본, 무작위 조각이
//!    붙는다)과 두 파일의 (장치, inode)를 적는다: 발행할 파일, 그리고 기존 파일이 있으면 그 파일(원본). 그다음 비밀을 임시 파일에
//!    쓰고, 원본이 있으면 보관본으로 옮긴 뒤(옮긴 것이 기록한 원본인지 확인한다) 임시 파일을 경로로 바꿔 놓는다.
//! 3. `finalize` — 활성 판본 확정까지 성공했을 때만. 복구 기록을 먼저 지우고 보관본을 버린다. 기록을 못 지웠거나 보관본이 기록한
//!    원본(같은 inode)이 아니면 보관본은 남기고 알린다.
//!
//! 되돌리는 길은 하나(`restore`)다. 확정 전에 시작이 실패하면(`Drop`·발행 도중 실패) 그 자리에서, 확정 전에 강제 종료됐으면 다음
//! `claim`이 잠금을 잡은 뒤 같은 함수로 복구한다(잠금을 잡았다는 것은 발행하던 프로세스가 이미 없다는 뜻이다). **복구 기록이 가리키는
//! 정확한 파일만** 다룬다 — 이름 접두사만 보고 옆의 파일을 건드리지 않는다(운영자가 따로 만든 `.<이름>.bak-…` 같은 파일은 그대로다).
//! 원본은 (장치, inode)로 알아본다: 원본이 제자리에 있거나, 보관본에서 제자리로 돌려놓았음을 확인해야 복구가 끝난다. 확인하지 못하면
//! (경로가 그 사이 바뀌었거나 보관본이 없거나 다른 파일이다) 추측하지 않고 보관본·기록·경로의 파일을 **그대로 남긴 채** 시작을 멈춘다
//! (`claim`은 Err, 실패한 기동은 stderr 안내). 원본이 없었던 발행은 이 발행이 둔 파일만 지운다. 확정된 뒤의 강제 종료는 복구 기록이
//! 없어 건드리지 않는다(판본은 이미 새것이고 다음 발행이 파일을 바꾼다). 종료할 때는 **자기가 발행한 파일**만 지운다.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAGIC: &str = "bot-token-journal-v2";

/// 파일의 (장치, inode)
type Id = (u64, u64);

pub struct TokenFile {
    path: PathBuf,
    uid: u32,
    /// 잠금을 쥔 파일(놓으면 풀린다)
    _lock: File,
    /// 발행했고 아직 확정하지 못한 것(확정하면 비운다)
    pending: Option<Pending>,
    /// 발행한 파일. 확정한 뒤 종료할 때 이 파일만 지운다.
    published: Option<Id>,
}

struct Pending {
    journal: PathBuf,
    entry: Journal,
}

/// 기존 파일(원본): 옮겨 둘 보관본의 이름과 원본의 (장치, inode)
#[derive(Debug, Eq, PartialEq)]
struct Original {
    backup: String,
    id: Id,
}

/// 복구 기록의 내용: 이 발행이 쓴 정확한 이름들과 파일의 inode
#[derive(Debug, Eq, PartialEq)]
struct Journal {
    temp: String,
    /// 발행 전에 있던 파일(없었으면 None)
    original: Option<Original>,
    /// 발행할 파일(임시 파일과 같은 inode)
    published: Id,
}

fn field<'a>(lines: &mut std::str::Lines<'a>, key: &str) -> Option<&'a str> {
    lines.next()?.strip_prefix(key)?.strip_prefix('=')
}

fn parse_id(text: &str) -> Option<Id> {
    let (dev, ino) = text.split_once(':')?;
    Some((dev.parse().ok()?, ino.parse().ok()?))
}

impl Journal {
    fn render(&self) -> String {
        let (backup, original) = match &self.original {
            Some(o) => (o.backup.clone(), format!("{}:{}", o.id.0, o.id.1)),
            None => ("none".to_owned(), "none".to_owned()),
        };
        format!(
            "{MAGIC}\ntemp={}\nbackup={backup}\noriginal={original}\npublished={}:{}\n",
            self.temp, self.published.0, self.published.1
        )
    }

    /// 이 경로(`name`)의 발행이 쓸 수 있는 모양의 기록만 받는다. 이름은 이 경로의 접두사·무작위 조각 형식이어야 한다.
    fn parse(text: &str, name: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let temp = field(&mut lines, "temp")?;
        let backup = field(&mut lines, "backup")?;
        let original = field(&mut lines, "original")?;
        let published = parse_id(field(&mut lines, "published")?)?;
        if lines.next().is_some() {
            return None;
        }
        let owned = |value: &str, kind: &str| {
            let prefix = format!(".{name}.{kind}-");
            value.strip_prefix(&prefix).is_some_and(|nonce| {
                nonce.len() == 16 && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            })
        };
        if !owned(temp, "tmp") {
            return None;
        }
        let original = match (backup, original) {
            ("none", "none") => None,
            ("none", _) | (_, "none") => return None,
            (backup, id) => {
                if !owned(backup, "bak") {
                    return None;
                }
                Some(Original {
                    backup: backup.to_owned(),
                    id: parse_id(id)?,
                })
            }
        };
        Some(Self {
            temp: temp.to_owned(),
            original,
            published,
        })
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn journal_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{}.journal", file_name(path)))
}

/// 어떤 자리에 무엇이 있는가
#[derive(Debug, Eq, PartialEq)]
enum Slot {
    Absent,
    /// 일반 파일(그 inode)
    File(Id),
    /// 링크·폴더 등 일반 파일이 아닌 것
    Other,
}

fn slot(path: &Path) -> Result<Slot, String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Slot::File((meta.dev(), meta.ino()))),
        Ok(_) => Ok(Slot::Other),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Slot::Absent),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// 되돌리는 유일한 길: 기록이 가리키는 정확한 파일로 발행 전 상태를 만든다. 실패한 기동(`Drop`)·발행 도중 실패·다음 시작(`claim`)이
/// 모두 이 함수를 쓴다. 원본이 제자리에 있음을 확인하지 못하면 아무것도 지우지 않고(보관본·기록·경로의 파일 그대로) Err다.
fn restore(path: &Path, journal: &Path, entry: &Journal) -> Result<(), String> {
    let io = |at: &Path, e: std::io::Error| format!("{}: {e}", at.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    // 이 발행이 쓰던 임시 파일(발행할 파일과 같은 inode일 때만 이 발행의 것이다)
    let temp = dir.join(&entry.temp);
    if slot(&temp)? == Slot::File(entry.published) {
        std::fs::remove_file(&temp).map_err(|e| io(&temp, e))?;
    }
    let here = slot(path)?;
    match &entry.original {
        // 발행 전에는 파일이 없었다: 이 발행이 둔 죽은 비밀만 지운다(그 뒤에 운영자가 둔 파일은 그대로)
        None => {
            if here == Slot::File(entry.published) {
                std::fs::remove_file(path).map_err(|e| io(path, e))?;
            }
        }
        Some(original) => {
            let backup = dir.join(&original.backup);
            let kept = slot(&backup)? == Slot::File(original.id);
            let unconfirmed = || {
                format!(
                    "{}: 원래 파일을 되돌렸다고 확인할 수 없어요(보관본 {}이 없거나 다른 파일이에요) — 복구 기록({})은 그대로 둬요. 확인한 뒤 기록을 지우고 다시 시작하세요",
                    path.display(),
                    backup.display(),
                    journal.display()
                )
            };
            let ours_or_empty = here == Slot::Absent || here == Slot::File(entry.published);
            if here == Slot::File(original.id) {
                // 원래 파일이 제자리에 있다(옮기기 전에 멈췄거나 이미 되돌렸다)
            } else if kept && ours_or_empty {
                // 보관본이 기록한 그 원본이고 경로는 비었거나 이 발행이 둔 파일이다: 되돌린다
                std::fs::rename(&backup, path).map_err(|e| io(&backup, e))?;
                if slot(path)? != Slot::File(original.id) {
                    return Err(unconfirmed());
                }
            } else if kept {
                return Err(format!(
                    "{}: 확정 전에 파일이 바뀌어서(이 발행이 둔 것이 아니라서) 원래 파일을 되돌리지 않았어요 — 원래 파일은 보관본 {}에, 복구 기록은 {}에 그대로 있어요. 확인한 뒤 보관본을 직접 되돌리거나 기록을 지우고 다시 시작하세요",
                    path.display(),
                    backup.display(),
                    journal.display()
                ));
            } else {
                return Err(unconfirmed());
            }
        }
    }
    match std::fs::remove_file(journal) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io(journal, e)),
    }
}

/// 잠금을 잡은 뒤, 죽은 프로세스가 확정하지 못한 발행을 복구 기록대로 되돌린다. 기록이 없으면 아무것도 하지 않는다.
/// 복구할 수 없는 상태이면 Err(시작을 멈춘다).
fn recover(path: &Path, uid: u32) -> Result<(), String> {
    let journal = journal_path(path);
    match std::fs::symlink_metadata(&journal) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", journal.display())),
        Ok(meta) if !(meta.is_file() && meta.uid() == uid) => {
            return Err(format!(
                "{}: 복구 기록이 본인 소유의 일반 파일이 아니에요 — 확인한 뒤 지우고 다시 시작하세요",
                journal.display()
            ));
        }
        Ok(_) => {}
    }
    let text =
        std::fs::read_to_string(&journal).map_err(|e| format!("{}: {e}", journal.display()))?;
    let entry = Journal::parse(&text, &file_name(path)).ok_or_else(|| {
        format!(
            "{}: 복구 기록이 올바르지 않아요 — 확인한 뒤 지우고 다시 시작하세요",
            journal.display()
        )
    })?;
    restore(path, &journal, &entry)
}

/// 발행 도중 실패했다: 같은 복구 함수로 되돌린다. 되돌렸다고 확인하지 못하면 그 사실도 알린다.
fn rollback(path: &Path, journal: &Path, entry: &Journal, error: String) -> String {
    match restore(path, journal, entry) {
        Ok(()) => error,
        Err(left) => format!("{error} — 되돌리지 못했어요: {left}"),
    }
}

impl TokenFile {
    /// 경로를 독점한다. 폴더는 다른 사용자가 쓸 수 없어야 하고, 이미 있는 비밀 파일은 본인 소유의 일반 파일이어야 한다
    /// (링크·남의 파일은 거절). 다른 프로세스가 쥐고 있으면 Err — 그 프로세스의 파일은 그대로다. 잠금을 잡으면 이전 시작이 남긴
    /// 미완료 발행을 복구하고, 복구하지 못하면 Err다.
    pub fn claim(path: &Path, uid: u32) -> Result<Self, String> {
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        let parent = path
            .parent()
            .ok_or_else(|| "BOT_ADMIN_TOKEN_FILE에 폴더가 필요해요".to_owned())?;
        let dir = std::fs::metadata(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        if !dir.is_dir() || dir.mode() & 0o022 != 0 {
            return Err(format!(
                "{}: 비밀 파일의 폴더는 다른 사용자가 쓸 수 없어야 해요",
                parent.display()
            ));
        }
        match std::fs::symlink_metadata(path) {
            Ok(old) if old.is_file() && old.uid() == uid => {}
            Ok(_) => {
                return Err(format!(
                    "{}: 본인 소유의 일반 파일(링크 아님)만 바꿀 수 있어요",
                    path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io(e)),
        }
        let lock_path = PathBuf::from(format!("{}.lock", path.display()));
        match std::fs::symlink_metadata(&lock_path) {
            Ok(old) if old.is_file() && old.uid() == uid => {}
            Ok(_) => {
                return Err(format!(
                    "{}: 본인 소유의 일반 파일(링크 아님)이어야 해요",
                    lock_path.display()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", lock_path.display())),
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(|e| format!("{}: {e}", lock_path.display()))?;
        crate::lock::retry(|| lock.try_lock()).map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => format!(
                "{}: 다른 프로세스가 이 운영자 비밀 파일을 쓰고 있어요(그 파일은 그대로 둬요)",
                path.display()
            ),
            std::fs::TryLockError::Error(e) => format!("{}: {e}", lock_path.display()),
        })?;
        // 잠금을 쥐었으니 발행하던 프로세스는 이미 없다: 확정하지 못한 발행을 되돌린다
        recover(path, uid)?;
        Ok(Self {
            path: path.to_owned(),
            uid,
            _lock: lock,
            pending: None,
            published: None,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 새 비밀을 0600 임시 파일에 쓴 뒤 한 번에 바꿔 놓는다. 비밀을 쓰기 전에 복구 기록을 남기고 기존 파일은 그 기록의 보관본으로
    /// 옮긴다(기존 파일은 이 호출이 성공할 때만 바뀌고, 실패하면 되돌린다).
    pub fn publish(&mut self, token: &str) -> Result<(), String> {
        if self.pending.is_some() || self.published.is_some() {
            return Err("이미 발행한 비밀 파일이에요".to_owned());
        }
        let shown = self.path.display().to_string();
        let io = |e: std::io::Error| format!("{shown}: {e}");
        // 원본: 없거나 본인 소유의 일반 파일이어야 한다(독점한 뒤 링크 등으로 바뀌었으면 건드리지 않는다)
        let original = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.is_file() && meta.uid() == self.uid => Some((meta.dev(), meta.ino())),
            Ok(_) => {
                return Err(format!(
                    "{shown}: 본인 소유의 일반 파일(링크 아님)만 바꿀 수 있어요"
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        let name = file_name(&self.path);
        let nonce = crate::nonce::hex();
        let temp_name = format!(".{name}.tmp-{nonce}");
        let temp = self.path.with_file_name(&temp_name);
        // 비밀을 쓰기 전에 빈 임시 파일부터 만들어 inode를 알아 둔다: 복구 기록이 이 파일을 가리킨 뒤에야 비밀이 디스크에 닿는다
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(io)?;
        let published = match file.metadata() {
            Ok(meta) => (meta.dev(), meta.ino()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(io(e));
            }
        };
        let entry = Journal {
            temp: temp_name,
            original: original.map(|id| Original {
                backup: format!(".{name}.bak-{nonce}"),
                id,
            }),
            published,
        };
        let journal = journal_path(&self.path);
        // 이미 있는 기록은 복구되지 않은 이전 발행의 것이라 건드리지 않는다(새로 만든 기록만 지운다)
        let mut record = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&journal)
        {
            Ok(record) => record,
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                return Err(format!("{}: {e}", journal.display()));
            }
        };
        if let Err(e) = record
            .write_all(entry.render().as_bytes())
            .and_then(|()| record.sync_all())
        {
            return Err(rollback(&self.path, &journal, &entry, io(e)));
        }
        drop(record);
        let secret = file
            .write_all(format!("{token}\n").as_bytes())
            .and_then(|()| file.sync_all())
            .and_then(|()| std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600)));
        drop(file);
        if let Err(e) = secret {
            return Err(rollback(&self.path, &journal, &entry, io(e)));
        }
        if let Some(original) = &entry.original {
            let backup = self.path.with_file_name(&original.backup);
            if let Err(e) = std::fs::rename(&self.path, &backup) {
                return Err(rollback(&self.path, &journal, &entry, io(e)));
            }
            // 옮긴 것이 기록한 그 원본이어야 한다(그 사이 바꿔치기됐으면 `restore`가 확인하지 못해 그대로 남기고 멈춘다)
            if slot(&backup) != Ok(Slot::File(original.id)) {
                let error = format!("{shown}: 옮긴 파일이 기록한 원본과 달라요");
                return Err(rollback(&self.path, &journal, &entry, error));
            }
        }
        if let Err(e) = std::fs::rename(&temp, &self.path) {
            return Err(rollback(&self.path, &journal, &entry, io(e)));
        }
        self.published = Some(published);
        self.pending = Some(Pending { journal, entry });
        Ok(())
    }

    /// 활성 판본 확정까지 성공했다: 복구 기록(확정의 기준)을 먼저 지우고 보관본을 버린다. 이제부터 종료할 때 자기가 발행한 파일을
    /// 지운다. 기록을 지우지 못하면 보관본도 남기고(기록이 보관본을 가리킨다), 보관본이 기록한 원본(같은 inode)이 아니면 지우지 않는다.
    pub fn finalize(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match std::fs::remove_file(&pending.journal) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!(
                    "{}: 복구 기록을 지우지 못했어요({e}) — 보관본도 그대로 둬요",
                    pending.journal.display()
                );
                return;
            }
        }
        if let Some(original) = &pending.entry.original {
            let backup = self.path.with_file_name(&original.backup);
            match slot(&backup) {
                // 이 발행이 옮겨 둔 원본(같은 inode)만 버린다
                Ok(Slot::File(id)) if id == original.id => {
                    let _ = std::fs::remove_file(&backup);
                }
                Ok(Slot::Absent) => {}
                // 그 사이 다른 파일로 바뀌었다: 지우지 않고 알린다
                _ => eprintln!(
                    "{}: 보관본이 이 발행이 옮겨 둔 원본과 달라서 지우지 않았어요 — 확인한 뒤 직접 지우세요",
                    backup.display()
                ),
            }
        }
    }

    /// 시험용: 강제 종료를 흉내 낸다 — 정리 없이 잠금만 놓는다(복구 기록·보관본·발행한 파일은 그대로 남는다).
    #[cfg(test)]
    pub fn abandon(mut self) {
        self.published = None;
        self.pending = None;
    }
}

impl Drop for TokenFile {
    /// 확정하지 못한 발행(기동 실패)은 다음 시작과 같은 복구 함수로 되돌린다. 원본이 돌아왔다고 확인하지 못하면 보관본·기록을 남기고
    /// 알린다. 확정한 뒤에는 자기가 발행한 파일만 지운다. 잠금은 이 값이 사라질 때 풀린다.
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            if let Err(e) = restore(&self.path, &pending.journal, &pending.entry) {
                eprintln!("운영자 비밀 파일을 원래대로 되돌리지 못했어요: {e}");
            }
        } else if let Some(published) = self.published
            && slot(&self.path) == Ok(Slot::File(published))
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests;
