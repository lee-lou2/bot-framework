//! Random hex nonces used to keep temporary and backup filenames collision-free.

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
