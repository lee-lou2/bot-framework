//! Shared observability export primitives: HTTP/HTTPS transport, OTLP protobuf encoding, runtime controls, and secret refs.

pub(crate) mod proto;
pub(crate) mod runtime;
pub(crate) mod transport;

use bot_core::id::{ProfileId, SecretName};

/// Default SecretStore profile namespace used by host-level observability configs when `profile` is omitted.
pub const DEFAULT_SECRET_PROFILE: &str = "host";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SecretRef {
    pub profile: ProfileId,
    pub name: SecretName,
}

impl SecretRef {
    pub fn host(name: SecretName) -> Self {
        Self {
            profile: ProfileId(DEFAULT_SECRET_PROFILE.to_owned()),
            name,
        }
    }

    pub fn in_profile(profile: ProfileId, name: SecretName) -> Self {
        Self { profile, name }
    }
}

#[derive(Clone, PartialEq)]
pub enum HostHeaderValue {
    Plain(String),
    Secret(SecretRef),
}

/// Redacts plain header values in debug output so accidentally placed tokens never leak into logs.
impl std::fmt::Debug for HostHeaderValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("Plain(..)"),
            Self::Secret(secret) => {
                write!(f, "Secret({}/{})", secret.profile.0, secret.name.as_str())
            }
        }
    }
}

pub use runtime::{Options, Stats};
