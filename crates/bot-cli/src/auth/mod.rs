//! Host-managed OAuth, Google service-account, and subscription credential engine and `bot auth` CLI.

pub mod cli;
pub mod crypto;
pub mod engine;
pub mod http;
pub mod import;
mod login;
pub mod oauth;
pub mod payload;
mod refresh;
pub mod service_account;

pub use engine::HostAuthResolver;

use crate::host::HostFile;

/// Host secret namespace storing the 32-byte (64-hex) AEAD key for `bot_credentials`.
pub const CREDENTIAL_KEY_PROFILE: &str = "host";
pub const CREDENTIAL_KEY_NAME: &str = bot_broker::private::CREDENTIAL_KEY_SECRET;

/// Returns true when any configured provider or remote MCP server references a host-managed credential.
pub fn host_needs_credential_key(host: &HostFile) -> bool {
    host.providers
        .values()
        .any(|provider| provider.auth().is_some())
        || host
            .remote_servers
            .values()
            .any(|remote| remote.oauth.is_some())
}
