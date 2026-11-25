//! Linux: srt가 모든 실행에 쓰기 가능으로 내주는 공용 임시 폴더(`/tmp/claude`)를 도구가 **읽지도 못하게** 가린다.
//!
//! srt(bubblewrap)는 `denyRead`로 이 폴더에 tmpfs를 덮고도, «샌드박스가 스스로 쓰는 곳»이라며 그 위에 실제 폴더를 쓰기 가능으로 다시 붙인다(`denyWrite`는 그것을
//! 읽기 전용으로 바꿀 뿐이라 내용은 그대로 보인다). srt는 이 경로를 설정에서 뺄 수 없어서 Linux에서는 설정만으로 읽기를 막지 못한다(macOS Seatbelt는 막는다).
//! 그래서 srt가 부르는 `bwrap` 자리(srt 설정 `bwrapPath`)에 얇은 실행 파일을 둔다: 모든 마운트가 끝난 뒤(명령 앞의 첫 `--`)에 같은 자리를 빈 읽기 전용
//! tmpfs로 다시 덮고 실제 `bwrap`을 부른다. 호스트의 폴더는 그대로이고 샌드박스 안에서만 비어 보인다. 폴더가 없으면 아무것도 덮지 않는다(srt도 없는
//! 쓰기 경로는 마운트하지 않고 읽기 전용 루트 위에서는 도구가 만들 수도 없다).

use std::{
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// srt가 모든 실행에 쓰기 가능으로 내주는 공용 임시 폴더.
pub(super) const SHARED_TMP: &str = "/tmp/claude";

/// 이 운영체제에서 srt 설정만으로는 공용 임시 폴더 읽기를 막지 못하는가(Linux의 bubblewrap).
fn needs_overlay(os: &str) -> bool {
    os == "linux"
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// `PATH` 순서대로 처음 나오는 실행 가능한 `bwrap`(srt가 찾는 것과 같다).
pub(super) fn find_bwrap(path: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join("bwrap"))
        .find(|candidate| candidate.is_absolute() && is_executable(candidate))
}

fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// 실제 `bwrap`을 부르되, `hide`가 폴더이면 명령 앞의 첫 `--` 바로 앞에 `hide`를 빈 읽기 전용 tmpfs로 덮는 인자를 끼워 넣는 sh 스크립트.
/// `--`가 없는 호출(`--version`)과 폴더가 없을 때는 인자를 그대로 넘긴다. 인자는 따옴표째 보존한다(공백·빈 인자 포함).
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

/// 이 호스트에서 srt가 부를 실제 `bwrap`. 설정만으로 막히는 운영체제이거나 `bwrap`을 찾지 못하면 `None`이다(srt가 어차피 띄우지 못하니 격리가 약해진 채 도는
/// 일은 없다).
pub(super) fn locate(os: &str, path: &str) -> Option<PathBuf> {
    needs_overlay(os).then(|| find_bwrap(path)).flatten()
}

/// 호출 하나가 쓸 대리 실행 파일을 `target`에 만든다(소유자만 읽고 실행한다). 호출마다 따로라 서로 덮어쓰거나 남아서 다른 호출에 쓰이는 일이 없고, 호출의
/// 메타 폴더(`.srt`)에 같이 있다가 호출과 함께 지워진다.
pub(super) fn write_launcher(target: &Path, real: &Path) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(target)?;
    file.write_all(script(real, SHARED_TMP).as_bytes())
}
