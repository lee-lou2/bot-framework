//! Pass chain (`bot/pass`) types and cursor outcomes for routing unhandled direct-tool events.

use crate::{
    command::Target,
    id::{CallId, RunId},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub trigger: String,
    pub target: Target,
}

/// Ordered pass chain persisted at intake; `candidates[0]` matches the intake target, and `default` runs at most once if every candidate passes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassChain {
    pub candidates: Vec<Candidate>,
    pub default: Option<Target>,
}

/// Call ID for candidate index `pos`; index 0 matches non-chain intakes so each candidate executes at most once.
pub fn direct_call_id(run: &RunId, pos: u32) -> CallId {
    if pos == 0 {
        CallId(format!("{}:tool", run.0))
    } else {
        CallId(format!("{}:tool:{pos}", run.0))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PassNext {
    /// Advanced to candidate index; run re-queued.
    Candidate(u32),
    /// All candidates passed; run re-queued for default conversation.
    Default,
    /// All candidates passed with no default; run ended silently.
    End,
    /// Refused with a fixed reason code; nothing written.
    Refused(&'static str),
}

pub const PASS_AFTER_EFFECT: &str = "pass_after_effect";
pub const INVALID_PASS: &str = "invalid_pass";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_candidate_keeps_the_plain_call_id_and_each_position_is_distinct() {
        let run = RunId("r1".into());
        assert_eq!(direct_call_id(&run, 0).0, "r1:tool");
        assert_eq!(direct_call_id(&run, 1).0, "r1:tool:1");
        assert_eq!(direct_call_id(&run, 2).0, "r1:tool:2");
        assert_ne!(direct_call_id(&run, 1), direct_call_id(&run, 11));
    }
}
