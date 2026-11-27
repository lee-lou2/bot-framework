//! Authority fingerprint contribution for `admin.issues` (`None` when disabled to preserve existing hashes).

use super::IssueConfig;
use sha2::{Digest, Sha256};

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn opt_field(hash: &mut Sha256, value: Option<&str>) {
    match value {
        None => hash.update([0]),
        Some(text) => {
            hash.update([1]);
            field(hash, text.as_bytes());
        }
    }
}

pub fn fingerprint(config: &Option<IssueConfig>) -> Option<[u8; 32]> {
    let cfg = config.as_ref()?;
    let mut hash = Sha256::new();
    hash.update(b"bot-issues-authority-v1\0");
    field(&mut hash, cfg.kind.as_str().as_bytes());
    field(&mut hash, cfg.endpoint.as_bytes());
    match &cfg.key {
        None => hash.update([0]),
        Some(secret) => {
            hash.update([1]);
            field(&mut hash, secret.profile.0.as_bytes());
            field(&mut hash, secret.name.as_str().as_bytes());
        }
    }
    opt_field(&mut hash, cfg.environment.as_deref());
    opt_field(&mut hash, cfg.release.as_deref());
    hash.update((cfg.sample + 0.0).to_bits().to_be_bytes());
    Some(hash.finalize().into())
}
