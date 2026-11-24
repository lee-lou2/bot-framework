//! 이름에 붙이는 무작위 조각(임시·보관 파일이 서로, 그리고 운영자가 따로 만든 파일과 겹치지 않게 한다).
use std::{fs::File, io::Read, time::SystemTime};

/// 운영체제 난수 8바이트의 16진 16자리. 난수를 읽을 수 없으면 시각·프로세스 번호로 채운다.
pub fn hex() -> String {
    let mut bytes = [0u8; 8];
    let filled = File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut bytes))
        .is_ok();
    if !filled {
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes.copy_from_slice(&(nanos as u64 ^ u64::from(std::process::id())).to_be_bytes());
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn nonces_are_sixteen_hex_digits_and_differ() {
        let (a, b) = (super::hex(), super::hex());
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
