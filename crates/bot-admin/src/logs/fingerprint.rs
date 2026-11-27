//! Authority fingerprint contribution for `admin.logs` (`None` when disabled to preserve existing hashes).

use super::LogConfig;
use crate::obs::HostHeaderValue;
use sha2::{Digest, Sha256};

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

pub fn fingerprint(config: &Option<LogConfig>) -> Option<[u8; 32]> {
    let cfg = config.as_ref()?;
    let mut hash = Sha256::new();
    hash.update(b"bot-logs-authority-v1\0");
    field(&mut hash, cfg.endpoint.as_bytes());
    field(&mut hash, cfg.level.as_str().as_bytes());
    hash.update((cfg.headers.len() as u64).to_be_bytes());
    for (name, value) in &cfg.headers {
        field(&mut hash, name.as_bytes());
        match value {
            HostHeaderValue::Plain(text) => {
                hash.update([0]);
                field(&mut hash, &Sha256::digest(text.as_bytes()));
            }
            HostHeaderValue::Secret(secret) => {
                hash.update([1]);
                field(&mut hash, secret.profile.0.as_bytes());
                field(&mut hash, secret.name.as_str().as_bytes());
            }
        }
    }
    Some(hash.finalize().into())
}
