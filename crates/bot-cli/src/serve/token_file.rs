//! Locked, journal-backed admin operator token file with atomic publish and rollback.
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAGIC: &str = "bot-token-journal-v2";

/// File identity `(dev, ino)` used to verify ownership across crash recovery and cleanup.
type Id = (u64, u64);

pub struct TokenFile {
    path: PathBuf,
    uid: u32,
    _lock: File,
    pending: Option<Pending>,
    published: Option<Id>,
}

struct Pending {
    journal: PathBuf,
    entry: Journal,
}

#[derive(Debug, Eq, PartialEq)]
struct Original {
    backup: String,
    id: Id,
}

#[derive(Debug, Eq, PartialEq)]
struct Journal {
    temp: String,
    original: Option<Original>,
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

#[derive(Debug, Eq, PartialEq)]
enum Slot {
    Absent,
    File(Id),
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

/// Single rollback path shared by `claim` recovery, mid-publish failures, and `Drop`.
/// Leaves journal, backup, and target untouched if the original `(dev, ino)` cannot be verified.
fn restore(path: &Path, journal: &Path, entry: &Journal) -> Result<(), String> {
    let io = |at: &Path, e: std::io::Error| format!("{}: {e}", at.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    let temp = dir.join(&entry.temp);
    if slot(&temp)? == Slot::File(entry.published) {
        std::fs::remove_file(&temp).map_err(|e| io(&temp, e))?;
    }
    let here = slot(path)?;
    match &entry.original {
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
                    "{}: cannot verify original file restoration (backup {} is missing or changed) — leaving journal {} intact; inspect and remove the journal before restarting",
                    path.display(),
                    backup.display(),
                    journal.display()
                )
            };
            let ours_or_empty = here == Slot::Absent || here == Slot::File(entry.published);
            if here == Slot::File(original.id) {
                // Original file is already in place.
            } else if kept && ours_or_empty {
                std::fs::rename(&backup, path).map_err(|e| io(&backup, e))?;
                if slot(path)? != Slot::File(original.id) {
                    return Err(unconfirmed());
                }
            } else if kept {
                return Err(format!(
                    "{}: target changed before finalization (not written by this process), so original file was not restored — backup remains at {} and journal at {}; restore or inspect manually then remove the journal",
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

fn recover(path: &Path, uid: u32) -> Result<(), String> {
    let journal = journal_path(path);
    match std::fs::symlink_metadata(&journal) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("{}: {e}", journal.display())),
        Ok(meta) if !(meta.is_file() && meta.uid() == uid) => {
            return Err(format!(
                "{}: recovery journal must be a regular file owned by the current user — inspect and remove it before restarting",
                journal.display()
            ));
        }
        Ok(_) => {}
    }
    let text =
        std::fs::read_to_string(&journal).map_err(|e| format!("{}: {e}", journal.display()))?;
    let entry = Journal::parse(&text, &file_name(path)).ok_or_else(|| {
        format!(
            "{}: invalid recovery journal — inspect and remove it before restarting",
            journal.display()
        )
    })?;
    restore(path, &journal, &entry)
}

fn rollback(path: &Path, journal: &Path, entry: &Journal, error: String) -> String {
    match restore(path, journal, entry) {
        Ok(()) => error,
        Err(left) => format!("{error} — rollback failed: {left}"),
    }
}

impl TokenFile {
    /// Exclusively locks `<path>.lock` and recovers any unfinalized publish left by a crashed process.
    pub fn claim(path: &Path, uid: u32) -> Result<Self, String> {
        let io = |e: std::io::Error| format!("{}: {e}", path.display());
        let parent = path
            .parent()
            .ok_or_else(|| "BOT_ADMIN_TOKEN_FILE must have a parent directory".to_owned())?;
        let dir = std::fs::metadata(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        if !dir.is_dir() || dir.mode() & 0o022 != 0 {
            return Err(format!(
                "{}: token file directory must not be writable by other users",
                parent.display()
            ));
        }
        match std::fs::symlink_metadata(path) {
            Ok(old) if old.is_file() && old.uid() == uid => {}
            Ok(_) => {
                return Err(format!(
                    "{}: can only replace a regular file (not a symlink) owned by the current user",
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
                    "{}: must be a regular file (not a symlink) owned by the current user",
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
                "{}: another process is already using this admin token file (leaving it untouched)",
                path.display()
            ),
            std::fs::TryLockError::Error(e) => format!("{}: {e}", lock_path.display()),
        })?;
        // Holding the exclusive lock guarantees no prior publisher is running.
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

    /// Writes `token` via a `0600` temp file after recording temp/backup inodes in the recovery journal.
    pub fn publish(&mut self, token: &str) -> Result<(), String> {
        if self.pending.is_some() || self.published.is_some() {
            return Err("token file has already been published".to_owned());
        }
        let shown = self.path.display().to_string();
        let io = |e: std::io::Error| format!("{shown}: {e}");
        let original = match std::fs::symlink_metadata(&self.path) {
            Ok(meta) if meta.is_file() && meta.uid() == self.uid => Some((meta.dev(), meta.ino())),
            Ok(_) => {
                return Err(format!(
                    "{shown}: can only replace a regular file (not a symlink) owned by the current user"
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io(e)),
        };
        let name = file_name(&self.path);
        let nonce = crate::nonce::hex();
        let temp_name = format!(".{name}.tmp-{nonce}");
        let temp = self.path.with_file_name(&temp_name);
        // Create the empty temp file first to capture its `(dev, ino)` in the journal before writing the secret.
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
            if slot(&backup) != Ok(Slot::File(original.id)) {
                let error = format!("{shown}: moved file no longer matches recorded original");
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

    /// Commits a published token once activation succeeds: removes the journal first, then drops the backup if its inode matches.
    pub fn finalize(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match std::fs::remove_file(&pending.journal) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                eprintln!(
                    "{}: failed to remove recovery journal ({e}) — leaving backup intact",
                    pending.journal.display()
                );
                return;
            }
        }
        if let Some(original) = &pending.entry.original {
            let backup = self.path.with_file_name(&original.backup);
            match slot(&backup) {
                Ok(Slot::File(id)) if id == original.id => {
                    let _ = std::fs::remove_file(&backup);
                }
                Ok(Slot::Absent) => {}
                _ => eprintln!(
                    "{}: backup does not match the recorded original, leaving it intact — inspect and remove manually",
                    backup.display()
                ),
            }
        }
    }

    #[cfg(test)]
    pub fn abandon(mut self) {
        self.published = None;
        self.pending = None;
    }
}

impl Drop for TokenFile {
    fn drop(&mut self) {
        if let Some(pending) = self.pending.take() {
            if let Err(e) = restore(&self.path, &pending.journal, &pending.entry) {
                eprintln!("failed to restore original admin token file: {e}");
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
