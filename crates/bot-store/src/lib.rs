//! PG 저장소: 접수·큐·리스·세대 펜싱·재시도·dead letter·감사 체인. 예약·위임·outbox·호출 원장·기억이 이어진다.
//! 예정 수락 시험: accepts_duplicate_request_once, protects_owner_memory.
//!
//! 규칙: ① 접수 성공은 커밋 성공이다 ② 시각은 DB 시계만 쓴다 ③ 소유자의 모든 쓰기는 같은 트랜잭션에서 세대·리스를 대조한다
//! ④ 긴 외부 호출 동안 잠금을 쥐지 않는다 ⑤ 상태를 바꾸는 트랜잭션은 마지막에 감사 체인에 한 줄을 더한다.
//! 잠금 순서: 부모 run → 자식 run → 감사 head. 어기면 교착이 생긴다.
//! 다른 크레이트는 `bot_core::port`의 트레이트로만 이 저장소를 쓴다(`ports.rs`).

mod accept;
mod activation_guard;
mod active;
mod admin;
mod admin_act;
mod admin_approvals;
mod admin_read;
mod api;
mod approval_open;
mod approvals;
mod audit;
mod calls;
mod cards;
mod codec;
mod cols;
mod db;
mod delegate;
mod delegate_result;
mod delivery;
mod effect_lookup;
mod effects;
mod finish;
mod handover;
mod memory;
mod outbox;
mod ports;
mod private;
mod profile_bundles;
mod queue;
mod replies;
mod schedule;
mod share;
mod shutdown;
mod unsent;
mod user_due;
mod user_notice;
mod user_schedule;
mod user_wake;

pub use db::{Config, Fail, Store};
pub use memory::{SeedError, check_seed};
