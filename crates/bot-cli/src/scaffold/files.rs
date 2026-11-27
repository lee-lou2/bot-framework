//! Race-safe, no-overwrite directory and file writer anchored to a single opened root fd.
//!
//! - Opens the user-provided root once (honoring symlinks chosen by the user in that path) and performs
//!   all subsequent operations relative to that fd (`openat`/`mkdirat` with `O_NOFOLLOW`) so later swaps
//!   cannot redirect writes outside the root.
//! - Creates files exclusively (`O_EXCL`). `undo` only `rmdir`s empty directories we created and never
//!   unlinks user-visible paths by name.
//! - Whole trees use `Stage`: built in a random staging dir created with final permissions (`0777 & !umask`),
//!   verified for owner/mode/emptiness before adoption, and published atomically via `renameat2(NOREPLACE)`.

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
    root: OwnedFd,
    dirs: Vec<Vec<String>>,
    files: Vec<String>,
}

fn plain(part: &str) -> bool {
    !part.is_empty() && !matches!(part, "." | "..") && !part.contains(['/', '\\', '\0'])
}

fn dir_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn kind(stat: &Stat) -> FileType {
    FileType::from_raw_mode(stat.st_mode)
}

fn clone(fd: &OwnedFd) -> Result<OwnedFd, String> {
    fd.try_clone()
        .map_err(|_| "failed to clone directory fd".to_owned())
}

/// Exclusive `flock` guard held on a directory fd; releases on drop without creating lock files.
pub struct DirLock(OwnedFd);

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}

const TOOL_TMP: &str = ".bot-add-tool.tmp";

/// Removes a leftover `.bot-add-tool.tmp` from a prior `SIGKILL` only if it is an owned regular file (`nlink == 1`).
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
                "temporary file ({TOOL_TMP}) for {rel} is a symlink — refusing to overwrite"
            ));
        }
        Err(error) => {
            return Err(format!(
                "failed to inspect temporary file ({TOOL_TMP}) for {rel}: {error}"
            ));
        }
    };
    let stat =
        fstat(&fd).map_err(|_| format!("failed to stat temporary file ({TOOL_TMP}) for {rel}"))?;
    let mine = u64::from(stat.st_uid) == u64::from(geteuid().as_raw());
    if kind(&stat) != FileType::RegularFile || stat.st_nlink != 1 || !mine {
        return Err(format!(
            "temporary file ({TOOL_TMP}) for {rel} is not a safe regular file — refusing to remove"
        ));
    }
    drop(fd);
    unlinkat(dir, TOOL_TMP, AtFlags::empty()).map_err(|error| {
        format!("failed to remove stale temporary file ({TOOL_TMP}) for {rel}: {error}")
    })
}

impl Writer {
    pub fn lock_dir(&self, parts: &[&str]) -> Result<DirLock, String> {
        let (depth, dir) = self.present(parts)?;
        if depth < parts.len() {
            return Err(format!("directory {} does not exist", parts.join("/")));
        }
        loop {
            match flock(&dir, FlockOperation::LockExclusive) {
                Ok(()) => return Ok(DirLock(dir)),
                Err(Errno::INTR) => continue,
                Err(error) => {
                    return Err(format!(
                        "failed to lock directory {}: {error}",
                        parts.join("/")
                    ));
                }
            }
        }
    }

