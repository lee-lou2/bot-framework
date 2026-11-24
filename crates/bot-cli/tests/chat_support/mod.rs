//! `bot chat` 프로세스 시험의 공용 장치: 유닉스 소켓에서 한 연결을 받는 가짜 게이트웨이와 터미널 제어 문자 찾기.
#![allow(dead_code)]
use serde_json::Value;
use std::{
    io::{Read, Write},
    os::unix::{
        fs::{DirBuilderExt, PermissionsExt},
        net::UnixListener,
    },
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// 연결을 기다리는 최대 시간: 막 새로 만든 실행 파일의 첫 실행은 느릴 수 있다(macOS의 첫 실행 검사·과부하). 접속이 없어야 하는 시험은 `join`이 곧바로 끝낸다.
const ACCEPT_WAIT: Duration = Duration::from_secs(30);
/// 접속한 쪽이 요청 한 줄을 보내기를 기다리는 최대 시간
const READ_WAIT: Duration = Duration::from_secs(5);

/// 가짜 게이트웨이가 본 것
#[derive(Debug, Default, Eq, PartialEq)]
pub struct Seen {
    /// 받은 연결 수
    pub connections: usize,
    /// 첫 연결에서 읽은 요청 줄(없으면 빈 값)
    pub request: String,
}

/// 소켓 폴더가 없으면 `folder_mode`로 만들어(상위 폴더도) 그 권한을 확실히 맞춘다.
pub fn make_folder(folder: &Path, folder_mode: u32) {
    if !folder.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(folder_mode)
            .create(folder)
            .unwrap();
    }
    std::fs::set_permissions(folder, std::fs::Permissions::from_mode(folder_mode)).unwrap();
}

/// 가짜 게이트웨이 스레드. `join`은 «시험이 돌린 프로세스가 끝났다»는 신호이기도 하다 — 그때까지 연결이 하나도 오지 않았으면 더 기다리지 않고 끝난다.
pub struct Gateway {
    handle: JoinHandle<Seen>,
    done: Arc<AtomicBool>,
}

impl Gateway {
    pub fn join(self) -> std::thread::Result<Seen> {
        self.done.store(true, Ordering::SeqCst);
        self.handle.join()
    }
}

/// `socket`에서 한 연결을 받아 요청 한 줄을 읽고 `replies`를 한 줄씩 돌려준 뒤 닫는 가짜 게이트웨이. 연결을 기다리는 것은 `ACCEPT_WAIT`까지이고,
/// 시험이 `join`하면(돌린 프로세스가 끝난 뒤) 연결이 없을 때 곧바로 끝난다.
/// 소켓 폴더는 `folder_mode`로 만든다(0700이 정상이고, 그 밖의 값은 «믿을 수 없는 폴더»를 만든다).
pub fn fake_gateway(socket: &Path, folder_mode: u32, replies: Vec<Value>) -> Gateway {
    make_folder(socket.parent().unwrap(), folder_mode);
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    let handle = std::thread::spawn(move || {
        let mut seen = Seen::default();
        let deadline = Instant::now() + ACCEPT_WAIT;
        while Instant::now() < deadline && !finished.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    seen.connections += 1;
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(READ_WAIT));
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    while let Ok(1) = stream.read(&mut byte) {
                        line.push(byte[0]);
                        if byte[0] == b'\n' {
                            break;
                        }
                    }
                    seen.request = String::from_utf8_lossy(&line).into_owned();
                    if !line.is_empty() {
                        for reply in replies {
                            let _ = writeln!(stream, "{reply}");
                        }
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        seen
    });
    Gateway { handle, done }
}

/// 터미널이 명령으로 읽을 바이트: C0(줄바꿈·탭 제외)·DEL, 그리고 UTF-8로 된 C1(U+0080–U+009F)
pub fn raw_controls(bytes: &[u8]) -> Vec<String> {
    let mut found = Vec::new();
    for (at, byte) in bytes.iter().enumerate() {
        let c0 = *byte < 0x20 && *byte != b'\n' && *byte != b'\t';
        let c1 = *byte == 0xc2
            && bytes
                .get(at + 1)
                .is_some_and(|next| (0x80..=0x9f).contains(next));
        if c0 || *byte == 0x7f || c1 {
            found.push(format!("{byte:#04x}@{at}"));
        }
    }
    found
}
