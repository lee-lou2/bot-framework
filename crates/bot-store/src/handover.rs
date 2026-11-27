//! Attempt-accounting rule for lease-expiry handovers across host active-set replacements.

/// Returns whether reclaiming an expired lease spends an attempt.
/// Forgives the attempt only when both the previous and current pinned active sets are known and differ.
pub(crate) fn spends_attempt(previous: Option<&str>, current: Option<&str>) -> bool {
    !matches!((previous, current), (Some(previous), Some(current)) if previous != current)
}

#[cfg(test)]
mod tests {
    use super::spends_attempt;

    #[test]
    fn only_two_known_and_different_sets_forgive_the_attempt() {
        assert!(!spends_attempt(Some("set-1"), Some("set-2")));
        assert!(spends_attempt(Some("set-1"), Some("set-1")));
        assert!(spends_attempt(None, Some("set-2")), "legacy row");
        assert!(spends_attempt(Some("set-1"), None), "unpinned handle");
        assert!(spends_attempt(None, None));
    }
}
