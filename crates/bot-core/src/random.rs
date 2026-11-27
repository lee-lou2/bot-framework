//! OS CSPRNG(`uuid::Uuid::new_v4`) 기반 무작위 바이트와 소문자 16진 문자열 생성.
//!
//! UUID v4의 고정된 버전·변형 비트가 출력에 편향을 남기지 않도록, 32바이트 블록마다
//! `Uuid::new_v4()` 두 개(244비트 엔트로피)를 SHA-256으로 해시해 균등한 바이트를 만든다.

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// 슬라이스 전체를 균등한 무작위 바이트로 채운다.
pub fn fill(out: &mut [u8]) {
    for chunk in out.chunks_mut(32) {
        let mut hasher = Sha256::new();
        hasher.update(Uuid::new_v4().as_bytes());
        hasher.update(Uuid::new_v4().as_bytes());
        let digest = hasher.finalize();
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
}

/// 길이 `N`의 균등한 무작위 바이트 배열을 만든다.
pub fn bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    fill(&mut out);
    out
}

/// 바이트 슬라이스를 소문자 16진 문자열로 바꾼다.
pub fn hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

/// `byte_len` 바이트의 무작위 값을 소문자 16진 문자열(`2 * byte_len`자)로 만든다.
pub fn random_hex(byte_len: usize) -> String {
    let mut buf = vec![0u8; byte_len];
    fill(&mut buf);
    hex(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_encodes_bytes_in_lowercase() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0a, 0xab, 0xff]), "000aabff");
    }

    #[test]
    fn bytes_produces_distinct_full_width_entropy_without_uuid_bit_bias() {
        assert_eq!(bytes::<0>(), [0u8; 0]);
        let a = bytes::<16>();
        let b = bytes::<16>();
        assert_ne!(a, b);
        let wide_a = bytes::<48>();
        let wide_b = bytes::<48>();
        assert_ne!(wide_a, wide_b);
        // RFC 4122 UUID v4는 6번째 바이트 상위 니블이 항상 0x4로 고정된다.
        // SHA-256으로 섞었으므로 여러 표본에서 0x4 이외의 값도 나와야 한다.
        assert!((0..64).any(|_| (bytes::<16>()[6] >> 4) != 0x4));
    }

    #[test]
    fn random_hex_has_expected_length_and_charset() {
        assert_eq!(random_hex(0), "");
        let a = random_hex(16);
        let b = random_hex(16);
        assert_eq!(a.len(), 32);
        assert_eq!(b.len(), 32);
        assert_ne!(a, b);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
    }
}
