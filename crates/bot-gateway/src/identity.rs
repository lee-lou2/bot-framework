//! Validated principal ID builders derived solely from adapter-authenticated facts.
//!
//! Formats: `local:<account>`, `slack:<workspace>/<user_id>`, and `http:<endpoint>/<sub>`.
//! Rejects delimiter injection, whitespace, and control characters.
use bot_core::id::PrincipalId;

pub use bot_core::id::is_name as is_alias;

fn validated(identity: String) -> Option<PrincipalId> {
    bot_core::identity::identity_kind(&identity).map(|_| PrincipalId(identity))
}

pub fn local(account: &str) -> Option<PrincipalId> {
    validated(format!("local:{account}"))
}

pub fn slack(workspace: &str, user: &str) -> Option<PrincipalId> {
    validated(format!("slack:{workspace}/{user}"))
}

pub fn http(endpoint: &str, subject: &str) -> Option<PrincipalId> {
    validated(format!("http:{endpoint}/{subject}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_the_documented_shapes() {
        assert_eq!(local("owner").unwrap().0, "local:owner");
        assert_eq!(slack("home", "U0123ABC").unwrap().0, "slack:home/U0123ABC");
        assert_eq!(
            http("reminders", "ana@example.org").unwrap().0,
            "http:reminders/ana@example.org"
        );
    }

    #[test]
    fn refuses_delimiter_injection() {
        assert!(local("owner:admin").is_none());
        assert!(local("Owner").is_none());
        assert!(slack("home", "U1/other").is_none());
        assert!(slack("home/x", "U1").is_none());
        assert!(slack("home", "u1").is_none());
        assert!(slack("home", "").is_none());
        assert!(http("reminders", "a/b").is_none());
        assert!(http("reminders", "a:b").is_none());
        assert!(http("reminders", "a b").is_none());
        assert!(http("reminders", "a\nb").is_none());
        assert!(http("reminders", &"a".repeat(129)).is_none());
    }

    #[test]
    fn alias_matches_the_profile_name_pattern() {
        assert!(is_alias("a"));
        assert!(is_alias("home-2"));
        assert!(is_alias(&format!("a{}", "b".repeat(62))));
        assert!(!is_alias(&format!("a{}", "b".repeat(63))));
        assert!(!is_alias("1a"));
        assert!(!is_alias("a_b"));
        assert!(!is_alias(""));
    }
}
