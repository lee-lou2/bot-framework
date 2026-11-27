//! Recovery planner deciding whether an interrupted call may resume, complete, or remain held.
//!
//! Never auto-resumes writes without proof that the prior process exited and no unresolved
//! effects exist; elapsed time never clears a hold.

use bot_core::{ledger::Resolution, state::Risk};

#[derive(Clone, Debug)]
pub struct Facts {
    pub class: Risk,
    /// Whether the tool uses the multi-step effect journal (`effects: true`).
    pub effects: bool,
    /// Whether the supervisor confirmed the prior process tree exited.
    pub reaped: bool,
    /// Count of unresolved effect steps (`None` if lookup failed).
    pub unresolved: Option<usize>,
    /// Whether any step already recorded a downstream confirmation receipt.
    pub confirmed_step: bool,
    /// Operator-recorded resolution verdict, if any.
    pub resolution: Option<Resolution>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Plan {
    /// Re-run under the same call ID after re-checking current authority and approval.
    Resume,
    /// Complete without re-running using the confirmed downstream receipt.
    Complete(String),
    /// Keep held with a fixed reason code.
    Hold(&'static str),
}

pub fn plan(facts: &Facts) -> Plan {
    if !facts.reaped {
        return Plan::Hold("previous_process_unconfirmed");
    }
    match &facts.resolution {
        Some(Resolution::Executed { receipt }) => return Plan::Complete(receipt.clone()),
        Some(Resolution::NotExecuted) if !facts.confirmed_step => return Plan::Resume,
        Some(Resolution::NotExecuted) | None => {}
    }
    if facts.confirmed_step {
        return Plan::Hold("response_unknown");
    }
    if facts.class == Risk::Read {
        return Plan::Resume;
    }
    if facts.effects {
        return match facts.unresolved {
            Some(0) => Plan::Resume,
            _ => Plan::Hold("effects_unresolved"),
        };
    }
    Plan::Hold("response_unknown")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(class: Risk, effects: bool) -> Facts {
        Facts {
            class,
            effects,
            reaped: true,
            unresolved: Some(0),
            confirmed_step: false,
            resolution: None,
        }
    }

    #[test]
    fn nothing_reruns_until_the_old_process_is_confirmed_gone() {
        for class in [Risk::Read, Risk::Write, Risk::Irreversible] {
            for resolution in [None, Some(Resolution::NotExecuted)] {
                let f = Facts {
                    reaped: false,
                    resolution,
                    ..facts(class, false)
                };
                assert_eq!(
                    plan(&f),
                    Plan::Hold("previous_process_unconfirmed"),
                    "{class:?}"
                );
            }
        }
        let f = Facts {
            reaped: false,
            resolution: Some(Resolution::Executed {
                receipt: "r".into(),
            }),
            ..facts(Risk::Write, false)
        };
        assert_eq!(plan(&f), Plan::Hold("previous_process_unconfirmed"));
    }

    #[test]
    fn a_single_write_without_evidence_stays_held() {
        for class in [Risk::Write, Risk::Irreversible] {
            assert_eq!(plan(&facts(class, false)), Plan::Hold("response_unknown"));
        }
    }

    #[test]
    fn an_admin_verdict_decides_the_single_write() {
        let f = Facts {
            resolution: Some(Resolution::NotExecuted),
            ..facts(Risk::Irreversible, false)
        };
        assert_eq!(plan(&f), Plan::Resume);
        let f = Facts {
            resolution: Some(Resolution::Executed {
                receipt: "evt-9".into(),
            }),
            ..facts(Risk::Irreversible, false)
        };
        assert_eq!(plan(&f), Plan::Complete("evt-9".into()));
    }

    #[test]
    fn known_execution_needs_the_receipt_for_the_whole_call() {
        let mut f = facts(Risk::Write, true);
        f.confirmed_step = true;
        for resolution in [None, Some(Resolution::NotExecuted)] {
            f.resolution = resolution;
            assert_eq!(plan(&f), Plan::Hold("response_unknown"));
        }
        f.resolution = Some(Resolution::Executed {
            receipt: "all".into(),
        });
        assert_eq!(plan(&f), Plan::Complete("all".into()));
    }

    #[test]
    fn reads_are_safe_to_repeat_once_the_old_process_is_gone() {
        assert_eq!(plan(&facts(Risk::Read, false)), Plan::Resume);
        assert_eq!(plan(&facts(Risk::Read, true)), Plan::Resume);
    }

    #[test]
    fn multi_step_writes_resume_only_when_no_step_is_unresolved() {
        assert_eq!(plan(&facts(Risk::Write, true)), Plan::Resume);
        let pending = Facts {
            unresolved: Some(1),
            ..facts(Risk::Write, true)
        };
        assert_eq!(plan(&pending), Plan::Hold("effects_unresolved"));
        let unknown = Facts {
            unresolved: None,
            ..facts(Risk::Write, true)
        };
        assert_eq!(plan(&unknown), Plan::Hold("effects_unresolved"));
    }
}