    pub fn new(root: &Path) -> Result<Self, String> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = open(root, flags, Mode::empty()).map_err(|errno| match errno {
            Errno::NOENT => format!(
                "directory {} does not exist — create it first",
                root.display()
            ),
            Errno::NOTDIR => format!("{} is not a directory", root.display()),
            _ => format!("failed to open directory {}", root.display()),
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

    /// Walks existing directory components under `root`, refusing symlinks and non-directories.
    fn present(&self, parts: &[&str]) -> Result<(usize, OwnedFd), String> {
        let mut dir = clone(&self.root)?;
        for (index, part) in parts.iter().enumerate() {
            if !plain(part) {
                return Err(format!("invalid path segment: {part:?}"));
            }
            match openat(&dir, *part, dir_flags(), Mode::empty()) {
                Ok(next) => dir = next,
                Err(Errno::NOENT) => return Ok((index, dir)),
                // Platforms report symlinks as ELOOP or ENOTDIR when `O_DIRECTORY | O_NOFOLLOW` is set.
                Err(Errno::LOOP | Errno::NOTDIR) => {
                    let shown = parts[..=index].join("/");
                    let link = statat(&dir, *part, AtFlags::SYMLINK_NOFOLLOW)
                        .is_ok_and(|stat| kind(&stat) == FileType::Symlink);
                    return Err(if link {
                        format!("{shown} is a symlink — refusing to follow")
                    } else {
                        format!("{shown} is not a directory")
                    });
                }
                Err(_) => {
                    return Err(format!("failed to inspect {}", parts[..=index].join("/")));
                }
            }
        }
        Ok((parts.len(), dir))
    }

    pub fn exists(&self, parts: &[&str]) -> Result<bool, String> {
        let Some((last, parents)) = parts.split_last() else {
            return Ok(false);
        };
        if !plain(last) {
            return Err(format!("invalid path segment: {last:?}"));
        }
        let (depth, dir) = self.present(parents)?;
        if depth < parents.len() {
            return Ok(false);
        }
        match statat(&dir, *last, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(_) => Err(format!("failed to inspect {}", parts.join("/"))),
        }
    }

    pub fn require_absent(&self, parts: &[&str]) -> Result<(), String> {
        if self.exists(parts)? {
            return Err(format!(
                "{} already exists — refusing to overwrite",
                parts.join("/")
            ));
        }
        Ok(())
    }

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

    pub fn child_dirs(&self, parts: &[&str]) -> Result<Vec<String>, String> {
        let (depth, dir) = self.present(parts)?;
        if depth < parts.len() {
            return Ok(Vec::new());
        }
        let entries =
            Dir::read_from(&dir).map_err(|_| format!("failed to read {}", parts.join("/")))?;
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

    /// Reads an existing regular file without following symlinks or blocking on FIFOs; refuses hardlinks and oversize files.
    pub fn read_regular(&self, parts: &[&str], max_bytes: u64) -> Result<Vec<u8>, String> {
        let Some((last, parents)) = parts.split_last() else {
            return Err("empty path".into());
        };
        if !plain(last) {
            return Err(format!("invalid path segment: {last:?}"));
        }
        let shown = parts.join("/");
        let (depth, dir) = self.present(parents)?;
        if depth < parents.len() {
            return Err(format!("{shown} does not exist"));
        }
        let fd = openat(
            &dir,
            *last,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::NOENT => format!("{shown} does not exist"),
            Errno::LOOP => format!("{shown} is a symlink — refusing to follow"),
            error => format!("failed to open {shown}: {error}"),
        })?;
        let stat = fstat(&fd).map_err(|_| format!("failed to stat {shown}"))?;
        if kind(&stat) != FileType::RegularFile {
            return Err(format!("{shown} is not a regular file"));
        }
        if stat.st_nlink > 1 {
            return Err(format!("{shown} is a hardlink — refusing to modify"));
        }
        if stat.st_size < 0 || stat.st_size as u64 > max_bytes {
            return Err(format!("{shown} exceeds size limit ({max_bytes} bytes)"));
        }
        let mut buf = Vec::new();
        File::from(fd)
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut buf)
            .map_err(|error| format!("failed to read {shown}: {error}"))?;
        if buf.len() as u64 > max_bytes {
            return Err(format!("{shown} exceeds size limit ({max_bytes} bytes)"));
        }
        Ok(buf)
    }

    /// Atomically replaces an existing regular file while preserving its mode; caller must hold `DirLock` on the parent directory.
    pub fn replace_regular(&self, rel: &str, content: &str) -> Result<(), String> {
        let parts: Vec<&str> = rel.split('/').collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Err("empty path".into());
        };
        if !parts.iter().all(|part| plain(part)) {
            return Err(format!("invalid path: {rel:?}"));
        }
        let (depth, dir) = self.present(dirs)?;
        if depth < dirs.len() {
            return Err(format!("parent directory of {rel} does not exist"));
        }
        let existing_fd = openat(
            &dir,
            *name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| match errno {
            Errno::LOOP => format!("{rel} is a symlink — refusing to overwrite"),
            Errno::NOENT => format!("{rel} does not exist"),
            _ => format!("failed to inspect {rel}"),
        })?;
        let stat = fstat(&existing_fd).map_err(|_| format!("failed to stat {rel}"))?;
        if kind(&stat) != FileType::RegularFile {
            return Err(format!("{rel} is not a regular file"));
        }
        if stat.st_nlink > 1 {
            return Err(format!("{rel} is a hardlink — refusing to overwrite"));
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
        .map_err(|error| format!("failed to create temporary file for {rel}: {error}"))?;
        let mut file = File::from(fd);
        if let Err(error) = file
            .write_all(content.as_bytes())
            .and_then(|()| file.sync_all())
        {
            drop(file);
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            return Err(format!("failed to write {rel}: {error}"));
        }
        if let Err(error) = fchmod(&file, existing_mode) {
            drop(file);
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            return Err(format!("failed to preserve permissions on {rel}: {error}"));
        }
        drop(file);
        renameat(&dir, TOOL_TMP, &dir, *name).map_err(|error| {
            let _ = unlinkat(&dir, TOOL_TMP, AtFlags::empty());
            format!("failed to replace {rel}: {error}")
        })
    }

    pub fn write(&mut self, rel: &str, content: &str) -> Result<(), String> {
        let parts: Vec<&str> = rel.split('/').collect();
        let Some((name, dirs)) = parts.split_last() else {
            return Err("empty path".into());
        };
        if !parts.iter().all(|part| plain(part)) {
            return Err(format!("invalid path: {rel:?}"));
        }
        let (present, mut dir) = self.present(dirs)?;
        for (index, part) in dirs.iter().enumerate().skip(present) {
            let shown = dirs[..=index].join("/");
            mkdirat(&dir, *part, Mode::from_raw_mode(0o777))
                .map_err(|error| format!("failed to create directory {shown}: {error}"))?;
            self.dirs
                .push(dirs[..=index].iter().map(|p| (*p).to_owned()).collect());
            // Re-open with O_NOFOLLOW to catch a swap between mkdirat and traversal.
            dir = openat(&dir, *part, dir_flags(), Mode::empty())
                .map_err(|_| format!("failed to open directory {shown}"))?;
        }
        let file = openat(
            &dir,
            *name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o666),
        )
        .map_err(|errno| match errno {
            Errno::EXIST => format!("{rel} already exists — refusing to overwrite"),
            error => format!("failed to create {rel}: {error}"),
        })?;
        self.files.push(rel.to_owned());
        File::from(file)
            .write_all(content.as_bytes())
            .map_err(|error| format!("failed to write {rel}: {error}"))
    }

