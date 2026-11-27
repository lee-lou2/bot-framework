//! Re-exports canonical name and principal identity rules from `bot_core`.

pub use bot_core::id::is_name;

pub use bot_core::identity::{IdentityKind, identity_kind};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_shapes() {
        assert!(is_name("main") && is_name("a-1") && is_name(&"a".repeat(63)));
        assert!(!is_name("") && !is_name("A") && !is_name("1a") && !is_name(&"a".repeat(64)));
        assert!(
            !is_name("a_b")
                && !is_name("a.b")
                && !is_name("a--b")
                && !is_name("a-")
                && !is_name("-a")
        );
        assert_eq!(identity_kind("local:owner"), Some(IdentityKind::Local));
        assert_eq!(
            identity_kind("service:morning"),
            Some(IdentityKind::Service)
        );
        assert_eq!(identity_kind("owner"), None);
        assert_eq!(identity_kind("local: x"), None);
        assert_eq!(identity_kind("Local:x"), None);
    }
}
