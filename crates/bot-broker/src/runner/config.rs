//! Tool runner configuration and per-call timeout budgets.
//!
//! Calls whose effective timeout exceeds `max_call_timeout` (`limits.tool_call_max_secs`) are
//! refused before dispatch (`Ran::Refused`). Sandbox `wall` and proxy tunnel limits stretch only
//! when a tool declares a timeout longer than the defaults.
use crate::{egress::EgressPolicy, proxy::ProxyConfig};
use bot_core::profile::Tool;
use std::time::Duration;

pub const TIMEOUT_OVER_CAP: &str = "tool_timeout_over_cap";

#[derive(Clone, Copy, Debug)]
pub struct RunnerConfig {
    /// Per-step limit for `initialize` and `tools/list`.
    pub handshake_timeout: Duration,
    /// Default call timeout when `tool.timeout_secs` is unset.
    pub call_timeout: Duration,
    /// Host ceiling (`limits.tool_call_max_secs`); effective call timeouts above this are refused.
    pub max_call_timeout: Duration,
    pub terminate_timeout: Duration,
    pub max_result_bytes: usize,
    /// Explicit operator opt-in for private-network egress.
    pub allow_private_egress: bool,
    /// Total sandbox wall-clock limit for default-timeout calls; extended by `call - call_timeout`.
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Budget {
    pub call: Duration,
    /// Monotonic end-to-end deadline covering startup, handshake, call, and teardown.
    pub wall: Duration,
}

impl RunnerConfig {
    /// Computes the per-call budget, refusing effective timeouts above `max_call_timeout`.
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
                "undeclared tool must keep 60s default (ceiling {ceiling})"
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
        assert_eq!(
            at(10),
            Budget {
                call: secs(10),
                wall: secs(300)
            }
        );
        for declared in [1, 60, 61, 300, 900] {
            let budget = at(declared);
            assert!(budget.wall >= budget.call + secs(240), "{declared}");
        }
    }

    #[test]
    fn a_limit_above_the_ceiling_is_refused_before_anything_is_sent_declared_or_not() {
        assert_eq!(
            cfg(300).budget(&tool(Some(301))),
            Err(TIMEOUT_OVER_CAP),
            "declared > ceiling"
        );
        assert!(
            cfg(300).budget(&tool(Some(300))).is_ok(),
            "declared == ceiling allowed"
        );
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
        for declared in [None, Some(30), Some(600)] {
            assert_eq!(proxy(config, declared).tunnel_limit, base, "{declared:?}");
        }
        assert_eq!(
            proxy(config, Some(601)).tunnel_limit,
            Duration::from_secs(601)
        );
        assert_eq!(
            proxy(config, Some(900)).tunnel_limit,
            Duration::from_secs(900)
        );
        assert!(!proxy(config, None).allow_private);
        let open = RunnerConfig {
            allow_private_egress: true,
            ..config
        };
        assert!(proxy(open, None).allow_private);
    }
}
