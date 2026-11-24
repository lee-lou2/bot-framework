//! 검증 결과 항목. 코드는 시험·도구가 읽는 안정된 이름, 설명은 사람이 읽는 한국어다.
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
    /// 프로필 폴더 기준 상대 경로. 여러 프로필을 한꺼번에 볼 때는 `프로필/경로`.
    pub path: String,
    /// 파일 안 위치(JSON 포인터 `/bindings/0` 또는 `줄 3`). 없으면 빈 문자열.
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

    /// 경로 앞에 프로필 이름을 붙인다.
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
            Severity::Error => "오류",
            Severity::Warning => "경고",
        };
        let place = if self.at.is_empty() {
            self.path.clone()
        } else {
            format!("{} {}", self.path, self.at)
        };
        write!(f, "{level} {place}: {} [{}]", self.message, self.code)
    }
}

/// 항목 수집기. 보관은 `cap`까지만 하지만 오류 개수는 한도와 무관하게 센다 — 보고 한도가 0이어도
/// 검증 실패가 "오류 없음"으로 바뀌지 않는다. 가득 찬 뒤 들어온 오류는 마지막 경고 자리를 차지한다.
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

    /// 보관하지 못한 오류까지 센 전체 개수
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

    /// 다른 수집기의 항목을 프로필 이름을 붙여 합친다. 보관하지 못한 오류 개수도 함께 옮긴다.
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
        assert_eq!(issues.error_count(), 3, "보관하지 못해도 센다");
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
