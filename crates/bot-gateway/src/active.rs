//! Active profile set check against the store-published active revision.
pub use bot_core::active::{ActiveCheck, ActiveSet};

/// Result of the pre-response authorization and active-set check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Release {
    Allowed,
    /// Membership was revoked or entrance ownership changed; do not reveal request existence.
    Forbidden,
    /// Stale process or check failed; fail closed as temporarily unavailable.
    Unavailable,
}
