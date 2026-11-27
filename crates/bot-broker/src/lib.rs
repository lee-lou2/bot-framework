//! Tool-call gate: authorization, risk classification, approval, sandboxed execution, and auditing.
//!
//! Pure policy modules (`classify`, `approval`, `recovery`, `egress`, `rate`, `guard`; plus `bot_core::scope`) have
//! no port dependencies; `service` wires them to `ports` to implement `bot_core::port::Broker`.
//! Start new modules as `pub(crate)` and open `pub mod` only when another crate requires it.

pub mod egress;
pub mod front;
pub mod guard;
pub mod mcp;
pub mod model;
pub mod ports;
pub mod preapproval;
pub mod private;
pub mod proxy;
pub mod rate;
pub mod resolve;
pub mod runner;
pub mod sandbox;
pub mod seal;
pub mod service;
#[cfg(unix)]
pub mod srt;
pub mod sys;

pub(crate) mod approval;
pub(crate) mod approver;
pub(crate) use bot_core::canon;
pub(crate) mod classify;
pub(crate) mod effects;
pub(crate) use bot_core::json;
pub(crate) mod keyed;
pub(crate) mod lines;
pub(crate) mod recovery;
pub(crate) mod text;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
