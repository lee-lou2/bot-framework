//! Non-canary integration test suite for `bot-runtime`.

mod support;

#[path = "suite/api_flows.rs"]
mod api_flows;
#[path = "suite/api_runner.rs"]
mod api_runner;
#[path = "suite/api_runner_keys.rs"]
mod api_runner_keys;
#[path = "suite/flows.rs"]
mod flows;
#[path = "suite/schedule.rs"]
mod schedule;
#[path = "suite/shutdown_release.rs"]
mod shutdown_release;
#[path = "suite/stale_handback.rs"]
mod stale_handback;
#[path = "suite/user_destination.rs"]
mod user_destination;
#[path = "suite/user_fairness.rs"]
mod user_fairness;
#[path = "suite/user_invalid_timing.rs"]
mod user_invalid_timing;
