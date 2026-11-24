//! 역할 사이의 데이터와 포트 계약. 구현은 각 담당 크레이트가 제공한다.
//! 예정 수락 시험: preserves_run_identity, prevents_cross_profile_lookup.
pub mod admin;
pub mod canon;
pub mod cards;
pub mod cli;
pub mod command;
pub mod cron;
pub mod id;
pub mod identity;
pub mod install;
pub mod intake;
pub mod json;
pub mod ledger;
pub mod port;
pub mod profile;
pub mod sandbox;
pub mod schedule;
pub mod scope;
pub mod state;
mod tool_env;
pub mod trace;
