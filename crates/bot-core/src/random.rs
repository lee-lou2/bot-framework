//! Uniform random bytes and lowercase hex encoding backed by OS CSPRNG (`Uuid::new_v4`).

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Hashes two `Uuid::new_v4()` values (244 bits of entropy) with SHA-256 per 32-byte block so fixed UUID version/variant bits cannot bias the output.
pub fn fill(out: &mut [u8]) {
    for chunk in out.chunks_mut(32) {
        let mut hasher = Sha256::new();
        hasher.update(Uuid::new_v4().as_bytes());
        hasher.update(Uuid::new_v4().as_bytes());
        let digest = hasher.finalize();
        chunk.copy_from_slice(&digest[..chunk.len()]);
    }
}

pub fn bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    fill(&mut out);
    out
}

pub fn hex(bytes: &[u8]) -> String {
    const LUT: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(LUT[(b >> 4) as usize] as char);
        out.push(LUT[(b & 0x0f) as usize] as char);
    }
    out
}

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
        // RFC 4122 UUID v4 fixes byte 6's upper nibble to 0x4; SHA-256 whitening removes that bias.
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
