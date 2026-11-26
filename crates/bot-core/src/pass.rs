//! 넘김(`bot/pass`): 직접 도구가 처리하지 않은 이벤트를 다음 후보로 넘기는 사슬과 커서.
//! 접수가 후보 목록(order·매치 순)과 기본 대화를 정해 한 접수 행에 남기고, 커서는 PASS와 같은 커밋에서만 앞으로 간다.
use crate::{
    command::Target,
    id::{CallId, RunId},
};

/// 후보 하나: 트리거 ID와 접수 때 계산한 실행 대상(고정 판본의 직접 도구 호출)
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub trigger: String,
    pub target: Target,
}

/// 접수가 남기는 사슬. `candidates[0]`은 접수 명령의 `target`과 같다. `default`는 모든 후보가 넘겼을 때 정확히 한 번 부르는 기본 대화다
/// (없으면 — 예: 반응 — 조용히 끝난다).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PassChain {
    pub candidates: Vec<Candidate>,
    pub default: Option<Target>,
}

/// 후보 위치 `pos`의 직접 호출 ID. 첫 후보는 사슬이 없는 접수와 같은 ID라, 후보마다 호출이 구분되고 한 후보는 한 번만 방문한다.
pub fn direct_call_id(run: &RunId, pos: u32) -> CallId {
    if pos == 0 {
        CallId(format!("{}:tool", run.0))
    } else {
        CallId(format!("{}:tool:{pos}", run.0))
    }
}

/// `CallLedger::finish_pass`의 결과
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PassNext {
    /// 다음 후보로 넘겼다(위치). run은 다시 대기열로 돌아갔다.
    Candidate(u32),
    /// 모든 후보가 넘겼다: run은 기본 대화를 부르도록 다시 대기열로 돌아갔다.
    Default,
    /// 모든 후보가 넘겼고 기본 대화도 없다: run은 조용히 끝났다.
    End,
    /// 넘길 수 없다(고정 사유). 아무것도 쓰지 않았다.
    Refused(&'static str),
}

/// 넘길 수 없는 사유(고정 어휘)
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
