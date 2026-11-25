//! 실행기 설정과 호출마다의 시간 예산.
//!
//! 호출 시간 한도는 도구마다 정한다: 선언하지 않은 도구는 `call_timeout`(기본 60초), 선언한 도구(`timeout_secs`)는 그 값이다. 실효 한도가 호스트 천장(`max_call_timeout`,
//! `limits.tool_call_max_secs`)을 넘으면 — 활성화 뒤에 천장을 내렸거나 복원한 옛 판본의 선언이다 — **보내기 전에** 거절한다(`Ran::Refused`). 샌드박스 전체 수명(`wall`)과
//! 단일 외부 연결의 상한(프록시 터널)은 기본 한도보다 긴 호출에서만 그만큼 늘어난다: 한도를 선언하지 않은 도구의 기한은 그대로다.
use crate::{egress::EgressPolicy, proxy::ProxyConfig};
use bot_core::profile::Tool;
use std::time::Duration;

/// 실효 호출 시간 한도가 호스트 천장을 넘어 보내지 않은 호출의 고정 사유.
pub const TIMEOUT_OVER_CAP: &str = "tool_timeout_over_cap";

#[derive(Clone, Copy, Debug)]
pub struct RunnerConfig {
    /// `initialize`와 `tools/list` 각각의 제한.
    pub handshake_timeout: Duration,
    /// `timeout_secs`를 선언하지 않은 도구의 호출 시간 한도.
    pub call_timeout: Duration,
    /// 호출 시간 한도의 천장(호스트 `limits.tool_call_max_secs`). 실효 한도가 이를 넘는 호출은 보내지 않는다.
    pub max_call_timeout: Duration,
    pub terminate_timeout: Duration,
    pub max_result_bytes: usize,
    /// 운영자가 사설망 접속을 명시적으로 열었을 때만 true.
    pub allow_private_egress: bool,
    /// 샌드박스 전체 수명 상한(호출 한도가 `call_timeout`일 때). 호출 한도가 더 길면 그만큼 늘어난다.
    pub wall: Duration,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(10),
            call_timeout: Duration::from_secs(60),
            max_call_timeout: Duration::from_secs(60),
            terminate_timeout: Duration::from_secs(10),
            max_result_bytes: 256 * 1024,
            allow_private_egress: false,
            wall: Duration::from_secs(300),
        }
    }
}

/// 한 호출의 시간 예산. 로컬·원격 실행이 같은 값을 쓴다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Budget {
    /// `tools/call`을 보낸 뒤 결과를 기다리는 시간
    pub call: Duration,
    /// 시작(기동·협상 포함)부터 정산까지의 절대 기한. 단조 시계 하나로 잰다.
    pub wall: Duration,
}

