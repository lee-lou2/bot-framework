//! Direct HTTP API `Step` model runner for Anthropic Messages and OpenAI-compatible Chat Completions.
//!
//! Security boundary: no VM, host filesystem, or shell exists on this path. The runner sends
//! assembled messages only to configured `ApiEndpoint`s (`https` or loopback IP `http`), checks
//! `ActiveCheck` before and after reading the provider key, fails closed with fixed vocabulary
//! codes if the key appears anywhere in the response, and never executes tools directly.

mod endpoint;
mod names;
mod runner;
mod scan;
mod transport;
mod wire;

pub use endpoint::ApiEndpoint;
pub use runner::{ApiRunner, ApiSettings, Effort, LimitField};
pub use transport::ApiClient;
pub use wire::ApiKind;

#[cfg(test)]
mod tests;
