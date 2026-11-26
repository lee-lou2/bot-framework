//! 이슈 트래커 설정(`admin.issues`)의 지문. 호스트 활성화 지문에 섞어, 이슈 보고를 거두거나
//! 목적지·자격·환경·표본을 바꾸는 것을 활성화 사건으로 만든다. 설정이 없으면 `None`이라 기존 지문은 바뀌지 않는다.

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
