//! 복구 판정: 시작한 호출의 결과를 잃은 뒤 같은 호출 ID를 다시 실행해도 되는가.
//!
//! 쓰기를 다시 실행해도 되는 경우는 둘뿐이다. 이전 프로세스(자식 포함)가 끝난 것을 확인했고,
//! 실행된 단계가 없으며 미실행이 증명됐거나, 외부 효과가 없는 읽기 도구일 때다.
//! 증거가 없으면 보류하고 자동 재실행은 0회다. 시간이 지났다고 보류를 풀지 않는다.

use bot_core::{ledger::Resolution, state::Risk};

#[derive(Clone, Debug)]
pub struct Facts {
    pub class: Risk,
    /// 도구가 단계 원장(`effects: true`)을 쓰는가.
    pub effects: bool,
    /// 옛 프로세스와 그 자식이 모두 끝난 것을 감독자가 확인했는가.
    pub reaped: bool,
    /// 단계 원장의 미결 단계 수. 조회하지 못했으면 `None`.
    pub unresolved: Option<usize>,
    /// 시작 뒤 수신처 영수증을 기록한 단계가 있는가.
    pub confirmed_step: bool,
    /// 관리자가 기록한 판정.
    pub resolution: Option<Resolution>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Plan {
    /// 같은 호출 ID로 현재 권한·승인을 거쳐 다시 실행한다.
    Resume,
    /// 수신처 영수증이 확인됐다. 실행하지 않고 그 영수증으로 완료한다.
    Complete(String),
    /// 보류를 유지한다(이유 코드).
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
