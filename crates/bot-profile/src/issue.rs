//! Structured validation diagnostics with stable error codes and bounded collection.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Issue {
    pub severity: Severity,
    pub code: &'static str,
    /// Path relative to the profile directory, or `<profile>/<path>` when aggregated.
    pub path: String,
    /// Location within the file (JSON pointer like `/bindings/0` or `line 3`), or empty.
    pub at: String,
    pub message: String,
}

impl Issue {
    pub fn error(code: &'static str, path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(Severity::Error, code, path, message)
    }

    pub fn warning(
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(Severity::Warning, code, path, message)
    }

    fn new(
        severity: Severity,
        code: &'static str,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            code,
            path: path.into(),
            at: String::new(),
            message: message.into(),
        }
    }

    pub fn at(mut self, at: impl Into<String>) -> Self {
        self.at = at.into();
        self
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }

    pub fn in_profile(mut self, profile: &str) -> Self {
        self.path = if self.path.is_empty() {
            profile.to_owned()
        } else {
            format!("{profile}/{}", self.path)
        };
        self
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let place = if self.at.is_empty() {
            self.path.clone()
        } else {
            format!("{} {}", self.path, self.at)
        };
        write!(f, "{level} {place}: {} [{}]", self.message, self.code)
    }
}

/// Collects issues up to `cap` while tracking the true error count regardless of `cap`;
/// once full, new errors displace the last stored warning.
#[derive(Debug)]
pub struct Issues {
    items: Vec<Issue>,
    cap: usize,
    truncated: bool,
    errors: usize,
}

impl Issues {
    pub fn new(cap: usize) -> Self {
        Self {
            items: Vec::new(),
            cap,
            truncated: false,
            errors: 0,
        }
    }

    pub fn push(&mut self, issue: Issue) {
        let is_error = issue.is_error();
        if is_error {
            self.errors += 1;
        }
        if self.items.len() < self.cap {
            self.items.push(issue);
            return;
        }
        self.truncated = true;
        if is_error && let Some(slot) = self.items.iter().rposition(|i| !i.is_error()) {
            self.items[slot] = issue;
        }
    }

    pub fn error(&mut self, code: &'static str, path: &str, message: impl Into<String>) {
        self.push(Issue::error(code, path, message));
    }

    pub fn error_at(
        &mut self,
        code: &'static str,
        path: &str,
        at: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.push(Issue::error(code, path, message).at(at));
    }

    pub fn warning(&mut self, code: &'static str, path: &str, message: impl Into<String>) {
        self.push(Issue::warning(code, path, message));
    }

    pub fn has_errors(&self) -> bool {
        self.errors > 0
    }

    /// Total error count including errors dropped due to `cap`.
    pub fn error_count(&self) -> usize {
        self.errors
    }

    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub fn items(&self) -> &[Issue] {
        &self.items
    }

    pub fn extend(&mut self, other: impl IntoIterator<Item = Issue>) {
        for issue in other {
            self.push(issue);
        }
    }

    /// Merges issues prefixed with `profile/`, carrying over dropped error counts.
    pub fn absorb(&mut self, other: Issues, profile: &str) {
        let stored = other.items.iter().filter(|i| i.is_error()).count();
        let dropped = other.errors - stored;
        self.truncated |= other.truncated;
        for issue in other.items {
            self.push(issue.in_profile(profile));
        }
        self.errors += dropped;
    }

    pub fn into_vec(self) -> Vec<Issue> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_counted_even_when_nothing_is_stored() {
        let mut issues = Issues::new(0);
        issues.error("a", "p", "x");
        issues.warning("b", "p", "y");
        assert!(issues.has_errors() && issues.error_count() == 1 && issues.truncated());
        assert!(issues.items().is_empty());
    }

    #[test]
    fn a_late_error_replaces_a_stored_warning() {
        let mut issues = Issues::new(2);
        issues.warning("w1", "p", "x");
        issues.warning("w2", "p", "x");
        issues.error("e1", "p", "x");
        let codes: Vec<_> = issues.items().iter().map(|i| i.code).collect();
        assert_eq!(codes, ["w1", "e1"]);
        issues.error("e2", "p", "x");
        issues.error("e3", "p", "x");
        assert_eq!(issues.error_count(), 3, "errors counted even when dropped");
        assert_eq!(issues.items().len(), 2);
    }

    #[test]
    fn absorb_carries_dropped_error_counts() {
        let mut local = Issues::new(0);
        local.error("a", "x.md", "m");
        local.error("b", "x.md", "m");
        let mut all = Issues::new(5);
        all.absorb(local, "personal");
        assert!(all.has_errors() && all.error_count() == 2 && all.truncated());
        let mut kept = Issues::new(5);
        kept.error("c", "x.md", "m");
        all.absorb(kept, "personal");
        assert_eq!(
            (all.error_count(), all.items()[0].path.as_str()),
            (3, "personal/x.md")
        );
    }
}
