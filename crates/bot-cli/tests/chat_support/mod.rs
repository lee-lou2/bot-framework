//! Shared test fixtures for `bot chat` process tests: fake Unix-socket gateway and terminal control-byte detector.
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

/// Upper bound waiting for a connection; freshly built binaries can be slow on first launch (e.g. macOS first-run checks).
/// Tests expecting zero connections terminate immediately via `Gateway::join`.
const ACCEPT_WAIT: Duration = Duration::from_secs(30);
const READ_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Default, Eq, PartialEq)]
pub struct Seen {
    pub connections: usize,
    pub request: String,
}

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

/// Fake gateway thread handle; calling `join` signals the child process has exited so the accept loop stops immediately.
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

/// Accepts one connection on `socket`, reads a single request line, writes `replies` line by line, and closes.
/// `folder_mode` sets parent directory permissions (`0o700` is trusted; wider modes simulate an untrusted directory).
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

/// Returns offsets of raw terminal control bytes: C0 (excluding `\n` and `\t`), DEL, and UTF-8 C1 (U+0080-U+009F).
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
