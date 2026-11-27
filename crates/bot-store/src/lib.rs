//! PostgreSQL store for durable intake, run queue, leases, generation fencing, retries, outbox, ledger, memory, and audit chain.
//!
//! Invariants:
//! 1. Intake succeeds only when the transaction commits.
//! 2. All timestamps come solely from the DB clock (`bot_now_ms()`).
//! 3. Every owner mutation verifies generation and lease in the same transaction.
//! 4. Never hold DB locks across external I/O.
//! 5. State-changing transactions append to the audit chain last.
//!
//! Lock order: parent run -> child run -> audit head (violating this causes deadlocks).
//! Other crates access `Store` solely through `bot_core::port` traits (`ports.rs`).

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
mod authority;
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
mod evidence;
mod finish;
mod grants;
mod handover;
mod memory;
mod outbox;
mod pass;
mod ports;
mod private;
mod profile_bundles;
mod queue;
mod replies;
mod schedule;
mod share;
mod shutdown;
mod slack_events;
mod threads;
mod unsent;
mod user_due;
mod user_notice;
mod user_schedule;
mod user_wake;

pub use db::{Config, Fail, Store};
pub use memory::{SeedError, check_seed};
