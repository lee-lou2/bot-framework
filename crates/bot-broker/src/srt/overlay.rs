//! Linux `bwrap` launcher wrapper that hides `srt`'s shared `/tmp/claude` directory behind a read-only tmpfs.
//!
//! On Linux, `srt` (bubblewrap) re-binds host `/tmp/claude` writable on top of `denyRead` (`denyWrite`
//! only makes it read-only, leaving contents visible), whereas macOS Seatbelt blocks reads via config.
//! Placing this wrapper at `bwrapPath` injects `--tmpfs` + `--remount-ro` immediately before the first
//! `--` when `/tmp/claude` exists, leaving the host directory untouched while hiding it inside the sandbox.

use std::{
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub(super) const SHARED_TMP: &str = "/tmp/claude";

fn needs_overlay(os: &str) -> bool {
    os == "linux"
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

pub(super) fn find_bwrap(path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_absolute() && is_executable(candidate))
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Generates a POSIX sh wrapper that masks `hide` with a read-only tmpfs before the first `--`, preserving all arguments verbatim.
pub(super) fn script(real: &Path, hide: &str) -> String {
    format!(
        "#!/bin/sh\n\
         real={real}\n\
         hide={hide}\n\
         if [ ! -d \"$hide\" ]; then exec \"$real\" \"$@\"; fi\n\
         n=$#\n\
         i=0\n\
         found=0\n\
         while [ \"$i\" -lt \"$n\" ]; do\n\
         \x20 a=$1\n\
         \x20 shift\n\
         \x20 i=$((i + 1))\n\
         \x20 if [ \"$found\" = 0 ] && [ \"$a\" = \"--\" ]; then\n\
         \x20   found=1\n\
         \x20   set -- \"$@\" --tmpfs \"$hide\" --remount-ro \"$hide\" --\n\
         \x20 else\n\
         \x20   set -- \"$@\" \"$a\"\n\
         \x20 fi\n\
         done\n\
         exec \"$real\" \"$@\"\n",
        real = sh_quote(&real.display().to_string()),
        hide = sh_quote(hide),
    )
}

/// Locates real `bwrap` on Linux; returns `None` on platforms that need no overlay or if `bwrap` is absent (in which case `srt` itself cannot spawn).
pub(super) fn locate(os: &str, path: &str) -> Option<PathBuf> {
    needs_overlay(os).then(|| find_bwrap(path)).flatten()
}

/// Writes an owner-only per-call launcher script into the call's `.srt` directory so it is cleaned up with the call.
pub(super) fn write_launcher(target: &Path, real: &Path) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(target)?;
    file.write_all(script(real, SHARED_TMP).as_bytes())
}
