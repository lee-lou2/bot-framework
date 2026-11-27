//! Authority fingerprint contribution for `admin.trace` (`None` when no profile exports to preserve existing hashes).

use super::{HeaderValue, Ids, ProfileTrace};
use bot_core::id::ProfileId;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

/// Excludes backend display names, link templates, and profiles without a backend; hashes plain header values.
pub fn fingerprint(config: &BTreeMap<ProfileId, ProfileTrace>) -> Option<[u8; 32]> {
    let exporting: Vec<_> = config
        .iter()
        .filter_map(|(profile, trace)| Some((profile, trace, trace.backend.as_ref()?)))
        .collect();
    if exporting.is_empty() {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"bot-trace-authority-v1\0");
    hash.update((exporting.len() as u64).to_be_bytes());
    for (profile, trace, backend) in exporting {
        field(&mut hash, profile.0.as_bytes());
        field(&mut hash, backend.endpoint.as_bytes());
        hash.update((backend.headers.len() as u64).to_be_bytes());
        for (name, value) in &backend.headers {
            field(&mut hash, name.as_bytes());
            match value {
                HeaderValue::Plain(text) => {
                    hash.update([0]);
                    field(&mut hash, &Sha256::digest(text.as_bytes()));
                }
                HeaderValue::Secret(secret) => {
                    hash.update([1]);
                    field(&mut hash, secret.as_str().as_bytes());
                }
            }
        }
        hash.update([match trace.ids {
            Ids::Plain => 0,
            Ids::Hash => 1,
        }]);
        hash.update([u8::from(trace.content)]);
        // Canonicalize -0.0 to +0.0.
        hash.update((trace.sample + 0.0).to_bits().to_be_bytes());
    }
    Some(hash.finalize().into())
}
