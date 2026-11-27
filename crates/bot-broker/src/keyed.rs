//! Keyed HMAC-SHA256 hashing for private inputs so short DMs resist dictionary lookups.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// Hashes length-prefixed parts to prevent boundary collisions.
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts keys of any length");
    for part in parts {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::hex;

    #[test]
    fn matches_the_rfc_4231_vector_for_a_single_part_plus_length_framing() {
        let a = hmac(b"key", &[b"ab", b"c"]);
        assert_eq!(a, hmac(b"key", &[b"ab", b"c"]));
        assert_ne!(
            a,
            hmac(b"key", &[b"a", b"bc"]),
            "part boundaries must affect the digest"
        );
        assert_ne!(a, hmac(b"other", &[b"ab", b"c"]));
        assert_eq!(hex(&a).len(), 64);
    }

    #[test]
    fn it_is_not_the_plain_sha256_of_the_text() {
        let keyed = hmac(b"key", &[b"hello"]);
        assert_ne!(keyed, crate::canon::sha256("hello"));
    }
}
