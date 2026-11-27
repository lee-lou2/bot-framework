//! 이름에 붙이는 무작위 조각(임시·보관 파일이 서로, 그리고 운영자가 따로 만든 파일과 겹치지 않게 한다).

/// 운영체제 난수 8바이트의 16진 16자리.
pub fn hex() -> String {
    bot_core::random::random_hex(8)
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