    /// Removes only empty directories created by this writer and returns created file paths without unlinking them.
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

    pub fn created(&self) -> Vec<String> {
        let mut files = self.files.clone();
        files.sort();
        files
    }

    pub fn stage(&self) -> Result<Stage, String> {
        self.stage_with(|_| {})
    }

    fn stage_with(&self, between: impl Fn(&str)) -> Result<Stage, String> {
        for _ in 0..32u32 {
            let name = format!(
                ".bot-init-{}-{}",
                std::process::id(),
                bot_core::random::random_hex(8)
            );
            // Serialize umask read + mkdirat so `expected` matches the mode actually applied.
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
                    // Refuse without unlinking if opening or ownership/mode/emptiness verification fails.
                    let fd = openat(&self.root, name.as_str(), dir_flags(), Mode::empty())
                        .map_err(|_| "failed to open staging directory".to_owned())?;
                    check_ours(&fd, expected)?;
                    return Ok(Stage {
                        parent: clone(&self.root)?,
                        name,
                        writer: Writer::at(fd),
                    });
                }
                Err(Errno::EXIST) => {}
                Err(error) => return Err(format!("failed to create staging directory: {error}")),
            }
        }
        Err("failed to allocate a staging directory name".into())
    }

    /// Unlinks files created inside a private staging directory (never used on user directories).
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

/// Computes `0777 & !umask`; caller must hold `UMASK` because `umask(2)` is process-global.
fn usual_mode() -> u64 {
    let current = umask(Mode::empty());
    umask(current);
    u64::from(Mode::from_raw_mode(0o777).difference(current).bits())
}

static UMASK: Mutex<()> = Mutex::new(());

fn made_by_us(kind: FileType, owner: u64, mode: u64, me: u64, expected: u64) -> bool {
    kind == FileType::Directory && owner == me && mode & 0o777 == expected
}

/// Verifies a newly opened staging fd is an empty directory owned by us with the expected umask-derived mode.
fn check_ours(fd: &OwnedFd, expected: u64) -> Result<(), String> {
    let refuse = || {
        "staging directory was not created by us (owner, mode, or contents differ) — stopped without deleting anything".to_owned()
    };
    let stat = fstat(fd).map_err(|_| "failed to stat staging directory".to_owned())?;
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
    let entries = Dir::read_from(fd).map_err(|_| "failed to read staging directory".to_owned())?;
    for entry in entries {
        let entry = entry.map_err(|_| "failed to read staging directory".to_owned())?;
        if !matches!(entry.file_name().to_bytes(), b"." | b"..") {
            return Err(refuse());
        }
    }
    Ok(())
}

pub struct Stage {
    parent: OwnedFd,
    name: String,
    pub writer: Writer,
}

impl Stage {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Removes only paths created by this stage (non-recursively) so foreign files are left untouched.
    pub fn discard(mut self) {
        self.writer.unlink_created_files();
        self.writer.undo();
        let _ = unlinkat(&self.parent, self.name.as_str(), AtFlags::REMOVEDIR);
    }

    /// Atomically renames the stage to `name` via `NOREPLACE`, discarding on failure without modifying mode bits.
    pub fn publish(self, name: &str) -> Result<(), String> {
        if !plain(name) {
            self.discard();
            return Err(format!("invalid path segment: {name:?}"));
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
                format!("{name} already exists — refusing to overwrite")
            }
            Errno::INVAL | Errno::NOSYS | Errno::NOTSUP => {
                "filesystem does not support non-replacing atomic rename".to_owned()
            }
            error => format!("failed to rename to {name}: {error}"),
        });
        if moved.is_err() {
            self.discard();
        }
        moved
    }
}

#[cfg(test)]
mod tests;
