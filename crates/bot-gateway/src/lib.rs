//! Authenticated ingress, private DM routing, and reply delivery across CLI, Slack, and HTTP adapters.
//!
//! Every adapter follows the same ingress sequence: `verify → normalize → durable accept → ack`.
pub mod active;
pub mod cli;
pub mod delivery;
pub mod entrance;
pub mod host;
pub mod http;
pub mod identity;
pub mod inbound;
pub mod ingress;
mod limits;
pub mod mapping;
pub mod observe;
pub mod reply;
pub mod routes;
pub mod schedule_routes;
pub mod slack;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod trigger;

pub use limits::Limits;
