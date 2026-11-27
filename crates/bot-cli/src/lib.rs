//! Library modules for the `bot` binary: settings, host config, secret stores, commands, and role wiring.

pub mod chat;
pub mod cli;
pub mod doctor;
pub(crate) mod embeddings;
pub(crate) mod execution;
pub mod host;
pub(crate) mod install;
pub(crate) mod load;
pub(crate) mod lock;
pub mod logging;
pub mod map;
pub mod mcp_sync;
pub(crate) mod memory_judge;
pub(crate) mod nonce;
pub mod profile_bundle;
pub mod scaffold;
pub mod secrets;
pub mod serve;
pub mod settings;
pub(crate) mod term;
pub(crate) mod tool_limits;
pub mod validate;