impl RunnerConfig {
    /// 이 도구를 이번에 부를 때의 시간 예산. 실효 한도(선언 또는 `call_timeout`)가 천장을 넘으면 `Err`다.
    /// `wall`은 기본 한도보다 긴 만큼만 늘린다(기동·양쪽 협상·정리 여유는 기존 `wall`이 품고 있다): `max(wall, call + (wall − call_timeout))`와 같다.
    pub(super) fn budget(&self, tool: &Tool) -> Result<Budget, &'static str> {
        let call = tool.timeout_secs.map_or(self.call_timeout, |secs| {
            Duration::from_secs(u64::from(secs))
        });
        if call > self.max_call_timeout {
            return Err(TIMEOUT_OVER_CAP);
        }
        Ok(Budget {
            call,
            wall: self.wall + call.saturating_sub(self.call_timeout),
        })
    }

    /// 이 호출의 egress 프록시 설정. 터널 하나의 상한은 호출 한도가 기본 상한(600초)보다 긴 도구만 호출 한도에 맞추고, 짧은 도구의 기존 네트워크 제한은 그대로 둔다.
    pub(super) fn proxy(
        &self,
        policy: EgressPolicy,
        password: String,
        budget: Budget,
    ) -> ProxyConfig {
        let mut proxy = ProxyConfig::new(policy, "bot".into(), password);
        proxy.allow_private = self.allow_private_egress;
        proxy.tunnel_limit = proxy.tunnel_limit.max(budget.call);
        proxy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(secs: Option<u32>) -> Tool {
        let mut tool = crate::testing::tool(bot_core::state::Risk::Read, &["calendar.read"]);
        tool.timeout_secs = secs;
        tool
    }

    fn cfg(ceiling_secs: u64) -> RunnerConfig {
        RunnerConfig {
            max_call_timeout: Duration::from_secs(ceiling_secs),
            ..RunnerConfig::default()
        }
    }

    #[test]
    fn an_undeclared_tool_keeps_the_default_limit_and_wall_whatever_the_ceiling_is() {
        for ceiling in [60, 300, 900] {
            let budget = cfg(ceiling).budget(&tool(None)).unwrap();
            assert_eq!(
                budget,
                Budget {
                    call: Duration::from_secs(60),
                    wall: Duration::from_secs(300)
                },
                "천장만 올려도 선언하지 않은 도구는 60초다 (천장 {ceiling})"
            );
        }
    }

    #[test]
    fn a_declared_limit_sets_the_call_limit_and_stretches_only_the_wall_beyond_the_default() {
        let config = cfg(900);
        let secs = |n: u64| Duration::from_secs(n);
        let at = |declared| config.budget(&tool(Some(declared))).unwrap();
        assert_eq!(
            at(900),
            Budget {
                call: secs(900),
                wall: secs(1140)
            }
        );
        assert_eq!(
            at(600),
            Budget {
                call: secs(600),
                wall: secs(840)
            }
        );
        // 기본보다 짧은 선언은 호출만 줄이고 기존 wall은 줄이지 않는다
        assert_eq!(
            at(10),
            Budget {
                call: secs(10),
                wall: secs(300)
            }
        );
        // 호출을 보낸 뒤 기다릴 수 있는 시간은 늘 wall 안이다(기동·협상·정리 여유가 남는다)
        for declared in [1, 60, 61, 300, 900] {
            let budget = at(declared);
            assert!(budget.wall >= budget.call + secs(240), "{declared}");
        }
    }

    #[test]
    fn a_limit_above_the_ceiling_is_refused_before_anything_is_sent_declared_or_not() {
        // 선언이 천장을 넘는다(천장을 내렸거나 복원한 옛 선언)
        assert_eq!(
            cfg(300).budget(&tool(Some(301))),
            Err(TIMEOUT_OVER_CAP),
            "선언 > 천장"
        );
        assert!(
            cfg(300).budget(&tool(Some(300))).is_ok(),
            "천장과 같으면 허용"
        );
        // 선언이 없어도 기본 60초가 천장을 넘으면(천장 < 60) 거절한다
        assert_eq!(cfg(30).budget(&tool(None)), Err(TIMEOUT_OVER_CAP));
        assert!(cfg(30).budget(&tool(Some(30))).is_ok());
    }

    #[test]
    fn only_a_call_longer_than_the_default_tunnel_limit_stretches_the_tunnel() {
        let proxy = |config: RunnerConfig, declared: Option<u32>| {
            let budget = config.budget(&tool(declared)).unwrap();
            config.proxy(EgressPolicy::new(&[], None), "pw".into(), budget)
        };
        let config = cfg(900);
        let base =
            ProxyConfig::new(EgressPolicy::new(&[], None), "bot".into(), "pw".into()).tunnel_limit;
        assert_eq!(base, Duration::from_secs(600));
        // 선언하지 않은 도구·600초 이하를 선언한 도구의 터널 제한은 기존 그대로다
        for declared in [None, Some(30), Some(600)] {
            assert_eq!(proxy(config, declared).tunnel_limit, base, "{declared:?}");
        }
        // 600초를 넘는 한도를 선언한 도구만 호출 한도에 맞춘다
        assert_eq!(
            proxy(config, Some(601)).tunnel_limit,
            Duration::from_secs(601)
        );
        assert_eq!(
            proxy(config, Some(900)).tunnel_limit,
            Duration::from_secs(900)
        );
        // 사설망 허용은 운영자 설정을 그대로 따른다
        assert!(!proxy(config, None).allow_private);
        let open = RunnerConfig {
            allow_private_egress: true,
            ..config
        };
        assert!(proxy(open, None).allow_private);
    }
}
