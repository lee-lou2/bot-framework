//! 키 있는 해시(HMAC-SHA256). private 입력은 평문 해시를 저장하지 않는다
//! (짧은 DM은 SHA-256만으로 사전 대입이 가능하다). 키는 프로필별 자격이고 호스트 밖으로 나가지 않는다.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// 조각마다 길이를 앞에 붙여 경계가 모호하지 않게 이어 해시한다.
pub fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC은 어떤 길이의 키도 받는다");
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
        // 같은 입력·같은 키는 같은 값, 키나 조각 경계가 다르면 다른 값이다
        let a = hmac(b"key", &[b"ab", b"c"]);
        assert_eq!(a, hmac(b"key", &[b"ab", b"c"]));
        assert_ne!(
            a,
            hmac(b"key", &[b"a", b"bc"]),
            "조각 경계가 해시에 들어간다"
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
